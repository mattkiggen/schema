use std::path::PathBuf;
use std::time::Duration;

use crate::changeset::ChangeSetId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{file}:{line}: {message}")]
    Parse {
        file: String,
        line: usize,
        message: String,
    },

    #[error("duplicate changeset {0}")]
    DuplicateChangeSet(ChangeSetId),

    #[error(
        "checksum mismatch for changeset {changeset}: recorded {recorded}, computed {computed} \
         (the changeset was modified after it ran; revert it or mark it runOnChange:true)"
    )]
    ChecksumMismatch {
        changeset: ChangeSetId,
        recorded: String,
        computed: String,
    },

    #[error("changeset {changeset} failed: {source}")]
    ChangeSetFailed {
        changeset: ChangeSetId,
        #[source]
        source: sqlx::Error,
    },

    #[error("could not acquire {table} within {waited:?} (held by {} since {})",
        locked_by.as_deref().unwrap_or("<unknown>"),
        locked_at.as_deref().unwrap_or("<unknown>"))]
    LockTimeout {
        table: &'static str,
        waited: Duration,
        locked_by: Option<String>,
        locked_at: Option<String>,
    },

    #[error("cannot roll back changeset {0}: it has no --rollback statements")]
    NoRollback(ChangeSetId),

    #[error(
        "cannot roll back changeset {0}: it is recorded in the database but missing from the local changelog"
    )]
    UnknownChangeSet(ChangeSetId),
}
