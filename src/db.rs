//! The database adapter contract. See docs/database-safety.md.
//!
//! Each source implements [`Source`] with its native client and its control-connection
//! queries. [`dump`] runs the steps that all sources share: staging, the client process,
//! the watchdog, cancellation and the closure check.

mod dump;

use std::{ffi::OsString, fmt, path::Path, path::PathBuf};

use serde::Serialize;

pub use dump::dump;

/// The source-specific steps of a dump. [`dump`] runs them in the order of the safety contract.
///
/// The async methods use the control connection. It runs short autocommit statements and holds
/// no locks and no snapshot. If the connection failed, a method opens a new one.
// shortcut: the futures are not Send, so a dump runs on one thread. Return `impl Future + Send` to move it.
#[allow(async_fn_in_trait)]
pub trait Source {
    /// The native client, such as `pg_dump`. It writes the dump to standard output.
    const CLIENT: &'static str;
    /// The dump format, which selects the restore tool.
    const FORMAT: &'static str;

    /// Opens the control connection and checks the server, the privileges and the source
    /// conditions. `client_version` is the first line of `CLIENT --version`.
    /// Returns the server version.
    async fn preflight(&mut self, client_version: &str) -> Result<String, String>;

    /// The client command for this run. [`dump`] writes [`Client::credentials`] to `credentials`.
    fn client(&self, run_id: &str, credentials: &Path) -> Client;

    /// The backup sessions of this run, and the lock waits that involve them.
    async fn observe(&mut self, run_id: &str) -> Result<Observation, String>;

    /// Ends the backup sessions of this run on the server, and no other sessions.
    async fn terminate(&mut self, run_id: &str) -> Result<(), String>;
}

/// The arguments and environment of the client. It runs without a shell, and its environment
/// holds only `PATH` and `env`.
pub struct Client {
    pub args: Vec<OsString>,
    pub env: Vec<(&'static str, OsString)>,
    /// The contents of the private credential file. [`dump`] deletes the file after the client exits.
    pub credentials: Option<String>,
}

/// The result of one detection query.
#[derive(Debug, Default)]
pub struct Observation {
    /// The IDs of the backup sessions of this run.
    pub sessions: Vec<String>,
    pub waits: Vec<Wait>,
}

/// A lock wait. The value identifies the wait from one query to the next and appears in errors.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Wait {
    /// A backup session waits for a lock. The value describes the lock.
    Backup(String),
    /// Another session waits for a lock that a backup session holds. The value describes the session.
    Blocked(String),
}

/// A complete dump in the run directory. The client exited with status 0, and no backup
/// session remains.
#[derive(Debug)]
pub struct Dump {
    pub path: PathBuf,
    pub format: &'static str,
    pub client_version: String,
    pub server_version: String,
    pub size: u64,
    /// The SHA-256 of the file, in lowercase hexadecimal.
    pub sha256: String,
}

#[derive(Debug)]
pub struct Error {
    pub kind: Failure,
    pub message: String,
}

/// The error kinds for the JSON result. The safety contract defines the safety failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// A check before the dump failed: the client, the connection, the versions, the privileges
    /// or the source conditions.
    Preflight,
    DetectionUnavailable,
    /// The client did not start, or it exited with an error.
    Client,
    LockTimeout,
    Contention,
    DumpTimeout,
    DetectionLost,
    StagingLimit,
    /// A staging file cannot be written.
    Staging,
    Cancelled,
    /// A backup session or a staging file remains.
    CleanupFailed,
}

impl Error {
    fn new(kind: Failure, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
