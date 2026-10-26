//! `svalbard backup`, run as a user would: a configuration file, the password in the
//! environment, and JSON on stdout.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{
    process::{Child, Command},
    time,
};

use svalbard::config::{Kind, Source, Tls};

use super::{floci, mysql, postgresql, ssh};

/// The longest a test lets one CLI run take.
const LIMIT: Duration = Duration::from_secs(300);

/// The parts of the PostgreSQL password that must never appear. JSON escapes `\`, so the raw
/// password alone could miss a leak.
pub const POSTGRESQL_SECRETS: &[&str] = &[postgresql::PASSWORD, "pa:ss"];
pub const MYSQL_SECRETS: &[&str] = &[mysql::PASSWORD, "ss#w'o"];

/// What one run of the CLI returned.
#[derive(Debug)]
pub struct Outcome {
    pub code: Option<i32>,
    pub result: serde_json::Value,
    pub stderr: String,
}

/// A test's configuration file and staging directory.
pub struct Setup {
    pub config: PathBuf,
    pub staging: PathBuf,
}

impl Setup {
    /// Writes `source` and `destination` with `safety` lines and a new, empty staging directory.
    pub fn new(name: &str, source: &str, destination: &str, safety: &str) -> Setup {
        let staging = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-staging"));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir(&staging).unwrap();
        Setup::at(name, staging, source, destination, safety)
    }

    /// Like [`Setup::new`], with an existing, empty staging directory.
    pub fn at(
        name: &str,
        staging: PathBuf,
        source: &str,
        destination: &str,
        safety: &str,
    ) -> Setup {
        let config = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.toml"));
        let text = format!(
            "{source}\n{destination}\n[safety]\nstaging_dir = \"{}\"\n{safety}",
            staging.display()
        );
        fs::write(&config, text).unwrap();
        Setup { config, staging }
    }

    /// Starts `svalbard backup --json` with only the variables it needs.
    pub fn spawn(&self, password: &str) -> Child {
        self.spawn_with(password, &[])
    }

    /// Like [`Setup::spawn`], with `env` set or replaced.
    pub fn spawn_with(&self, password: &str, env: &[(&str, &str)]) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_svalbard"));
        command
            .args(["backup", "--json", "--config"])
            .arg(&self.config)
            .env_clear()
            .env("SVALBARD_SOURCE_PASSWORD", password)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in [
            "PATH",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_REGION",
            "AWS_EC2_METADATA_DISABLED",
        ] {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command.envs(env.iter().copied());
        command.spawn().expect("the CLI must start")
    }

    /// Waits for the CLI. Fails if a part of `secrets` appears in its output, or if the run left
    /// files in the staging directory.
    pub async fn finish(&self, child: Child, secrets: &[&str]) -> Outcome {
        let output = time::timeout(LIMIT, child.wait_with_output())
            .await
            .expect("the CLI must finish")
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        for secret in secrets {
            assert!(!stdout.contains(secret), "{secret:?} in {stdout}");
            assert!(!stderr.contains(secret), "{secret:?} in {stderr}");
        }
        let left: Vec<_> = fs::read_dir(&self.staging).unwrap().collect();
        assert!(left.is_empty(), "{left:?}: {stderr}");
        Outcome {
            code: output.status.code(),
            result: serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("{stdout}")),
            stderr,
        }
    }

    pub async fn run(&self, password: &str, secrets: &[&str]) -> Outcome {
        self.finish(self.spawn(password), secrets).await
    }
}

/// Waits for `future`, or fails the test after a minute.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    time::timeout(Duration::from_secs(60), future)
        .await
        .expect("the test waited for longer than a minute")
}

pub fn postgresql_source(fixture: &postgresql::Fixture) -> String {
    source(&fixture.source(postgresql::verify_full()))
}

pub fn mysql_source(fixture: &mysql::Fixture) -> String {
    source(&fixture.source(mysql::verify_full()))
}

/// The `[source]` section for `source`, which must use verify-full with a CA file. The
/// password comes from the environment.
pub fn source(source: &Source) -> String {
    let kind = match source.kind {
        Kind::Postgresql => "postgresql",
        Kind::Mysql => "mysql",
    };
    let Tls::VerifyFull(Some(ca_file)) = &source.tls else {
        panic!("the tests use verify-full with the test CA");
    };
    // A JSON array of plain strings is also a TOML array.
    format!(
        "[source]\ntype = \"{kind}\"\nhost = \"{}\"\nport = {}\ndatabase = \"{}\"\n\
         user = \"{}\"\ntls_ca_file = \"{}\"\nexclude_tables = {}\n",
        source.host,
        source.port,
        source.database,
        source.user,
        ca_file.display(),
        serde_json::to_string(&source.exclude_tables).unwrap()
    )
}

/// The child processes of `pid`, from /proc.
pub fn children(pid: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let child: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(format!("/proc/{child}/stat")).ok()?;
            // The command name in parentheses can hold spaces, so read after it.
            let parent: u32 = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            (parent == pid).then_some(child)
        })
        .collect()
}

/// Floci, at `endpoint`.
pub fn s3(bucket: &str, endpoint: &str) -> String {
    format!(
        "[destination]\ntype = \"s3\"\nbucket = \"{bucket}\"\nprefix = \"app/\"\n\
         endpoint = \"{endpoint}\"\n"
    )
}

/// Floci, directly.
pub fn floci(bucket: &str) -> String {
    s3(bucket, &floci::endpoint())
}

pub fn r2(endpoint: &str, bucket: &str, prefix: &str) -> String {
    format!(
        "[destination]\ntype = \"r2\"\nbucket = \"{bucket}\"\nprefix = \"{prefix}\"\n\
         endpoint = \"{endpoint}\"\n"
    )
}

/// `mode` (`sftp` or `scp`) to `dir`, as `backup` with the `key` client key.
pub fn ssh_destination(mode: &str, host: &str, port: u16, dir: &str, key: &str) -> String {
    format!(
        "[destination]\ntype = \"{mode}\"\nhost = \"{host}\"\nport = {port}\nuser = \"backup\"\n\
         path = \"{dir}\"\nidentity_file = \"{}\"\nknown_hosts_file = \"{}\"\n",
        ssh::key(key).display(),
        ssh::key("known_hosts").display()
    )
}
