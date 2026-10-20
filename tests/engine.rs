use std::path::Path;
use std::time::Duration;

use schema::{ChangeSetState, Engine, Error};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    url: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("changelog")).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("test.db").display());
        Self { dir, url }
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.changelog().join(name), content).unwrap();
    }

    fn changelog(&self) -> std::path::PathBuf {
        self.dir.path().join("changelog")
    }

    async fn engine(&self) -> Engine {
        Engine::connect(&self.url)
            .await
            .unwrap()
            .changelog(self.changelog())
            .lock_timeout(Duration::from_millis(300))
            .lock_retry_interval(Duration::from_millis(50))
    }
}

async fn scalar(engine: &Engine, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(engine.pool())
        .await
        .unwrap()
}

async fn table_exists(engine: &Engine, name: &str) -> bool {
    let sql =
        format!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{name}'");
    scalar(engine, &sql).await == 1
}

async fn lock_held(engine: &Engine) -> bool {
    scalar(
        engine,
        "SELECT COUNT(*) FROM __schema_lock WHERE locked = TRUE",
    )
    .await
        == 1
}

const BASE: &str = "\
--liquibase formatted sql

--changeset alice:1
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
INSERT INTO users (name) VALUES ('ann');
--rollback
DROP TABLE users;

--changeset alice:2
CREATE TABLE posts (id INTEGER PRIMARY KEY);
--rollback DROP TABLE posts;
";

#[tokio::test]
async fn applies_pending_changesets_once() {
    let fx = Fixture::new();
    fx.write("001.sql", BASE);
    let engine = fx.engine().await;

    let report = engine.migrate().await.unwrap();
    assert_eq!(report.executed.len(), 2);
    assert_eq!(report.skipped, 0);
    assert_eq!(scalar(&engine, "SELECT COUNT(*) FROM users").await, 1);
    assert!(table_exists(&engine, "posts").await);

    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, author, filename, order_executed FROM __schema_changelog ORDER BY order_executed",
    )
    .fetch_all(engine.pool())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("1".into(), "alice".into(), "001.sql".into(), 1),
            ("2".into(), "alice".into(), "001.sql".into(), 2),
        ]
    );
    assert!(!lock_held(&engine).await);

    let again = engine.migrate().await.unwrap();
    assert!(again.executed.is_empty());
    assert_eq!(again.skipped, 2);
    assert_eq!(scalar(&engine, "SELECT COUNT(*) FROM users").await, 1);
}

#[tokio::test]
async fn files_in_a_directory_run_in_name_order() {
    let fx = Fixture::new();
    fx.write(
        "002.sql",
        "--changeset a:posts\nCREATE TABLE posts (user_id INTEGER REFERENCES users(id));",
    );
    fx.write(
        "001.sql",
        "--changeset a:users\nCREATE TABLE users (id INTEGER PRIMARY KEY);",
    );
    let engine = fx.engine().await;

    let report = engine.migrate().await.unwrap();
    let ids: Vec<_> = report
        .executed
        .iter()
        .map(|k| k.filename.as_str())
        .collect();
    assert_eq!(ids, ["001.sql", "002.sql"]);
}

#[tokio::test]
async fn checksum_mismatch_fails_before_running_anything() {
    let fx = Fixture::new();
    fx.write("001.sql", BASE);
    let engine = fx.engine().await;
    engine.migrate().await.unwrap();

    let edited = BASE.replace("('ann')", "('bob')")
        + "\n--changeset alice:3\nCREATE TABLE later (id INT);\n";
    fx.write("001.sql", &edited);

    let err = engine.migrate().await.unwrap_err();
    assert!(
        matches!(&err, Error::ChecksumMismatch { changeset, .. } if changeset.id == "1"),
        "{err}"
    );
    assert!(
        !table_exists(&engine, "later").await,
        "nothing after validation failure should run"
    );
    assert!(!lock_held(&engine).await, "lock must be released on error");

    let status = engine.status().await.unwrap();
    assert_eq!(status.changesets[0].state, ChangeSetState::ChecksumMismatch);
    assert_eq!(status.changesets[2].state, ChangeSetState::Pending);
}

#[tokio::test]
async fn run_on_change_reexecutes_and_updates_checksum() {
    let fx = Fixture::new();
    let v1 =
        "--changeset a:view runOnChange:true\nCREATE TABLE t (n INT);\nINSERT INTO t VALUES (1);";
    fx.write("001.sql", v1);
    let engine = fx.engine().await;
    engine.migrate().await.unwrap();
    let old: String = sqlx::query_scalar("SELECT checksum FROM __schema_changelog")
        .fetch_one(engine.pool())
        .await
        .unwrap();

    fx.write("001.sql", "--changeset a:view runOnChange:true\nDROP TABLE t;\nCREATE TABLE t (n INT);\nINSERT INTO t VALUES (2);");
    assert_eq!(
        engine.status().await.unwrap().changesets[0].state,
        ChangeSetState::Changed
    );

    let report = engine.migrate().await.unwrap();
    assert_eq!(report.executed.len(), 1);
    assert_eq!(scalar(&engine, "SELECT n FROM t").await, 2);

    let (checksum, order): (String, i64) =
        sqlx::query_as("SELECT checksum, order_executed FROM __schema_changelog")
            .fetch_one(engine.pool())
            .await
            .unwrap();
    assert_ne!(checksum, old);
    assert_eq!(order, 2);
    assert_eq!(
        scalar(&engine, "SELECT COUNT(*) FROM __schema_changelog").await,
        1
    );

    assert!(engine.migrate().await.unwrap().executed.is_empty());
}

#[tokio::test]
async fn run_always_executes_every_time() {
    let fx = Fixture::new();
    fx.write(
        "001.sql",
        "--changeset a:1\nCREATE TABLE hits (n INT);\n--changeset a:2 runAlways:true\nINSERT INTO hits VALUES (1);",
    );
    let engine = fx.engine().await;
    for _ in 0..3 {
        engine.migrate().await.unwrap();
    }
    assert_eq!(scalar(&engine, "SELECT COUNT(*) FROM hits").await, 3);
}

#[tokio::test]
async fn failed_changeset_rolls_back_its_ddl_and_tracking_row() {
    let fx = Fixture::new();
    fx.write(
        "001.sql",
        "--changeset a:ok\nCREATE TABLE ok (id INT);\n\
         --changeset a:bad\nCREATE TABLE half (id INT);\nINSERT INTO no_such_table VALUES (1);",
    );
    let engine = fx.engine().await;

    let err = engine.migrate().await.unwrap_err();
    assert!(
        matches!(&err, Error::ChangeSetFailed { changeset, .. } if changeset.id == "bad"),
        "{err}"
    );
    assert!(table_exists(&engine, "ok").await);
    assert!(
        !table_exists(&engine, "half").await,
        "DDL of the failed changeset must be rolled back"
    );
    assert_eq!(
        scalar(&engine, "SELECT COUNT(*) FROM __schema_changelog").await,
        1
    );
    assert!(!lock_held(&engine).await);
}

#[tokio::test]
async fn rollback_reverts_latest_changesets() {
    let fx = Fixture::new();
    fx.write("001.sql", BASE);
    let engine = fx.engine().await;
    engine.migrate().await.unwrap();

    let rolled = engine.rollback(1).await.unwrap();
    assert_eq!(rolled.len(), 1);
    assert_eq!(rolled[0].id, "2");
    assert!(!table_exists(&engine, "posts").await);
    assert!(table_exists(&engine, "users").await);

    let rolled = engine.rollback(5).await.unwrap();
    assert_eq!(rolled.len(), 1);
    assert!(!table_exists(&engine, "users").await);
    assert_eq!(
        scalar(&engine, "SELECT COUNT(*) FROM __schema_changelog").await,
        0
    );
    assert!(!lock_held(&engine).await);

    // Re-applying after a full rollback works.
    assert_eq!(engine.migrate().await.unwrap().executed.len(), 2);
}

#[tokio::test]
async fn rollback_without_rollback_sql_is_refused() {
    let fx = Fixture::new();
    fx.write("001.sql", "--changeset a:1\nCREATE TABLE t (id INT);\n--rollback DROP TABLE t;\n--changeset a:2\nCREATE TABLE u (id INT);");
    let engine = fx.engine().await;
    engine.migrate().await.unwrap();

    let err = engine.rollback(2).await.unwrap_err();
    assert!(
        matches!(&err, Error::NoRollback(key) if key.id == "2"),
        "{err}"
    );
    assert!(
        table_exists(&engine, "t").await,
        "validation must happen before any rollback runs"
    );
    assert!(!lock_held(&engine).await);
}

#[tokio::test]
async fn lock_times_out_when_held_and_can_be_force_released() {
    let fx = Fixture::new();
    fx.write("001.sql", BASE);
    let engine = fx.engine().await;
    engine.status().await.unwrap(); // creates the tables

    sqlx::query("UPDATE __schema_lock SET locked = TRUE, locked_at = CURRENT_TIMESTAMP, locked_by = 'other-host' WHERE id = 1")
        .execute(engine.pool())
        .await
        .unwrap();

    let err = engine.migrate().await.unwrap_err();
    match &err {
        Error::LockTimeout {
            locked_by, waited, ..
        } => {
            assert_eq!(locked_by.as_deref(), Some("other-host"));
            assert!(*waited >= Duration::from_millis(300));
        }
        _ => panic!("expected LockTimeout, got {err}"),
    }
    assert!(!table_exists(&engine, "users").await);
    assert!(
        lock_held(&engine).await,
        "someone else's lock must not be released"
    );

    assert_eq!(
        engine.force_release_lock().await.unwrap().as_deref(),
        Some("other-host")
    );
    assert_eq!(engine.migrate().await.unwrap().executed.len(), 2);
}

#[tokio::test]
async fn waits_for_lock_released_within_timeout() {
    let fx = Fixture::new();
    fx.write("001.sql", BASE);
    let engine = fx.engine().await.lock_timeout(Duration::from_secs(5));
    engine.status().await.unwrap();
    sqlx::query("UPDATE __schema_lock SET locked = TRUE, locked_by = 'other' WHERE id = 1")
        .execute(engine.pool())
        .await
        .unwrap();

    let pool = engine.pool().clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        sqlx::query("UPDATE __schema_lock SET locked = FALSE, locked_by = NULL WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
    });
    assert_eq!(engine.migrate().await.unwrap().executed.len(), 2);
}

#[tokio::test]
async fn status_reports_unknown_rows() {
    let fx = Fixture::new();
    fx.write("001.sql", BASE);
    let engine = fx.engine().await;
    engine.migrate().await.unwrap();
    std::fs::remove_file(fx.changelog().join("001.sql")).unwrap();
    fx.write("002.sql", "--changeset a:new\nSELECT 1;");

    let status = engine.status().await.unwrap();
    assert_eq!(status.changesets.len(), 1);
    assert_eq!(status.unknown.len(), 2);
    assert_eq!(status.unknown[0].order_executed, 1);

    let err = engine.rollback(1).await.unwrap_err();
    assert!(matches!(err, Error::UnknownChangeSet(_)), "{err}");
}

#[test]
fn single_file_changelog_uses_bare_file_name() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("main.sql");
    std::fs::write(&file, BASE).unwrap();
    let cs = schema::load_changelog(Path::new(&file)).unwrap();
    assert!(cs.iter().all(|c| c.key.filename == "main.sql"));
}
