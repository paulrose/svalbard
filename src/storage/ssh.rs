//! SSH servers, through the OpenSSH clients: SFTP, or the legacy SCP protocol. See "SSH" in
//! README.md.

use std::{
    env,
    fs::File,
    io::Read,
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Child,
    time::{self, Instant},
};

use super::{Destination, Error, Failure, Staged};
use crate::{
    config::{self, SshMode},
    process,
};

/// The host name in client commands. `HostName` holds the real host, so that an IPv6 address
/// needs no brackets.
const ALIAS: &str = "svalbard-destination";
/// Time for a stopped client and its `ssh` to exit.
const GRACE: Duration = Duration::from_secs(5);
/// The sftp options: batch commands on standard input. Not `-q`, which also hides the errors of
/// `ssh`.
const SFTP: &[&str] = &["-b", "-"];

/// A directory on an SSH server. Svalbard uploads to a temporary name and renames the file when
/// the size matches.
pub struct Ssh {
    config: config::Ssh,
    pending: Option<Pending>,
}

/// An upload that has not finished.
struct Pending {
    key: String,
    /// The remote directory of the file.
    dir: String,
    temporary: String,
    /// The process group of the last client.
    group: Option<Pid>,
}

/// A client that stops with its `ssh` when it is dropped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        // Tokio forgets the ID once the client was reaped, so only a running client is stopped.
        if let Some(group) = self.0.id().and_then(|id| Pid::from_raw(id as i32)) {
            let _ = kill_process_group(group, Signal::TERM);
        }
    }
}

impl Ssh {
    /// Makes no connection.
    pub fn new(config: config::Ssh) -> Ssh {
        Ssh {
            config,
            pending: None,
        }
    }

    /// Uploads `local` from `dir` to the temporary name. Fails unless the remote file has
    /// `size` bytes.
    async fn upload(&mut self, local: &str, dir: &Path, size: u64) -> Result<(), Error> {
        let pending = self.pending.as_ref().expect("an upload is pending");
        let (remote, temporary) = (pending.dir.clone(), pending.temporary.clone());
        match self.config.mode {
            SshMode::Sftp => {
                let batch = format!(
                    "@cd {}\n@put -f {local} {temporary}\n@ls -ln {temporary}\n",
                    sftp_quote(&remote)
                );
                let (stdout, stderr) = self
                    .run("sftp", SFTP, &["--", ALIAS], &batch, Some(dir), true)
                    .await?;
                // sftp only warns when the server cannot sync the file.
                if stderr.contains("fsync") || stderr.contains("Couldn't sync") {
                    return Err(Error::new(
                        Failure::Upload,
                        format!("the server did not sync {temporary}: {}", stderr.trim()),
                    ));
                }
                let listed = stdout
                    .lines()
                    .rev()
                    .find_map(|line| line.split_whitespace().nth(4)?.parse::<u64>().ok());
                if listed != Some(size) {
                    return Err(Error::new(
                        Failure::Upload,
                        format!(
                            "sftp listed {temporary} with {listed:?} bytes, not {size}: {}",
                            stdout.trim()
                        ),
                    ));
                }
            }
            // SCP announces the size, and the server writes exactly that many bytes or fails.
            // The finalize script checks the size too.
            SshMode::Scp => {
                let target = format!("{ALIAS}:{}/{temporary}", shell_quote(&remote));
                self.run("scp", &["-O"], &["--", local, &target], "", Some(dir), true)
                    .await?;
            }
        }
        Ok(())
    }

    /// Renames the temporary file to `name`. For SCP, only if the remote file has `size` bytes.
    async fn finalize(&mut self, size: u64, name: &str) -> Result<(), Error> {
        let pending = self.pending.as_ref().expect("an upload is pending");
        let (remote, temporary) = (pending.dir.clone(), pending.temporary.clone());
        match self.config.mode {
            SshMode::Sftp => {
                let batch = format!("@cd {}\n@rename {temporary} {name}\n", sftp_quote(&remote));
                self.run("sftp", SFTP, &["--", ALIAS], &batch, None, false)
                    .await?;
            }
            SshMode::Scp => {
                let script = format!(
                    "cd -- {} && size=$(wc -c < {temporary}) && if [ $size -eq {size} ]; then \
                     mv -f -- {temporary} {name}; else echo \"{temporary} has $size bytes on \
                     the server, not {size}\" >&2; exit 1; fi",
                    shell_quote(&remote)
                );
                self.run("ssh", &[], &["--", ALIAS, &script], "", None, false)
                    .await?;
            }
        }
        Ok(())
    }

    /// Runs an OpenSSH client and returns its standard output and error. `options` go before
    /// the shared options, and `operands` after them. If `first` and the client failed before
    /// a session, nothing remains to clean up.
    async fn run(
        &mut self,
        program: &str,
        options: &[&str],
        operands: &[&str],
        input: &str,
        dir: Option<&Path>,
        first: bool,
    ) -> Result<(String, String), Error> {
        let mut command = process::command(program);
        command
            .args(options)
            .args(self.options())
            .args(operands)
            .stdin(if input.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A terminal's Ctrl-C reaches Svalbard, which stops the client itself.
            .process_group(0);
        if let Some(socket) = env::var_os("SSH_AUTH_SOCK") {
            command.env("SSH_AUTH_SOCK", socket);
        }
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        process::die_with_parent(&mut command);
        let mut child = command.spawn().map_err(|error| {
            Error::new(Failure::Upload, format!("cannot start {program}: {error}"))
        })?;
        if let Some(pending) = &mut self.pending {
            pending.group = child.id().and_then(|id| Pid::from_raw(id as i32));
        }
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let mut running = Running(child);
        let write = async {
            // A client that fails early closes its input. Its status reports the failure.
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(input.as_bytes()).await;
            }
        };
        let read = async {
            let mut text = String::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_string(&mut text).await;
            }
            text
        };
        let stderr = process::tail(stderr.expect("stderr is piped"));
        let (status, (), stdout, stderr) = tokio::join!(running.0.wait(), write, read, stderr);
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        match status {
            Ok(status) if status.success() => Ok((stdout, stderr)),
            status => {
                if first && before_session(&stderr) {
                    self.pending = None;
                }
                Err(failure(self.config.mode, program, status, &stderr))
            }
        }
    }

    /// The options of every client: strict host keys, no prompts, and nothing from
    /// `ssh_config`.
    fn options(&self) -> Vec<String> {
        let config = &self.config;
        let mut options = vec![
            "BatchMode=yes".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
            "UpdateHostKeys=no".to_owned(),
            "CheckHostIP=no".to_owned(),
            "PasswordAuthentication=no".to_owned(),
            "KbdInteractiveAuthentication=no".to_owned(),
            "LogLevel=ERROR".to_owned(),
            "ConnectTimeout=10".to_owned(),
            "ServerAliveInterval=15".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
            format!("HostName={}", config.host),
            format!("Port={}", config.port),
            format!("User={}", config.user),
        ];
        if let Some(file) = &config.known_hosts_file {
            options.push(format!("UserKnownHostsFile=\"{}\"", openssh_path(file)));
            options.push("GlobalKnownHostsFile=/dev/null".to_owned());
        }
        if let Some(file) = &config.identity_file {
            options.push(format!("IdentityFile=\"{}\"", openssh_path(file)));
            options.push("IdentitiesOnly=yes".to_owned());
            options.push("IdentityAgent=none".to_owned());
        }
        let mut args = vec!["-F".to_owned(), "none".to_owned()];
        for option in options {
            args.extend(["-o".to_owned(), option]);
        }
        args
    }
}

impl Destination for Ssh {
    async fn put(&mut self, key: &str, file: &Staged<'_>) -> Result<(), Error> {
        let path = self.config.path.trim_end_matches('/');
        let (dir, name) = match key.rsplit_once('/') {
            Some((dir, name)) => (format!("{path}/{dir}"), name),
            None => (path.to_owned(), key),
        };
        // Random for each attempt, so that a retry never writes to the file of an old session.
        let temporary = format!(".{name}.{}.partial", random()?);
        let (Some(local), Some(local_dir)) = (
            file.path.file_name().and_then(|name| name.to_str()),
            file.path.parent(),
        ) else {
            return Err(Error::new(
                Failure::Upload,
                format!("cannot upload {}", file.path.display()),
            ));
        };
        self.pending = Some(Pending {
            key: key.to_owned(),
            dir,
            temporary: temporary.clone(),
            group: None,
        });
        self.upload(local, local_dir, file.size).await?;
        self.finalize(file.size, name).await?;
        self.pending = None;
        Ok(())
    }

    async fn abort(&mut self, key: &str) -> Result<(), Error> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if pending.key != key {
            return Ok(());
        }
        // Dropping the upload stopped its client. Its `ssh` must end before the removal, so
        // that no request of the old session arrives after it.
        if let Some(group) = pending.group {
            let deadline = Instant::now() + GRACE;
            while test_kill_process_group(group).is_ok() && Instant::now() < deadline {
                time::sleep(Duration::from_millis(50)).await;
            }
        }
        let (dir, temporary) = (&pending.dir, &pending.temporary);
        let result = match self.config.mode {
            SshMode::Sftp => {
                let batch = format!("@cd {}\n@rm {temporary}\n", sftp_quote(dir));
                self.run("sftp", SFTP, &["--", ALIAS], &batch, None, false)
                    .await
            }
            SshMode::Scp => {
                let script = format!("cd -- {} && rm -f -- {temporary}", shell_quote(dir));
                self.run("ssh", &[], &["--", ALIAS, &script], "", None, false)
                    .await
            }
        };
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.message.contains("No such file") => Ok(()),
            Err(error) => Err(Error::new(
                Failure::CleanupFailed,
                format!("{dir}/{temporary} may remain: {error}"),
            )),
        }
    }
}

/// Whether the client failed before it had a session, from the stable messages of OpenSSH.
fn before_session(stderr: &str) -> bool {
    [
        "Host key verification failed",
        "Permission denied (",
        "Could not resolve hostname",
        "Connection refused",
        "Connection timed out",
        "No route to host",
        "Network is unreachable",
        "subsystem request failed",
        "illegal option -- O",
        "unknown option -- O",
        "This service allows sftp connections only",
    ]
    .iter()
    .any(|message| stderr.contains(message))
}

/// Classifies a failed client from the stable messages of OpenSSH.
fn failure(
    mode: SshMode,
    program: &str,
    status: std::io::Result<ExitStatus>,
    stderr: &str,
) -> Error {
    let status = match status {
        Ok(status) => status.to_string(),
        Err(error) => error.to_string(),
    };
    let detail = match stderr.trim() {
        "" => status,
        text => text.trim_end_matches('.').to_owned(),
    };
    let (kind, hint) = if stderr.contains("Host key verification failed") {
        (
            Failure::HostKeyRejected,
            " The host key is missing from the known_hosts file or does not match.",
        )
    } else if stderr.contains("Permission denied") {
        (Failure::AccessDenied, "")
    } else if stderr.contains("subsystem request failed") {
        (Failure::Upload, " The server offers no SFTP subsystem.")
    } else if stderr.contains("option -- O") {
        (
            Failure::Upload,
            " SCP needs an scp client that accepts -O, such as OpenSSH 9.0 or newer.",
        )
    } else if mode == SshMode::Scp
        && ![
            "Connection closed",
            "lost connection",
            "Connection reset",
            "Broken pipe",
        ]
        .iter()
        .any(|message| stderr.contains(message))
    {
        (
            Failure::Upload,
            " SCP needs a POSIX login shell that prints nothing, with scp, mv, rm and wc.",
        )
    } else {
        (Failure::Upload, "")
    };
    Error::new(kind, format!("{program} failed: {detail}.{hint}"))
}

/// A path for an sftp command. The configuration rejects `"` and `\`, and sftp escapes glob
/// characters inside quotes itself.
fn sftp_quote(path: &str) -> String {
    format!("\"{path}\"")
}

/// A path for a POSIX shell.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// A path for an OpenSSH option, which expands `%` tokens.
fn openssh_path(path: &Path) -> String {
    path.to_string_lossy().replace('%', "%%")
}

/// 16 random hexadecimal digits.
fn random() -> Result<String, Error> {
    let mut bytes = [0; 8];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| {
            Error::new(
                Failure::Upload,
                format!("cannot read /dev/urandom: {error}"),
            )
        })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_keeps_every_character_literal() {
        assert_eq!(shell_quote("/srv/it's $(x)"), r"'/srv/it'\''s $(x)'");
    }
}
