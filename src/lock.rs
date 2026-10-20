//! Exclusive migration lock backed by the single-row `__schema_lock` table.
//!
//! The lock is a row flag rather than a session-level database lock, so it is held across
//! connections and transactions (the Liquibase model). If a process dies while holding it,
//! clear it with [`crate::Engine::force_release_lock`].

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::AnyPool;

use crate::error::{Error, Result};

pub const LOCK_TABLE: &str = "__schema_lock";

#[derive(Debug, Clone, Copy)]
pub struct LockConfig {
    /// Total time to keep retrying before giving up with [`Error::LockTimeout`].
    pub timeout: Duration,
    /// Pause between attempts.
    pub retry_interval: Duration,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            retry_interval: Duration::from_secs(1),
        }
    }
}

pub(crate) async fn ensure_table(pool: &AnyPool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS __schema_lock (
            id INT PRIMARY KEY,
            locked BOOLEAN NOT NULL,
            locked_at TIMESTAMP NULL,
            locked_by VARCHAR(255) NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO __schema_lock (id, locked) VALUES (1, FALSE) ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A unique identity for this lock holder: host, pid and a per-call nonce, so two engines
/// in the same process cannot release each other's lock.
pub(crate) fn holder_id() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{} (pid {}, {:x})",
        gethostname::gethostname().to_string_lossy(),
        std::process::id(),
        nonce
    )
}

pub(crate) async fn acquire(pool: &AnyPool, holder: &str, config: LockConfig) -> Result<()> {
    let started = Instant::now();
    loop {
        let claimed = sqlx::query(
            "UPDATE __schema_lock SET locked = TRUE, locked_at = CURRENT_TIMESTAMP, locked_by = $1
             WHERE id = 1 AND locked = FALSE",
        )
        .bind(holder)
        .execute(pool)
        .await?
        .rows_affected();
        if claimed == 1 {
            return Ok(());
        }

        let elapsed = started.elapsed();
        if elapsed >= config.timeout {
            let (locked_by, locked_at) = current_holder(pool).await?.unwrap_or_default();
            return Err(Error::LockTimeout {
                table: LOCK_TABLE,
                waited: elapsed,
                locked_by,
                locked_at,
            });
        }
        tokio::time::sleep(config.retry_interval.min(config.timeout - elapsed)).await;
    }
}

pub(crate) async fn release(pool: &AnyPool, holder: &str) -> Result<()> {
    sqlx::query(
        "UPDATE __schema_lock SET locked = FALSE, locked_at = NULL, locked_by = NULL
         WHERE id = 1 AND locked_by = $1",
    )
    .bind(holder)
    .execute(pool)
    .await?;
    Ok(())
}

/// Clears the lock regardless of who holds it. Returns the previous holder, if any.
pub(crate) async fn force_release(pool: &AnyPool) -> Result<Option<String>> {
    let previous = current_holder(pool).await?.and_then(|(by, _)| by);
    sqlx::query(
        "UPDATE __schema_lock SET locked = FALSE, locked_at = NULL, locked_by = NULL WHERE id = 1",
    )
    .execute(pool)
    .await?;
    Ok(previous)
}

/// `(locked_by, locked_at)` when the lock is held.
async fn current_holder(pool: &AnyPool) -> Result<Option<(Option<String>, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT locked_by, CAST(locked_at AS VARCHAR(64)) FROM __schema_lock
         WHERE id = 1 AND locked = TRUE",
    )
    .fetch_optional(pool)
    .await?)
}
