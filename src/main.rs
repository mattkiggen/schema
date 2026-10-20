use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use schema::{ChangeSetState, Engine};

#[derive(Parser)]
#[command(
    version,
    about = "Liquibase-style migrations for formatted SQL changelogs"
)]
struct Cli {
    /// Database URL, e.g. postgres://user@localhost/app or sqlite://app.db?mode=rwc
    #[arg(long, env = "DATABASE_URL")]
    url: String,

    /// Changelog file, or directory of .sql files
    #[arg(long, default_value = schema::DEFAULT_CHANGELOG)]
    changelog: PathBuf,

    /// Seconds to wait for the migration lock before giving up
    #[arg(long, default_value_t = 60)]
    lock_timeout: u64,

    /// Milliseconds between lock attempts
    #[arg(long, default_value_t = 1000)]
    lock_retry_ms: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show each changeset and whether `update` would run it
    Status,
    /// Apply all pending changesets
    Update,
    /// Roll back the most recently executed changesets
    Rollback {
        /// Number of changesets to roll back
        count: u32,
    },
    /// Clear a lock left behind by a crashed run
    ReleaseLocks,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> schema::Result<()> {
    let engine = Engine::connect(&cli.url)
        .await?
        .changelog(&cli.changelog)
        .lock_timeout(Duration::from_secs(cli.lock_timeout))
        .lock_retry_interval(Duration::from_millis(cli.lock_retry_ms));

    match cli.command {
        Command::Status => {
            let report = engine.status().await?;
            for s in &report.changesets {
                let state = match s.state {
                    ChangeSetState::Pending => "pending",
                    ChangeSetState::UpToDate => "applied",
                    ChangeSetState::Changed => "changed (will re-run)",
                    ChangeSetState::RunAlways => "applied (runAlways)",
                    ChangeSetState::ChecksumMismatch => "CHECKSUM MISMATCH",
                };
                let when = s
                    .applied
                    .as_ref()
                    .map(|a| format!("  [{}]", a.executed_at))
                    .unwrap_or_default();
                println!("{state:<22} {}{when}", s.changeset.key);
            }
            for a in &report.unknown {
                println!("{:<22} {}  [{}]", "not in changelog", a.key, a.executed_at);
            }
            println!(
                "{} of {} changesets to run",
                report.pending().count(),
                report.changesets.len()
            );
        }
        Command::Update => {
            let report = engine.migrate().await?;
            for key in &report.executed {
                println!("executed {key}");
            }
            println!(
                "{} executed, {} already up to date",
                report.executed.len(),
                report.skipped
            );
        }
        Command::Rollback { count } => {
            let rolled_back = engine.rollback(count).await?;
            for key in &rolled_back {
                println!("rolled back {key}");
            }
            println!("{} rolled back", rolled_back.len());
        }
        Command::ReleaseLocks => match engine.force_release_lock().await? {
            Some(holder) => println!("released lock held by {holder}"),
            None => println!("lock was not held"),
        },
    }
    Ok(())
}
