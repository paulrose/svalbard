//! The OpenSSH server, through the shell of its user `backup`.

use std::{
    env,
    path::PathBuf,
    process::{Command, Output},
};

pub fn var(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set: run scripts/test-integration.sh"))
}

/// The host and port of the server, for the test proxy.
pub fn upstream() -> String {
    format!("{}:22", var("SVALBARD_TEST_SSH_HOST"))
}

pub fn key(name: &str) -> PathBuf {
    PathBuf::from(var("SVALBARD_TEST_SSH_KEYS")).join(name)
}

/// Runs `script` in the shell of `backup`, independently of Svalbard.
pub fn remote(script: &str) -> Output {
    Command::new("ssh")
        .args([
            "-F",
            "none",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
        ])
        .args(["-o", "GlobalKnownHostsFile=/dev/null", "-o"])
        .arg(format!(
            "UserKnownHostsFile={}",
            key("known_hosts").display()
        ))
        .arg("-i")
        .arg(key("client"))
        .arg(format!("backup@{}", var("SVALBARD_TEST_SSH_HOST")))
        .args(["--", script])
        .output()
        .expect("ssh must start")
}

pub fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Runs `script` and returns its standard output.
pub fn shell(script: &str) -> String {
    let output = remote(script);
    assert!(
        output.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The bytes of a remote file.
pub fn download(path: &str) -> Vec<u8> {
    let output = remote(&format!("cat -- {}", quote(path)));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// A new directory `name` for one test, under `parent`.
pub fn directory(parent: &str, name: &str) -> String {
    let dir = format!("{parent}/{name}");
    shell(&format!("mkdir -- {}", quote(&dir)));
    dir
}

/// The names in `dir`, hidden ones included.
pub fn files(dir: &str) -> Vec<String> {
    let mut names: Vec<_> = shell(&format!("ls -A -- {}", quote(dir)))
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}
