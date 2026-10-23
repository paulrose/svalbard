//! Native client processes: the database dump clients and the OpenSSH clients.

use std::{env, process::Stdio};

use tokio::{
    io::AsyncReadExt,
    process::{ChildStderr, Command},
};

/// Bytes of client standard error to keep for the error message.
const STDERR_TAIL: usize = 4096;

/// A command for a native client. It runs without a shell, with only `PATH` from the
/// environment, so that inherited client options such as `PGOPTIONS` have no effect.
pub fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear().stdin(Stdio::null()).kill_on_drop(true);
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
}

/// Makes the client receive SIGTERM when Svalbard exits.
pub fn die_with_parent(command: &mut Command) {
    // The kernel sends the signal when the spawning thread exits, so spawn from a long-lived thread.
    #[cfg(target_os = "linux")]
    // SAFETY: the closure makes one prctl system call, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            Ok(rustix::process::set_parent_process_death_signal(Some(
                rustix::process::Signal::TERM,
            ))?)
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// Keeps the last [`STDERR_TAIL`] bytes of the client's standard error.
pub async fn tail(mut stderr: ChildStderr) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buffer = [0; 4096];
    while let Ok(n @ 1..) = stderr.read(&mut buffer).await {
        tail.extend_from_slice(&buffer[..n]);
        tail.drain(..tail.len().saturating_sub(STDERR_TAIL));
    }
    tail
}
