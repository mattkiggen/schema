//! PostgreSQL integration tests. Skipped unless `TEST_POSTGRES_URL` is set, e.g.
//!
//! ```sh
//! docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:17
//! TEST_POSTGRES_URL=postgres://postgres:postgres@localhost/postgres cargo test --test postgres
//! ```
//!
//! Each test runs in its own schema (via `search_path`), dropped when the test passes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use schema::{ChangeSetState, Engine, Error};
use tempfile::TempDir;

struct PgFixture {
    dir: TempDir,
    url: String,
    schema: String,
    admin: Engine,
}

impl PgFixture {
    async fn new() -> Option<Self> {
        let Ok(base) = std::env::var("TEST_POSTGRES_URL") else {
            eprintln!("skipping: TEST_POSTGRES_URL is not set");
            return None;
        };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("migrate_test_{nanos}");
        let admin = Engine::connect(&base).await.unwrap();
        sqlx::raw_sql(&format!("CREATE SCHEMA {schema}"))
            .execute(admin.pool())
            .await
            .unwrap();

        let sep = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{sep}options[search_path]={schema}");
        let dir = TempDir::new().unwrap();
        Some(Self {
            dir,
            url,
            schema,
            admin,
        })
    }

    fn write(&self, content: &str) {
        std::fs::write(self.dir.path().join("changelog.sql"), content).unwrap();
    }

    async fn engine(&self) -> Engine {
        Engine::connect(&self.url)
            .await
            .unwrap()
            .changelog(self.dir.path().join("changelog.sql"))
            .lock_timeout(Duration::from_millis(300))
            .lock_retry_interval(Duration::from_millis(50))
    }

    async fn cleanup(self) {
        sqlx::raw_sql(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(self.admin.pool())
            .await
            .unwrap();
    }
}

async fn scalar(engine: &Engine, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(engine.pool())
        .await
        .unwrap()
}

async fn table_exists(engine: &Engine, name: &str) -> bool {
    let sql = format!(
        "SELECT COUNT(*) FROM information_schema.tables
         WHERE table_schema = current_schema() AND table_name = '{name}'"
    );
    scalar(engine, &sql).await == 1
}

async fn lock_held(engine: &Engine) -> bool {
    scalar(engine, "SELECT COUNT(*) FROM __schema_lock WHERE locked").await == 1
}

// Multi-statement bodies, a dollar-quoted function containing semicolons, and a trigger:
// all must survive being sent to Postgres as one changeset.
const CHANGELOG: &str = r#"--liquibase formatted sql

--changeset alice:users
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL,
    updated_at TIMESTAMPTZ
);
INSERT INTO users (email) VALUES ('ann@example.com');
--rollback
DROP TABLE users;

--changeset alice:touch-trigger
CREATE FUNCTION touch() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER users_touch BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION touch();
--rollback
DROP TRIGGER users_touch ON users;
DROP FUNCTION touch();
"#;

#[tokio::test]
async fn migrate_skip_and_rollback() {
    let Some(fx) = PgFixture::new().await else {
        return;
    };
    fx.write(CHANGELOG);
    let engine = fx.engine().await;

    let report = engine.migrate().await.unwrap();
    assert_eq!(report.executed.len(), 2);
    sqlx::query("UPDATE users SET email = 'bob@example.com'")
        .execute(engine.pool())
        .await
        .unwrap();
    assert_eq!(
        scalar(
            &engine,
            "SELECT COUNT(*) FROM users WHERE updated_at IS NOT NULL"
        )
        .await,
        1
    );
    assert!(!lock_held(&engine).await);

    let status = engine.status().await.unwrap();
    assert!(
        status
            .changesets
            .iter()
            .all(|s| s.state == ChangeSetState::UpToDate)
    );
    let applied = status.changesets[1].applied.as_ref().unwrap();
    assert_eq!(applied.order_executed, 2);
    assert!(
        applied.executed_at.starts_with("20"),
        "executed_at = {}",
        applied.executed_at
    );

    assert_eq!(engine.migrate().await.unwrap().skipped, 2);

    assert_eq!(engine.rollback(2).await.unwrap().len(), 2);
    assert!(!table_exists(&engine, "users").await);
    assert_eq!(
        scalar(&engine, "SELECT COUNT(*) FROM __schema_changelog").await,
        0
    );

    fx.cleanup().await;
}

#[tokio::test]
async fn failed_changeset_rolls_back_ddl() {
    let Some(fx) = PgFixture::new().await else {
        return;
    };
    fx.write(
        "--changeset a:ok\nCREATE TABLE ok (id INT);\n\
         --changeset a:bad\nCREATE TABLE half (id INT);\nINSERT INTO half VALUES ('not a number');",
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
        "Postgres DDL must roll back with the transaction"
    );
    assert_eq!(
        scalar(&engine, "SELECT COUNT(*) FROM __schema_changelog").await,
        1
    );
    assert!(!lock_held(&engine).await);

    fx.cleanup().await;
}

#[tokio::test]
async fn checksum_rules() {
    let Some(fx) = PgFixture::new().await else {
        return;
    };
    fx.write("--changeset a:fixed\nCREATE TABLE t (n INT);\n--changeset a:view runOnChange:true\nCREATE VIEW v AS SELECT 1 AS n;");
    let engine = fx.engine().await;
    engine.migrate().await.unwrap();

    fx.write("--changeset a:fixed\nCREATE TABLE t (n INT);\n--changeset a:view runOnChange:true\nCREATE OR REPLACE VIEW v AS SELECT 2 AS n;");
    assert_eq!(engine.migrate().await.unwrap().executed.len(), 1);
    assert_eq!(scalar(&engine, "SELECT CAST(n AS BIGINT) FROM v").await, 2);
    assert_eq!(
        scalar(
            &engine,
            "SELECT CAST(MAX(order_executed) AS BIGINT) FROM __schema_changelog"
        )
        .await,
        3
    );

    fx.write("--changeset a:fixed\nCREATE TABLE t (n BIGINT);\n--changeset a:view runOnChange:true\nCREATE OR REPLACE VIEW v AS SELECT 2 AS n;");
    let err = engine.migrate().await.unwrap_err();
    assert!(
        matches!(&err, Error::ChecksumMismatch { changeset, .. } if changeset.id == "fixed"),
        "{err}"
    );
    assert!(!lock_held(&engine).await);

    fx.cleanup().await;
}

#[tokio::test]
async fn lock_timeout_reports_holder() {
    let Some(fx) = PgFixture::new().await else {
        return;
    };
    fx.write(CHANGELOG);
    let engine = fx.engine().await;
    engine.status().await.unwrap();
    sqlx::query("UPDATE __schema_lock SET locked = TRUE, locked_at = now(), locked_by = 'other-host' WHERE id = 1")
        .execute(engine.pool())
        .await
        .unwrap();

    match engine.migrate().await.unwrap_err() {
        Error::LockTimeout {
            locked_by,
            locked_at,
            ..
        } => {
            assert_eq!(locked_by.as_deref(), Some("other-host"));
            assert!(locked_at.is_some());
        }
        err => panic!("expected LockTimeout, got {err}"),
    }
    assert!(!table_exists(&engine, "users").await);

    assert_eq!(
        engine.force_release_lock().await.unwrap().as_deref(),
        Some("other-host")
    );
    assert_eq!(engine.migrate().await.unwrap().executed.len(), 2);

    fx.cleanup().await;
}

#[tokio::test]
async fn concurrent_engines_run_each_changeset_once() {
    let Some(fx) = PgFixture::new().await else {
        return;
    };
    fx.write("--changeset a:1\nCREATE TABLE hits (n INT);\nSELECT pg_sleep(0.2);");

    let a = fx.engine().await.lock_timeout(Duration::from_secs(10));
    let b = fx.engine().await.lock_timeout(Duration::from_secs(10));
    let (ra, rb) = tokio::join!(a.migrate(), b.migrate());
    let executed = ra.unwrap().executed.len() + rb.unwrap().executed.len();
    assert_eq!(executed, 1, "exactly one engine should run the changeset");

    fx.cleanup().await;
}
