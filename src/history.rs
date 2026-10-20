//! Access to the `__schema_changelog` tracking table.

use std::collections::HashMap;

use sqlx::{AnyConnection, AnyPool};

use crate::changeset::{ChangeSet, ChangeSetId};
use crate::error::Result;

/// A row of `__schema_changelog`.
#[derive(Debug, Clone)]
pub struct AppliedChangeSet {
    pub key: ChangeSetId,
    pub checksum: String,
    /// As rendered by the database, e.g. `2026-10-20 14:03:11`.
    pub executed_at: String,
    pub order_executed: i64,
}

pub(crate) async fn ensure_table(pool: &AnyPool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS __schema_changelog (
            id VARCHAR(255) NOT NULL,
            author VARCHAR(255) NOT NULL,
            filename VARCHAR(255) NOT NULL,
            checksum VARCHAR(64) NOT NULL,
            executed_at TIMESTAMP NOT NULL,
            order_executed INT NOT NULL,
            PRIMARY KEY (id, author, filename)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

// Casts keep decoding uniform across backends: the `Any` driver has no timestamp type, and
// SQLite reports INT columns as 64-bit.
const SELECT_ROWS: &str = "SELECT id, author, filename, checksum,
        CAST(executed_at AS VARCHAR(64)), CAST(order_executed AS BIGINT)
    FROM __schema_changelog";

type Row = (String, String, String, String, String, i64);

fn from_row(
    (id, author, filename, checksum, executed_at, order_executed): Row,
) -> AppliedChangeSet {
    AppliedChangeSet {
        key: ChangeSetId {
            id,
            author,
            filename,
        },
        checksum,
        executed_at,
        order_executed,
    }
}

pub(crate) async fn all(pool: &AnyPool) -> Result<HashMap<ChangeSetId, AppliedChangeSet>> {
    let rows: Vec<Row> = sqlx::query_as(SELECT_ROWS).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(from_row)
        .map(|a| (a.key.clone(), a))
        .collect())
}

/// The most recently executed changesets, newest first.
pub(crate) async fn latest(pool: &AnyPool, count: u32) -> Result<Vec<AppliedChangeSet>> {
    let sql = format!("{SELECT_ROWS} ORDER BY order_executed DESC LIMIT $1");
    let rows: Vec<Row> = sqlx::query_as(&sql)
        .bind(i64::from(count))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(from_row).collect())
}

async fn next_order(conn: &mut AnyConnection) -> Result<i32> {
    let (next,): (i64,) = sqlx::query_as(
        "SELECT CAST(COALESCE(MAX(order_executed), 0) + 1 AS BIGINT) FROM __schema_changelog",
    )
    .fetch_one(conn)
    .await?;
    Ok(i32::try_from(next).expect("order_executed overflowed INT"))
}

pub(crate) async fn insert(conn: &mut AnyConnection, cs: &ChangeSet) -> Result<()> {
    let order = next_order(conn).await?;
    sqlx::query(
        "INSERT INTO __schema_changelog (id, author, filename, checksum, executed_at, order_executed)
         VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP, $5)",
    )
    .bind(&cs.key.id)
    .bind(&cs.key.author)
    .bind(&cs.key.filename)
    .bind(&cs.checksum)
    .bind(order)
    .execute(conn)
    .await?;
    Ok(())
}

/// Records a re-execution: new checksum, timestamp and position in the execution order.
pub(crate) async fn update(conn: &mut AnyConnection, cs: &ChangeSet) -> Result<()> {
    let order = next_order(conn).await?;
    sqlx::query(
        "UPDATE __schema_changelog
         SET checksum = $1, executed_at = CURRENT_TIMESTAMP, order_executed = $2
         WHERE id = $3 AND author = $4 AND filename = $5",
    )
    .bind(&cs.checksum)
    .bind(order)
    .bind(&cs.key.id)
    .bind(&cs.key.author)
    .bind(&cs.key.filename)
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn delete(conn: &mut AnyConnection, key: &ChangeSetId) -> Result<()> {
    sqlx::query("DELETE FROM __schema_changelog WHERE id = $1 AND author = $2 AND filename = $3")
        .bind(&key.id)
        .bind(&key.author)
        .bind(&key.filename)
        .execute(conn)
        .await?;
    Ok(())
}
