//! The backup command end to end: the real binary against the servers in Docker. Run the tests
//! with `scripts/test-integration.sh`.
//!
//! Each test writes a configuration file with its own staging directory, runs
//! `svalbard backup --json`, and checks that no secret reaches the output.

mod common;

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use common::{Fault, floci, hex, mysql, postgresql, proxy, ssh};
use rustix::process::{Pid, Signal, kill_process};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};

/// What one run of the CLI returned.
#[derive(Debug)]
struct Outcome {
    code: Option<i32>,
    result: serde_json::Value,
    stderr: String,
}

/// A test's configuration file and staging directory.
struct Setup {
    config: PathBuf,
    staging: PathBuf,
}

impl Setup {
    /// Writes `source` and `destination` with `safety` lines and a new, empty staging directory.
    fn new(name: &str, source: &str, destination: &str, safety: &str) -> Setup {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let staging = tmp.join(format!("{name}-staging"));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir(&staging).unwrap();
        let config = tmp.join(format!("{name}.toml"));
        let text = format!(
            "{source}\n{destination}\n[safety]\nstaging_dir = \"{}\"\n{safety}",
            staging.display()
        );
        fs::write(&config, text).unwrap();
        Setup { config, staging }
    }

    /// Starts `svalbard backup --json` with only the variables it needs.
    fn spawn(&self, password: &str) -> Child {
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
            "AWS_REGION",
            "AWS_EC2_METADATA_DISABLED",
        ] {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command.spawn().expect("the CLI must start")
    }

    /// Waits for the CLI. Fails if a part of `secrets` appears in its output, or if the run left
    /// files in the staging directory.
    async fn finish(&self, child: Child, secrets: &[&str]) -> Outcome {
        let output = child.wait_with_output().await.unwrap();
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

    async fn run(&self, password: &str, secrets: &[&str]) -> Outcome {
        self.finish(self.spawn(password), secrets).await
    }
}

/// The parts of the PostgreSQL password that must never appear. JSON escapes `\`, so the raw
/// password alone could miss a leak.
const POSTGRESQL_SECRETS: &[&str] = &[postgresql::PASSWORD, "pa:ss"];
const MYSQL_SECRETS: &[&str] = &[mysql::PASSWORD, "ss#w'o"];

fn postgresql_source(fixture: &postgresql::Fixture) -> String {
    let source = fixture.source(postgresql::verify_full());
    format!(
        "[source]\ntype = \"postgresql\"\nhost = \"{}\"\nport = {}\ndatabase = \"{}\"\n\
         user = \"{}\"\ntls_ca_file = \"{}\"\n",
        source.host,
        source.port,
        source.database,
        source.user,
        env::var("SVALBARD_TEST_POSTGRESQL_CA").unwrap()
    )
}

fn mysql_source(fixture: &mysql::Fixture) -> String {
    let source = fixture.source(mysql::verify_full());
    format!(
        "[source]\ntype = \"mysql\"\nhost = \"{}\"\nport = {}\ndatabase = \"{}\"\n\
         user = \"{}\"\ntls_ca_file = \"{}\"\n",
        source.host,
        source.port,
        source.database,
        source.user,
        env::var("SVALBARD_TEST_MYSQL_CA").unwrap()
    )
}

fn s3(bucket: &str) -> String {
    format!(
        "[destination]\ntype = \"s3\"\nbucket = \"{bucket}\"\nprefix = \"app/\"\n\
         endpoint = \"{}\"\n",
        floci::endpoint()
    )
}

/// SFTP to `dir`, through a proxy on `port`.
fn sftp(dir: &str, port: u16) -> String {
    format!(
        "[destination]\ntype = \"sftp\"\nhost = \"127.0.0.1\"\nport = {port}\nuser = \"backup\"\n\
         path = \"{dir}\"\nidentity_file = \"{}\"\nknown_hosts_file = \"{}\"\n",
        ssh::key("client").display(),
        ssh::key("known_hosts").display()
    )
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn postgresql_backup_to_s3_restores() {
    let fixture = postgresql::Fixture::create("cli_restore").await;
    let bucket = floci::bucket("cli-restore", None).await;
    let setup = Setup::new(
        "cli-restore",
        &postgresql_source(&fixture),
        &s3(&bucket),
        "",
    );
    let outcome = setup.run(postgresql::PASSWORD, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let result = &outcome.result;
    assert_eq!(result["result"], "complete", "{result}");
    let data = floci::download(&bucket, result["data"].as_str().unwrap());
    assert_eq!(result["size"], data.len());
    assert_eq!(result["sha256"], hex(&Sha256::digest(&data)));
    let record = floci::download(&bucket, result["record"].as_str().unwrap());
    let record: serde_json::Value = serde_json::from_slice(&record).unwrap();
    assert_eq!(record["sha256"], result["sha256"]);

    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli-restore.dump");
    fs::write(&path, &data).unwrap();
    let restored = fixture.restore(&path).await;
    assert_eq!(
        postgresql::fingerprint(&restored).await,
        postgresql::fingerprint(&fixture.client().await).await
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn failed_dump_stores_nothing() {
    let fixture = postgresql::Fixture::create("cli_password").await;
    let bucket = floci::bucket("cli-password", None).await;
    let setup = Setup::new(
        "cli-password",
        &postgresql_source(&fixture),
        &s3(&bucket),
        "",
    );
    let wrong = "wrong-pa:ss";
    let outcome = setup.run(wrong, &[wrong]).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "dump", "{outcome:?}");
    assert_eq!(outcome.result["error"], "preflight", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn denied_upload_stores_nothing() {
    let fixture = postgresql::Fixture::create("cli_denied").await;
    let bucket = floci::bucket("cli-denied", Some(("s3:PutObject", "*"))).await;
    let setup = Setup::new("cli-denied", &postgresql_source(&fixture), &s3(&bucket), "");
    let outcome = setup.run(postgresql::PASSWORD, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "transfer", "{outcome:?}");
    assert_eq!(outcome.result["error"], "access_denied", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn denied_record_leaves_no_completion_record() {
    let fixture = postgresql::Fixture::create("cli_record").await;
    let bucket = floci::bucket("cli-record", Some(("s3:PutObject", "*.json"))).await;
    let setup = Setup::new("cli-record", &postgresql_source(&fixture), &s3(&bucket), "");
    let outcome = setup.run(postgresql::PASSWORD, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "access_denied", "{outcome:?}");
    let objects = floci::objects(&bucket).await;
    assert_eq!(objects.len(), 1, "{objects:?}");
    assert!(objects[0].ends_with(".dump"), "{objects:?}");
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn sigterm_during_the_dump_cancels_and_cleans_up() {
    let fixture = postgresql::Fixture::create("cli_cancel").await;
    let bucket = floci::bucket("cli-cancel", None).await;
    // Longer than the test, so that only the signal ends the wait.
    let setup = Setup::new(
        "cli-cancel",
        &postgresql_source(&fixture),
        &s3(&bucket),
        "lock_timeout = \"5min\"\n",
    );
    let _lock = fixture.hold_stall_lock().await;
    let child = setup.spawn(postgresql::PASSWORD);
    postgresql::stalled(&fixture.client().await).await;
    let pid = Pid::from_raw(child.id().unwrap() as i32).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    let outcome = setup.finish(child, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "cancelled", "{outcome:?}");
    assert_eq!(fixture.backup_sessions().await, 0);
    assert!(floci::objects(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn mysql_backup_to_sftp_retries_a_dropped_transfer() {
    let fixture = mysql::Fixture::create("cli_retry").await;
    let dir = ssh::directory("/srv/backups", "cli-retry");
    // The destination check passes. The upload loses its connection, and the retry connects
    // anew.
    let proxy = proxy(&ssh::upstream(), Fault::CloseAfter(64 << 10)).await;
    let setup = Setup::new(
        "cli-retry",
        &mysql_source(&fixture),
        &sftp(&dir, proxy.address.port()),
        "",
    );
    let outcome = setup.run(mysql::PASSWORD, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("trying again"), "{outcome:?}");
    let result = &outcome.result;
    assert_eq!(
        ssh::files(&dir),
        [
            result["record"].as_str().unwrap(),
            result["data"].as_str().unwrap()
        ]
    );
    let data = ssh::download(&format!("{dir}/{}", result["data"].as_str().unwrap()));
    assert_eq!(result["sha256"], hex(&Sha256::digest(&data)));
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn failed_remote_cleanup_exits_with_5() {
    let fixture = mysql::Fixture::create("cli_cleanup").await;
    let dir = ssh::directory("/srv/backups", "cli-cleanup");
    let proxy = proxy(&ssh::upstream(), Fault::StallThenRefuse(64 << 10)).await;
    let setup = Setup::new(
        "cli-cleanup",
        &mysql_source(&fixture),
        &sftp(&dir, proxy.address.port()),
        "",
    );
    let child = setup.spawn(mysql::PASSWORD);
    proxy.stalled.notified().await;
    let pid = Pid::from_raw(child.id().unwrap() as i32).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    let outcome = setup.finish(child, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(5), "{outcome:?}");
    assert_eq!(outcome.result["error"], "cleanup_failed", "{outcome:?}");
    let files = ssh::files(&dir);
    assert!(
        files.len() == 1 && files[0].ends_with(".partial"),
        "{files:?}"
    );
}
