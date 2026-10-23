//! The storage contract. See "Storage" in README.md.
//!
//! Each destination implements [`Destination`]. [`store`] runs the steps that all destinations
//! share: the names, the completion record, the deadline, cancellation and cleanup.

mod s3;

use std::{
    fmt, fs,
    future::Future,
    io::{ErrorKind, Write as _},
    path::Path,
    pin::pin,
    time::Duration,
};

use aws_smithy_types::date_time::{DateTime, Format};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time;

use crate::{
    db::Dump,
    staging::{self, Run},
};

pub use s3::S3;

/// The time for cleanup after an error.
const CLEANUP: Duration = Duration::from_secs(30);

/// The upload steps of a destination. [`store`] calls them.
// shortcut: the futures are not Send, so an upload runs on one thread. Return `impl Future + Send` to move it.
#[allow(async_fn_in_trait)]
pub trait Destination {
    /// Uploads `file` to `key`. Fails unless the destination confirms the file's SHA-256.
    async fn put(&mut self, key: &str, file: &Staged<'_>) -> Result<(), Error>;

    /// Removes what an unfinished [`Destination::put`] to `key` left. Never removes a complete
    /// object, so a destination that denies deletes keeps its backups.
    async fn abort(&mut self, key: &str) -> Result<(), Error>;
}

/// A local file to upload.
pub struct Staged<'a> {
    pub path: &'a Path,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// The names of a stored backup.
#[derive(Debug, PartialEq, Eq)]
pub struct Stored {
    pub data: String,
    /// The completion record. Data without a record is an incomplete backup.
    pub record: String,
}

#[derive(Debug)]
pub struct Error {
    pub kind: Failure,
    pub message: String,
}

/// The error kinds for the JSON result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// The destination refused the credentials or the request.
    AccessDenied,
    Upload,
    UploadTimeout,
    Cancelled,
    /// An unfinished upload remains at the destination.
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

/// The completion record, version 1.
#[derive(Serialize)]
struct Record<'a> {
    version: u32,
    created: &'a str,
    run_id: &'a str,
    data: &'a str,
    format: &'a str,
    client_version: &'a str,
    server_version: &'a str,
    size: u64,
    sha256: &'a str,
}

/// Uploads `dump`, then its completion record, under `prefix`.
///
/// The names depend only on the run, so a second call for the same run replaces the same
/// objects. After an error, no unfinished upload remains unless the error is
/// [`Failure::CleanupFailed`]. The data can remain without its record.
pub async fn store<D: Destination>(
    destination: &mut D,
    prefix: &str,
    dump: &Dump,
    run: &Run,
    timeout: Duration,
    cancel: impl Future<Output = ()>,
) -> Result<Stored, Error> {
    let created = DateTime::from_secs(DateTime::from(run.started).secs())
        .fmt(Format::DateTime)
        .map_err(|error| Error::new(Failure::Upload, format!("invalid run time: {error}")))?;
    let stem = format!("{prefix}{}-{}", created.replace(['-', ':'], ""), run.id);
    let stored = Stored {
        data: format!("{stem}.{}", dump.extension),
        record: format!("{stem}.json"),
    };
    let sha256 = digest(&dump.sha256).ok_or_else(|| {
        Error::new(
            Failure::Upload,
            format!("invalid SHA-256 `{}`", dump.sha256),
        )
    })?;
    let record = serde_json::to_vec_pretty(&Record {
        version: 1,
        created: &created,
        run_id: &run.id,
        data: &stored.data,
        format: dump.format,
        client_version: &dump.client_version,
        server_version: &dump.server_version,
        size: dump.size,
        sha256: &dump.sha256,
    })
    .expect("the record serializes");
    let record_path = run.dir.join("record.json");
    write_private(&record_path, &record).map_err(|error| {
        Error::new(
            Failure::Upload,
            format!("cannot write {}: {error}", record_path.display()),
        )
    })?;

    let files = [
        (
            &stored.data,
            Staged {
                path: &dump.path,
                size: dump.size,
                sha256,
            },
        ),
        (
            &stored.record,
            Staged {
                path: &record_path,
                size: record.len() as u64,
                sha256: Sha256::digest(&record).into(),
            },
        ),
    ];
    let mut deadline = pin!(time::sleep(timeout));
    let mut cancel = pin!(cancel);
    for (key, file) in &files {
        let result = tokio::select! {
            result = destination.put(key, file) => result,
            () = deadline.as_mut() => Err(Error::new(
                Failure::UploadTimeout,
                format!("the upload took longer than {timeout:?}"),
            )),
            () = cancel.as_mut() => Err(Error::new(Failure::Cancelled, "the upload was cancelled")),
        };
        if let Err(error) = result {
            let cleanup = match time::timeout(CLEANUP, destination.abort(key)).await {
                Ok(cleanup) => cleanup,
                Err(_) => Err(Error::new(
                    Failure::CleanupFailed,
                    format!("the cleanup of {key} took longer than {CLEANUP:?}"),
                )),
            };
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => Error::new(
                    cleanup.kind,
                    format!("{cleanup} after an earlier error: {error}"),
                ),
            });
        }
    }
    Ok(stored)
}

/// Writes a private file. A retry of the same run replaces it.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    staging::create_private(path)?.write_all(contents)
}

/// The bytes of a lowercase or uppercase hexadecimal SHA-256.
fn digest(hex: &str) -> Option<[u8; 32]> {
    let mut digest = [0; 32];
    if hex.len() != 64 {
        return None;
    }
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_reads_hexadecimal_sha256_only() {
        let hex = "b287a8ff3f309882607669c709380d0ee77fac582295bb76900c412cde46ef19";
        assert_eq!(digest(hex).map(|d| d[0]), Some(0xb2));
        assert_eq!(digest(&hex[2..]), None);
        assert_eq!(digest(&hex.replace('b', "x")), None);
    }
}
