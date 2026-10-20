# schema

A Liquibase-inspired migration engine for formatted-SQL changelogs. Supports PostgreSQL and SQLite (both have transactional DDL).

## Changelog format

```sql
--liquibase formatted sql

--changeset alice:create-users
CREATE TABLE users (id INTEGER PRIMARY KEY, email VARCHAR(255) NOT NULL);
--rollback
DROP TABLE users;

--changeset alice:active-users runOnChange:true
CREATE OR REPLACE VIEW active_users AS SELECT * FROM users;
--rollback DROP VIEW active_users;
```

- `--changeset <author>:<id>` opens a changeset. Optional attributes: `runOnChange:true`, `runAlways:true`. Other Liquibase attributes are accepted and ignored.
- Everything after `--rollback` (up to the next changeset) is rollback SQL. `--rollback <sql>` on a single line also works.
- `--changelog` takes one file, or a directory: every `.sql` file under it runs in path order.
- A changeset's identity is `(id, author, filename)`. Its checksum is the SHA-256 of its forward SQL, ignoring trailing whitespace. Rollback SQL is not part of the checksum.

## CLI

```sh
export DATABASE_URL=postgres://user@localhost/app   # or sqlite://app.db?mode=rwc
schema status
schema update
schema rollback 2
schema release-locks        # clear a lock left by a crashed run
```

`DATABASE_URL` is also read from a `.env` file in the current directory (or the nearest parent), so a project's existing `.env` works as-is. Variables already set in the shell take precedence.

Flags: `--changelog <path>` (default `db/changelog`), `--lock-timeout <secs>` (default 60), `--lock-retry-ms <ms>` (default 1000).

## Library

```rust
let report = schema::Engine::connect(&url).await?
    .changelog("db/changelog")
    .migrate()
    .await?;

// or, with an existing pool (call sqlx::any::install_default_drivers() first):
schema::Engine::new(pool).migrate().await?;
```

## Behavior

1. Creates `__schema_changelog` and `__schema_lock` if they are missing.
2. Takes the lock with `UPDATE ... WHERE locked = FALSE`. Retries until the timeout, then returns `Error::LockTimeout` naming the holder.
3. Validates every changeset before running any of them. A recorded changeset whose checksum changed fails with `Error::ChecksumMismatch`, unless it is marked `runOnChange`.
4. Runs each changeset and writes its tracking row in one transaction. If a changeset fails, the changesets before it stay committed and the failed one is rolled back completely.
5. Releases the lock on success and on error.

`rollback N` reverts the N most recently executed changesets, newest first. It checks all N before running any: each must exist locally, be unmodified, and have rollback SQL.
