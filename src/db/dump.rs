//! The shared steps of a dump. See "Run sequence" in docs/database-safety.md.

use std::{
    collections::HashMap,
    env, fs,
    future::Future,
    io::{ErrorKind, Write as _},
    path::Path,
    pin::{Pin, pin},
    process::Stdio,
    time::Duration,
};

use rustix::process::{Pid, Signal, kill_process};
use sha2::{Digest, Sha256};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStderr, Command},
    time::{self, Instant, MissedTickBehavior},
};

use super::{Client, Dump, Error, Failure, Observation, Source, Wait};
use crate::{
    config::Safety,
    staging::{self, Run},
};

/// Time between two detection queries.
const INTERVAL: Duration = Duration::from_secs(1);
/// Time for the client to exit after SIGTERM, and for ended sessions to disappear.
const GRACE: Duration = Duration::from_secs(10);
/// Bytes written between two free-space checks.
const SPACE_CHECK: u64 = 64 << 20;
/// Bytes of client standard error to keep for the error message.
const STDERR_TAIL: usize = 4096;

/// Dumps `source` into the run directory.
///
/// Returns when the client exited, the staged file is closed and no backup session remains.
/// After an error, the client has exited and the run directory holds no dump. No backup
/// session remains unless the error is [`Failure::CleanupFailed`]. The credential file is
/// always deleted.
///
/// To cancel the dump, resolve `cancel`. Dropping the future kills the client, but does not
/// end its sessions on the server.
pub async fn dump<S: Source>(
    source: &mut S,
    safety: &Safety,
    run: &Run,
    cancel: impl Future<Output = ()>,
) -> Result<Dump, Error> {
    let path = run.dir.join("dump");
    let credentials = run.dir.join("credentials");
    let result = dump_to(source, safety, run, &path, &credentials, pin!(cancel)).await;
    let cleanup = match result {
        Ok(_) => remove(&credentials),
        Err(_) => remove(&credentials).and(remove(&path)),
    };
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(after(cleanup, error)),
    }
}

async fn dump_to<S: Source>(
    source: &mut S,
    safety: &Safety,
    run: &Run,
    path: &Path,
    credentials: &Path,
    mut cancel: Pin<&mut impl Future<Output = ()>>,
) -> Result<Dump, Error> {
    check_space(&run.dir, safety.min_free_space)?;
    let preflight = async {
        let client_version = client_version(S::CLIENT).await?;
        let server_version = source
            .preflight(&client_version)
            .await
            .map_err(|error| Error::new(Failure::Preflight, error))?;
        source.observe(&run.id).await.map_err(|error| {
            Error::new(
                Failure::DetectionUnavailable,
                format!("contention detection failed: {error}"),
            )
        })?;
        Ok((client_version, server_version))
    };
    let (client_version, server_version) = tokio::select! {
        result = preflight => result?,
        () = cancel.as_mut() => return Err(cancelled()),
    };

    let client = source.client(&run.id, credentials);
    if let Some(contents) = &client.credentials {
        staging::create_private(credentials)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .map_err(|error| staging_error(credentials, error))?;
    }
    let file = staging::create_private(path).map_err(|error| staging_error(path, error))?;
    let mut staged = Staged {
        path,
        file: File::from_std(file),
        sha256: Sha256::new(),
        size: 0,
        safety,
    };
    let mut child = spawn::<S>(client)?;

    if let Err(error) = supervise(source, safety, &run.id, &mut child, &mut staged, cancel).await {
        stop(source, &run.id, &mut child).await;
        return Err(match end_sessions(source, &run.id).await {
            Ok(()) => error,
            Err(cleanup) => after(cleanup, error),
        });
    }
    end_sessions(source, &run.id).await?;
    let (size, sha256) = staged.finish().await?;
    Ok(Dump {
        path: path.to_owned(),
        format: S::FORMAT,
        client_version,
        server_version,
        size,
        sha256,
    })
}

/// Copies the client output to the staged file until the client exits. Runs the watchdog.
async fn supervise<S: Source>(
    source: &mut S,
    safety: &Safety,
    run_id: &str,
    child: &mut Child,
    staged: &mut Staged<'_>,
    mut cancel: Pin<&mut impl Future<Output = ()>>,
) -> Result<(), Error> {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = tokio::spawn(tail(child.stderr.take().expect("stderr is piped")));
    let started = Instant::now();
    let mut deadline = pin!(time::sleep(safety.dump_timeout));
    let mut ticks = time::interval_at(started + INTERVAL, INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watchdog = Watchdog {
        started,
        detected: started,
        session_seen: false,
        waits: HashMap::new(),
    };
    let mut buffer = vec![0; 64 << 10];
    let mut eof = false;

    loop {
        tokio::select! {
            read = stdout.read(&mut buffer), if !eof => match read {
                Ok(0) => eof = true,
                Ok(n) => staged.write(&buffer[..n]).await?,
                Err(error) => {
                    return Err(Error::new(
                        Failure::Client,
                        format!("cannot read the output of {}: {error}", S::CLIENT),
                    ));
                }
            },
            status = child.wait(), if eof => {
                let status = status.map_err(|error| {
                    Error::new(Failure::Client, format!("cannot wait for {}: {error}", S::CLIENT))
                })?;
                if status.success() {
                    return Ok(());
                }
                let stderr = stderr.await.unwrap_or_default();
                return Err(Error::new(
                    Failure::Client,
                    format!(
                        "{} failed with {status}: {}",
                        S::CLIENT,
                        String::from_utf8_lossy(&stderr).trim()
                    ),
                ));
            },
            _ = ticks.tick() => {
                // A longer query would delay the output copy, the deadline and cancellation.
                let observation = time::timeout(INTERVAL, source.observe(run_id))
                    .await
                    .unwrap_or_else(|_| Err("the detection query timed out".to_owned()));
                watchdog.check(observation, Instant::now(), safety)?;
            },
            () = deadline.as_mut() => {
                return Err(Error::new(
                    Failure::DumpTimeout,
                    format!("the dump took longer than {:?}", safety.dump_timeout),
                ));
            },
            () = cancel.as_mut() => return Err(cancelled()),
        }
    }
}

/// Decides from each detection result whether the backup must stop.
struct Watchdog {
    started: Instant,
    /// The time of the last successful detection.
    detected: Instant,
    session_seen: bool,
    /// The time when each current wait was first seen.
    waits: HashMap<Wait, Instant>,
}

impl Watchdog {
    fn check(
        &mut self,
        detection: Result<Observation, String>,
        now: Instant,
        safety: &Safety,
    ) -> Result<(), Error> {
        let observation = match detection {
            Ok(observation) => observation,
            Err(error) if now - self.detected > safety.contention_timeout => {
                return Err(Error::new(
                    Failure::DetectionLost,
                    format!("contention detection failed: {error}"),
                ));
            }
            Err(_) => return Ok(()),
        };
        self.detected = now;
        self.session_seen |= !observation.sessions.is_empty();
        if !self.session_seen && now - self.started > safety.lock_timeout {
            return Err(Error::new(
                Failure::DetectionLost,
                format!(
                    "the backup session did not appear within {:?}",
                    safety.lock_timeout
                ),
            ));
        }

        self.waits
            .retain(|wait, _| observation.waits.contains(wait));
        for wait in observation.waits {
            let waited = now - *self.waits.entry(wait.clone()).or_insert(now);
            match wait {
                Wait::Backup(lock) if waited > safety.lock_timeout => {
                    return Err(Error::new(
                        Failure::LockTimeout,
                        format!(
                            "the backup waited longer than {:?} for {lock}",
                            safety.lock_timeout
                        ),
                    ));
                }
                Wait::Blocked(session) if waited > safety.contention_timeout => {
                    return Err(Error::new(
                        Failure::Contention,
                        format!(
                            "{session} waited longer than {:?} for a lock that the backup holds",
                            safety.contention_timeout
                        ),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The staged dump. It counts and hashes the bytes, and enforces the staging limits.
struct Staged<'a> {
    path: &'a Path,
    file: File,
    sha256: Sha256,
    size: u64,
    safety: &'a Safety,
}

impl Staged<'_> {
    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let before = self.size;
        self.size += bytes.len() as u64;
        if self.size > self.safety.max_dump_size {
            return Err(Error::new(
                Failure::StagingLimit,
                format!(
                    "the dump is larger than safety.max_dump_size ({} bytes)",
                    self.safety.max_dump_size
                ),
            ));
        }
        self.sha256.update(bytes);
        self.file
            .write_all(bytes)
            .await
            .map_err(|error| staging_error(self.path, error))?;
        if before / SPACE_CHECK != self.size / SPACE_CHECK {
            check_space(self.path, self.safety.min_free_space)?;
        }
        Ok(())
    }

    /// Flushes and closes the file. Returns its size and SHA-256.
    async fn finish(mut self) -> Result<(u64, String), Error> {
        self.file
            .flush()
            .await
            .map_err(|error| staging_error(self.path, error))?;
        // A sync reports write errors that the filesystem defers until the data reaches the disk.
        self.file
            .sync_all()
            .await
            .map_err(|error| staging_error(self.path, error))?;
        let sha256 = self
            .sha256
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok((self.size, sha256))
    }
}

/// A command for the native client. It runs without a shell, with only `PATH` from the
/// environment, so that inherited client options such as `PGOPTIONS` have no effect.
fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear().stdin(Stdio::null()).kill_on_drop(true);
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
}

/// The first line of `program --version`.
async fn client_version(program: &str) -> Result<String, Error> {
    let failed = |reason: String| {
        Error::new(
            Failure::Preflight,
            format!("cannot run {program} --version: {reason}"),
        )
    };
    let output = time::timeout(GRACE, command(program).arg("--version").output())
        .await
        .map_err(|_| failed("timed out".to_owned()))?
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(output.status.to_string()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().next().unwrap_or_default().trim().to_owned())
}

fn spawn<S: Source>(client: Client) -> Result<Child, Error> {
    let mut command = command(S::CLIENT);
    command
        .args(client.args)
        .envs(client.env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The kernel sends the signal when the spawning thread exits, so spawn from a long-lived thread.
    #[cfg(target_os = "linux")]
    // SAFETY: the closure makes one prctl system call, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            Ok(rustix::process::set_parent_process_death_signal(Some(
                Signal::TERM,
            ))?)
        });
    }
    command.spawn().map_err(|error| {
        Error::new(
            Failure::Client,
            format!("cannot start {}: {error}", S::CLIENT),
        )
    })
}

/// Keeps the last [`STDERR_TAIL`] bytes of the client's standard error.
async fn tail(mut stderr: ChildStderr) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buffer = [0; 4096];
    while let Ok(n @ 1..) = stderr.read(&mut buffer).await {
        tail.extend_from_slice(&buffer[..n]);
        tail.drain(..tail.len().saturating_sub(STDERR_TAIL));
    }
    tail
}

/// Ends the backup sessions, then stops the client. See "Cancellation" in docs/database-safety.md.
async fn stop<S: Source>(source: &mut S, run_id: &str, child: &mut Child) {
    // A failure shows as a remaining session in `end_sessions`.
    let _ = time::timeout(GRACE, source.terminate(run_id)).await;
    if let Some(pid) = child.id().and_then(|id| Pid::from_raw(id.try_into().ok()?)) {
        let _ = kill_process(pid, Signal::TERM);
    }
    if time::timeout(GRACE, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

/// Waits until no backup session of the run remains. Ends the sessions that remain.
async fn end_sessions<S: Source>(source: &mut S, run_id: &str) -> Result<(), Error> {
    let deadline = Instant::now() + GRACE;
    loop {
        let remaining = match time::timeout(GRACE, source.observe(run_id)).await {
            Ok(Ok(observation)) if observation.sessions.is_empty() => return Ok(()),
            Ok(Ok(observation)) => {
                format!("backup sessions {} remain", observation.sessions.join(", "))
            }
            Ok(Err(error)) => format!("cannot check for backup sessions: {error}"),
            Err(_) => "cannot check for backup sessions: the detection query timed out".to_owned(),
        };
        if Instant::now() >= deadline {
            return Err(Error::new(Failure::CleanupFailed, remaining));
        }
        let _ = time::timeout(GRACE, source.terminate(run_id)).await;
        time::sleep(INTERVAL).await;
    }
}

fn check_space(path: &Path, min_free_space: u64) -> Result<(), Error> {
    let free = staging::free_space(path).map_err(|error| {
        Error::new(
            Failure::Staging,
            format!(
                "cannot check the free space for {}: {error}",
                path.display()
            ),
        )
    })?;
    if free < min_free_space {
        return Err(Error::new(
            Failure::StagingLimit,
            format!(
                "the staging filesystem has {free} bytes free, less than safety.min_free_space"
            ),
        ));
    }
    Ok(())
}

fn staging_error(path: &Path, error: std::io::Error) -> Error {
    if error.kind() == ErrorKind::StorageFull {
        return Error::new(
            Failure::StagingLimit,
            format!("the staging filesystem is full: {error}"),
        );
    }
    Error::new(
        Failure::Staging,
        format!("cannot write {}: {error}", path.display()),
    )
}

fn remove(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(Error::new(
            Failure::CleanupFailed,
            format!("cannot delete {}: {error}", path.display()),
        )),
        _ => Ok(()),
    }
}

/// Reports a cleanup failure together with the error that caused the cleanup.
fn after(cleanup: Error, error: Error) -> Error {
    Error::new(
        cleanup.kind,
        format!("{cleanup} after an earlier error: {error}"),
    )
}

fn cancelled() -> Error {
    Error::new(Failure::Cancelled, "the backup was cancelled")
}
