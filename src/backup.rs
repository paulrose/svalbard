//! The backup command: check the destination, dump the source, store the dump and its record,
//! and remove the run directory. See "Back up" in README.md.

use std::{fmt, future, time::Duration};

use serde::Serialize;
use tokio::{
    sync::watch,
    time::{self, Instant},
};

use crate::{
    config::{self, Config, Kind, Safety, Target},
    db::{self, Mysql, Postgres, Source},
    staging::Run,
    storage::{self, Destination, S3, Ssh},
};

/// The time for the destination check.
const CHECK: Duration = Duration::from_secs(30);
/// The wait before the one retry of a failed transfer.
const RETRY: Duration = Duration::from_secs(10);

/// A complete backup: the dump and its record are stored.
#[derive(Debug, Serialize)]
pub struct Report {
    pub run_id: String,
    pub data: String,
    pub record: String,
    pub size: u64,
    pub sha256: String,
    pub format: &'static str,
    pub client_version: String,
    pub server_version: String,
}

/// A failed backup. `cleanup_failed` means that something may remain: a backup session, the
/// run directory, or an unfinished upload.
#[derive(Debug, Serialize)]
pub struct Failed {
    pub stage: Stage,
    /// The failure kind, such as `lock_timeout`.
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// The stages, in the order of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Configuration,
    Destination,
    Staging,
    Dump,
    Transfer,
    Cleanup,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&name(self))
    }
}

impl Failed {
    pub fn new(
        stage: Stage,
        error: impl Serialize,
        message: impl Into<String>,
        run: Option<&Run>,
    ) -> Failed {
        Failed {
            stage,
            error: name(error),
            message: message.into(),
            run_id: run.map(|run| run.id.clone()),
        }
    }
}

/// Runs one backup. `cancel` becomes true on SIGINT or SIGTERM.
pub async fn backup(config: Config, cancel: watch::Receiver<bool>) -> Result<Report, Failed> {
    let Config {
        source,
        destination,
        safety,
    } = config;
    let label = format!("{} on {}", source.database, source.host);
    match source.kind {
        Kind::Postgresql => {
            let source = Postgres::new(source).map_err(dump_failed(None))?;
            with_destination(source, destination, &safety, &label, &cancel).await
        }
        Kind::Mysql => {
            let source = Mysql::new(source).map_err(dump_failed(None))?;
            with_destination(source, destination, &safety, &label, &cancel).await
        }
    }
}

async fn with_destination<S: Source>(
    source: S,
    destination: config::Destination,
    safety: &Safety,
    label: &str,
    cancel: &watch::Receiver<bool>,
) -> Result<Report, Failed> {
    let config::Destination {
        target,
        prefix,
        upload_timeout,
    } = destination;
    let plan = Plan {
        prefix: &prefix,
        upload_timeout,
        safety,
        label,
        cancel,
    };
    match target {
        Target::ObjectStore(store) => back_up(source, S3::new(&store).await, plan).await,
        Target::Ssh(ssh) => back_up(source, Ssh::new(ssh), plan).await,
    }
}

/// The settings of one run.
struct Plan<'a> {
    prefix: &'a str,
    upload_timeout: Duration,
    safety: &'a Safety,
    label: &'a str,
    cancel: &'a watch::Receiver<bool>,
}

async fn back_up<S: Source, D: Destination>(
    source: S,
    mut destination: D,
    plan: Plan<'_>,
) -> Result<Report, Failed> {
    if *plan.cancel.borrow() {
        return Err(cancelled(Stage::Destination, None));
    }
    tokio::select! {
        result = time::timeout(CHECK, destination.check(plan.prefix)) => match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(Failed::new(Stage::Destination, error.kind, error.message, None));
            }
            Err(_) => {
                let message = format!("the destination check took longer than {CHECK:?}");
                return Err(Failed::new(Stage::Destination, storage::Failure::Upload, message, None));
            }
        },
        () = until(plan.cancel) => return Err(cancelled(Stage::Destination, None)),
    }

    let run = Run::create(&plan.safety.staging_dir).map_err(|error| {
        let message = format!(
            "cannot create a run directory in {}: {error}",
            plan.safety.staging_dir.display()
        );
        Failed::new(Stage::Staging, db::Failure::Staging, message, None)
    })?;
    eprintln!("svalbard: run {}: dumping {}", run.id, plan.label);
    let result = dump_and_store(source, &mut destination, &plan, &run).await;

    let dir = run.dir.clone();
    let run_id = run.id.clone();
    match (result, run.remove()) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(Failed {
            stage: Stage::Cleanup,
            error: name(db::Failure::CleanupFailed),
            message: format!("cannot remove {}: {error}", dir.display()),
            run_id: Some(run_id),
        }),
        (Err(failed), Err(error)) => Err(Failed {
            error: name(db::Failure::CleanupFailed),
            message: format!(
                "cannot remove {}: {error} after an earlier error: {}",
                dir.display(),
                failed.message
            ),
            ..failed
        }),
    }
}

/// Dumps the source into `run`, then stores the dump and its record.
async fn dump_and_store<S: Source, D: Destination>(
    mut source: S,
    destination: &mut D,
    plan: &Plan<'_>,
    run: &Run,
) -> Result<Report, Failed> {
    if *plan.cancel.borrow() {
        return Err(cancelled(Stage::Dump, Some(run)));
    }
    let dump = db::dump(&mut source, plan.safety, run, until(plan.cancel))
        .await
        .map_err(dump_failed(Some(run)))?;
    // The snapshot is closed. The control connection closes before the upload starts.
    drop(source);
    eprintln!("svalbard: dumped {} bytes, uploading", dump.size);

    // One deadline for both attempts. Each attempt reuses the staged dump and the same names.
    let deadline = Instant::now() + plan.upload_timeout;
    let mut retried = false;
    let stored = loop {
        if *plan.cancel.borrow() {
            return Err(cancelled(Stage::Transfer, Some(run)));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match storage::store(
            destination,
            plan.prefix,
            &dump,
            run,
            remaining,
            until(plan.cancel),
        )
        .await
        {
            Ok(stored) => break stored,
            // The S3 SDK already retries each request. A retry here restarts the transfer.
            Err(error)
                if error.kind == storage::Failure::Upload
                    && !retried
                    && deadline.saturating_duration_since(Instant::now()) > RETRY =>
            {
                eprintln!("svalbard: the transfer failed, trying again in {RETRY:?}: {error}");
                retried = true;
                tokio::select! {
                    () = time::sleep(RETRY) => {}
                    () = until(plan.cancel) => return Err(cancelled(Stage::Transfer, Some(run))),
                }
            }
            Err(error) => {
                return Err(Failed::new(
                    Stage::Transfer,
                    error.kind,
                    error.message,
                    Some(run),
                ));
            }
        }
    };
    Ok(Report {
        run_id: run.id.clone(),
        data: stored.data,
        record: stored.record,
        size: dump.size,
        sha256: dump.sha256,
        format: dump.format,
        client_version: dump.client_version,
        server_version: dump.server_version,
    })
}

fn cancelled(stage: Stage, run: Option<&Run>) -> Failed {
    Failed::new(
        stage,
        storage::Failure::Cancelled,
        "the backup was cancelled",
        run,
    )
}

fn dump_failed(run: Option<&Run>) -> impl FnOnce(db::Error) -> Failed + '_ {
    move |error| Failed::new(Stage::Dump, error.kind, error.message, run)
}

/// Resolves when `cancel` becomes true. Never resolves if the sender is gone.
async fn until(cancel: &watch::Receiver<bool>) {
    let mut cancel = cancel.clone();
    if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
        future::pending::<()>().await;
    }
}

/// The snake_case name of a failure kind or stage.
fn name(value: impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        value => format!("{value:?}"),
    }
}
