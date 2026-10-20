use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::AnyPool;
use sqlx::any::AnyPoolOptions;

use crate::changeset::{ChangeSet, ChangeSetId, load_changelog};
use crate::error::{Error, Result};
use crate::history::{self, AppliedChangeSet};
use crate::lock::{self, LockConfig};

pub const DEFAULT_CHANGELOG: &str = "db/changelog";

/// Applies a formatted-SQL changelog to a database.
///
/// Supported backends are PostgreSQL and SQLite: both have transactional DDL, so each
/// changeset and its tracking row commit or roll back together.
///
/// ```no_run
/// # async fn run() -> schema::Result<()> {
/// let engine = schema::Engine::connect("postgres://localhost/app").await?;
/// let report = engine.changelog("db/changelog").migrate().await?;
/// println!("applied {} changesets", report.executed.len());
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct Engine {
    pool: AnyPool,
    changelog: PathBuf,
    lock: LockConfig,
}

/// How a local changeset relates to what the database has recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeSetState {
    /// Never executed; `update` will run it.
    Pending,
    /// Executed and unchanged; `update` will skip it.
    UpToDate,
    /// Modified since it ran and marked `runOnChange`; `update` will re-run it.
    Changed,
    /// Marked `runAlways`; `update` will re-run it.
    RunAlways,
    /// Modified since it ran without `runOnChange`; `update` will refuse to run.
    ChecksumMismatch,
}

#[derive(Debug, Clone)]
pub struct ChangeSetStatus {
    pub changeset: ChangeSet,
    pub state: ChangeSetState,
    pub applied: Option<AppliedChangeSet>,
}

#[derive(Debug, Clone)]
pub struct StatusReport {
    /// Every local changeset, in changelog order.
    pub changesets: Vec<ChangeSetStatus>,
    /// Rows recorded in the database with no matching local changeset, oldest first.
    pub unknown: Vec<AppliedChangeSet>,
}

impl StatusReport {
    pub fn pending(&self) -> impl Iterator<Item = &ChangeSetStatus> {
        self.changesets
            .iter()
            .filter(|s| s.state != ChangeSetState::UpToDate)
    }
}

#[derive(Debug, Clone, Default)]
pub struct MigrateReport {
    /// Changesets executed by this run, in execution order.
    pub executed: Vec<ChangeSetId>,
    /// Changesets skipped because they were already applied and unchanged.
    pub skipped: usize,
}

fn classify(cs: &ChangeSet, applied: Option<&AppliedChangeSet>) -> ChangeSetState {
    match applied {
        None => ChangeSetState::Pending,
        Some(a) if a.checksum != cs.checksum && cs.run_on_change => ChangeSetState::Changed,
        Some(a) if a.checksum != cs.checksum => ChangeSetState::ChecksumMismatch,
        Some(_) if cs.run_always => ChangeSetState::RunAlways,
        Some(_) => ChangeSetState::UpToDate,
    }
}

fn mismatch(cs: &ChangeSet, applied: &AppliedChangeSet) -> Error {
    Error::ChecksumMismatch {
        changeset: cs.key.clone(),
        recorded: applied.checksum.clone(),
        computed: cs.checksum.clone(),
    }
}

impl Engine {
    /// Wraps an existing pool. Call [`sqlx::any::install_default_drivers`] before creating it.
    pub fn new(pool: AnyPool) -> Self {
        Self {
            pool,
            changelog: PathBuf::from(DEFAULT_CHANGELOG),
            lock: LockConfig::default(),
        }
    }

    /// Connects to `url` (`postgres://...` or `sqlite://...`).
    pub async fn connect(url: &str) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await?;
        Ok(Self::new(pool))
    }

    /// The changelog file, or a directory of `.sql` files. Defaults to [`DEFAULT_CHANGELOG`].
    pub fn changelog(mut self, path: impl AsRef<Path>) -> Self {
        self.changelog = path.as_ref().to_path_buf();
        self
    }

    pub fn lock_config(mut self, lock: LockConfig) -> Self {
        self.lock = lock;
        self
    }

    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock.timeout = timeout;
        self
    }

    pub fn lock_retry_interval(mut self, interval: Duration) -> Self {
        self.lock.retry_interval = interval;
        self
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// Applies every pending changeset.
    ///
    /// All checksums are validated before anything runs, so a [`Error::ChecksumMismatch`]
    /// leaves the database untouched. Each changeset then runs in its own transaction
    /// together with its tracking row; a failure stops the run with earlier changesets
    /// committed and the failing one rolled back.
    pub async fn migrate(&self) -> Result<MigrateReport> {
        let changesets = load_changelog(&self.changelog)?;
        let holder = self.acquire_lock().await?;
        let result = self.migrate_locked(&changesets).await;
        self.release_lock(&holder, result).await
    }

    /// Rolls back the `count` most recently executed changesets, newest first, using their
    /// `--rollback` SQL from the local changelog.
    ///
    /// Every target is validated (present locally, unmodified, has rollback SQL) before any
    /// rollback runs.
    pub async fn rollback(&self, count: u32) -> Result<Vec<ChangeSetId>> {
        let changesets = load_changelog(&self.changelog)?;
        let holder = self.acquire_lock().await?;
        let result = self.rollback_locked(&changesets, count).await;
        self.release_lock(&holder, result).await
    }

    /// Reports the state of every changeset. Does not take the lock.
    pub async fn status(&self) -> Result<StatusReport> {
        let changesets = load_changelog(&self.changelog)?;
        self.ensure_tables().await?;
        let mut applied = history::all(&self.pool).await?;

        let changesets = changesets
            .into_iter()
            .map(|cs| {
                let applied = applied.remove(&cs.key);
                ChangeSetStatus {
                    state: classify(&cs, applied.as_ref()),
                    changeset: cs,
                    applied,
                }
            })
            .collect();
        let mut unknown: Vec<_> = applied.into_values().collect();
        unknown.sort_by_key(|a| a.order_executed);
        Ok(StatusReport {
            changesets,
            unknown,
        })
    }

    /// Clears the lock regardless of holder, e.g. after a crashed run. Returns the previous
    /// holder, if the lock was held.
    pub async fn force_release_lock(&self) -> Result<Option<String>> {
        self.ensure_tables().await?;
        lock::force_release(&self.pool).await
    }

    async fn ensure_tables(&self) -> Result<()> {
        lock::ensure_table(&self.pool).await?;
        history::ensure_table(&self.pool).await
    }

    async fn acquire_lock(&self) -> Result<String> {
        self.ensure_tables().await?;
        let holder = lock::holder_id();
        lock::acquire(&self.pool, &holder, self.lock).await?;
        Ok(holder)
    }

    /// Releases the lock whatever `result` is. The operation's error takes precedence over
    /// a release error.
    async fn release_lock<T>(&self, holder: &str, result: Result<T>) -> Result<T> {
        let released = lock::release(&self.pool, holder).await;
        let value = result?;
        released?;
        Ok(value)
    }

    async fn migrate_locked(&self, changesets: &[ChangeSet]) -> Result<MigrateReport> {
        let applied = history::all(&self.pool).await?;

        let mut report = MigrateReport::default();
        let mut plan = Vec::new();
        for cs in changesets {
            let recorded = applied.get(&cs.key);
            match classify(cs, recorded) {
                ChangeSetState::UpToDate => report.skipped += 1,
                ChangeSetState::ChecksumMismatch => return Err(mismatch(cs, recorded.unwrap())),
                ChangeSetState::Pending => plan.push((cs, false)),
                ChangeSetState::Changed | ChangeSetState::RunAlways => plan.push((cs, true)),
            }
        }

        for (cs, rerun) in plan {
            let mut tx = self.pool.begin().await?;
            sqlx::raw_sql(&cs.sql)
                .execute(&mut *tx)
                .await
                .map_err(|source| Error::ChangeSetFailed {
                    changeset: cs.key.clone(),
                    source,
                })?;
            if rerun {
                history::update(&mut tx, cs).await?;
            } else {
                history::insert(&mut tx, cs).await?;
            }
            tx.commit().await?;
            report.executed.push(cs.key.clone());
        }
        Ok(report)
    }

    async fn rollback_locked(
        &self,
        changesets: &[ChangeSet],
        count: u32,
    ) -> Result<Vec<ChangeSetId>> {
        let local: HashMap<&ChangeSetId, &ChangeSet> =
            changesets.iter().map(|cs| (&cs.key, cs)).collect();

        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for applied in history::latest(&self.pool, count).await? {
            let cs = *local
                .get(&applied.key)
                .ok_or_else(|| Error::UnknownChangeSet(applied.key.clone()))?;
            if classify(cs, Some(&applied)) == ChangeSetState::ChecksumMismatch {
                return Err(mismatch(cs, &applied));
            }
            let rollback = cs
                .rollback
                .as_deref()
                .ok_or_else(|| Error::NoRollback(cs.key.clone()))?;
            if seen.insert(&cs.key) {
                targets.push((cs, rollback));
            }
        }

        let mut rolled_back = Vec::new();
        for (cs, rollback) in targets {
            let mut tx = self.pool.begin().await?;
            sqlx::raw_sql(rollback)
                .execute(&mut *tx)
                .await
                .map_err(|source| Error::ChangeSetFailed {
                    changeset: cs.key.clone(),
                    source,
                })?;
            history::delete(&mut tx, &cs.key).await?;
            tx.commit().await?;
            rolled_back.push(cs.key.clone());
        }
        Ok(rolled_back)
    }
}
