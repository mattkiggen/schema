//! A Liquibase-inspired migration engine for formatted-SQL changelogs.
//!
//! State lives in two tables the engine creates on first use:
//! `__schema_changelog` (one row per executed changeset) and `__schema_lock`
//! (a single-row exclusive lock).

pub mod changeset;
mod engine;
mod error;
mod history;
mod lock;

pub use changeset::{ChangeSet, ChangeSetId, load_changelog, parse};
pub use engine::{
    ChangeSetState, ChangeSetStatus, DEFAULT_CHANGELOG, Engine, MigrateReport, StatusReport,
};
pub use error::{Error, Result};
pub use history::AppliedChangeSet;
pub use lock::LockConfig;
