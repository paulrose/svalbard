//! Tests against an OpenSSH server in Docker. Run them with `scripts/test-integration.sh`.
//!
//! Each test writes to its own directory, which it creates through the shell of `backup`.

mod common;

use std::{
    env, future,
    path::PathBuf,
    process::{Command, Output},
    time::Duration,
};

use common::{Fault, hex, proxy, staged, store};
use sha2::{Digest, Sha256};
use svalbard::{
    config::{self, SshMode, Target},
    staging::Run,
    storage::Failure,
};

const MODES: [SshMode; 2] = [SshMode::Sftp, SshMode::Scp];

fn var(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set: run scripts/test-integration.sh"))
}

fn key(name: &str) -> PathBuf {
    PathBuf::from(var("SVALBARD_TEST_SSH_KEYS")).join(name)
}

/// `backup` on the server, with the authorized key and the right host key.
fn server(mode: SshMode, path: &str) -> config::Ssh {
    config::Ssh {
        mode,
        host: var("SVALBARD_TEST_SSH_HOST"),
        port: 22,
        user: "backup".to_owned(),
        path: path.to_owned(),
        identity_file: Some(key("client")),
        known_hosts_file: Some(key("known_hosts")),
    }
}

/// The same server, through a test proxy.
async fn proxied(mode: SshMode, path: &str, fault: Fault) -> (config::Ssh, common::Proxy) {
    let upstream = format!("{}:22", var("SVALBARD_TEST_SSH_HOST"));
    let proxy = proxy(&upstream, fault).await;
    let server = config::Ssh {
        host: "127.0.0.1".to_owned(),
        port: proxy.address.port(),
        ..server(mode, path)
    };
    (server, proxy)
}

fn destination(server: config::Ssh) -> config::Destination {
    config::Destination {
        target: Target::Ssh(server),
        prefix: String::new(),
        upload_timeout: Duration::from_secs(60),
    }
}

/// Runs `script` in the shell of `backup`, independently of Svalbard.
fn remote(script: &str) -> Output {
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

fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Runs `script` and returns its standard output.
fn shell(script: &str) -> String {
    let output = remote(script);
    assert!(
        output.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The bytes of a remote file.
fn download(path: &str) -> Vec<u8> {
    let output = remote(&format!("cat -- {}", quote(path)));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// A new directory for one test, under `parent`.
fn directory(parent: &str, run: &Run) -> String {
    let dir = format!("{parent}/{}", run.id);
    shell(&format!("mkdir -- {}", quote(&dir)));
    dir
}

/// The names in `dir`, hidden ones included.
fn files(dir: &str) -> Vec<String> {
    let mut names: Vec<_> = shell(&format!("ls -A -- {}", quote(dir)))
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn dump_and_record_round_trip() {
    for mode in MODES {
        let (dump, run) = staged(1 << 20);
        let dir = directory("/srv/backups", &run);
        let destination = destination(server(mode, &dir));

        // A second upload of the same run replaces the files.
        for _ in 0..2 {
            let stored = store(&destination, &dump, &run, future::pending())
                .await
                .unwrap_or_else(|error| panic!("{mode:?}: {error}"));

            assert_eq!(
                files(&dir),
                [stored.record.as_str(), stored.data.as_str()],
                "{mode:?}"
            );
            let data = download(&format!("{dir}/{}", stored.data));
            assert_eq!(hex(&Sha256::digest(&data)), dump.sha256);
            let record: serde_json::Value =
                serde_json::from_slice(&download(&format!("{dir}/{}", stored.record))).unwrap();
            assert_eq!(record["data"], stored.data);
            assert_eq!(record["sha256"], dump.sha256);
            assert_eq!(record["size"], 1 << 20);
            let modes = shell(&format!(
                "cd -- {} && stat -c %a {} {}",
                quote(&dir),
                stored.data,
                stored.record
            ));
            assert_eq!(modes, "600\n600\n", "{mode:?}");
        }
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn agent_key_authenticates() {
    let added = Command::new("ssh-add")
        .arg("-q")
        .arg(key("client"))
        .status()
        .unwrap();
    assert!(added.success());
    let (dump, run) = staged(1 << 10);
    let dir = directory("/srv/backups", &run);
    let server = config::Ssh {
        identity_file: None,
        ..server(SshMode::Sftp, &dir)
    };

    store(&destination(server), &dump, &run, future::pending())
        .await
        .expect("the agent key must authenticate");
    assert_eq!(files(&dir).len(), 2);
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn wrong_or_missing_host_key_is_rejected() {
    for mode in MODES {
        for known_hosts in ["known_hosts_wrong", "known_hosts_empty"] {
            let (dump, run) = staged(1 << 10);
            let server = config::Ssh {
                known_hosts_file: Some(key(known_hosts)),
                ..server(mode, "/srv/backups")
            };
            let error = store(&destination(server), &dump, &run, future::pending())
                .await
                .expect_err("the upload must fail");

            assert_eq!(
                error.kind,
                Failure::HostKeyRejected,
                "{mode:?} {known_hosts}: {error}"
            );
            run.remove().unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn unknown_client_key_is_access_denied() {
    for mode in MODES {
        let (dump, run) = staged(1 << 10);
        let server = config::Ssh {
            identity_file: Some(key("stranger")),
            ..server(mode, "/srv/backups")
        };
        let error = store(&destination(server), &dump, &run, future::pending())
            .await
            .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::AccessDenied, "{mode:?}: {error}");
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn denied_write_is_access_denied() {
    for mode in MODES {
        let (dump, run) = staged(1 << 10);
        let error = store(
            &destination(server(mode, "/srv/readonly")),
            &dump,
            &run,
            future::pending(),
        )
        .await
        .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::AccessDenied, "{mode:?}: {error}");
        assert!(files("/srv/readonly").is_empty());
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn unusual_paths_stay_literal() {
    for mode in MODES {
        for name in [
            "with space",
            "it's",
            "$(touch /tmp/pwned)",
            "*?[x]",
            "ünïcode",
        ] {
            let (dump, run) = staged(1 << 10);
            let dir = directory(&format!("/srv/backups/{name}"), &run);
            store(
                &destination(server(mode, &dir)),
                &dump,
                &run,
                future::pending(),
            )
            .await
            .unwrap_or_else(|error| panic!("{mode:?} {name}: {error}"));

            assert_eq!(files(&dir).len(), 2, "{mode:?} {name}");
            run.remove().unwrap();
        }
    }
    assert_eq!(shell("test -e /tmp/pwned && echo ran || true"), "");
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn cancellation_removes_the_partial_file() {
    for mode in MODES {
        let (dump, run) = staged(32 << 20);
        let dir = directory("/srv/backups", &run);
        let (server, proxy) = proxied(mode, &dir, Fault::StallAfter(8 << 20)).await;
        let error = store(&destination(server), &dump, &run, proxy.stalled.notified())
            .await
            .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::Cancelled, "{mode:?}: {error}");
        assert!(files(&dir).is_empty(), "{mode:?}: {:?}", files(&dir));
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn disconnect_fails_and_removes_the_partial_file() {
    for mode in MODES {
        let (dump, run) = staged(32 << 20);
        let dir = directory("/srv/backups", &run);
        let (server, _proxy) = proxied(mode, &dir, Fault::CloseAfter(8 << 20)).await;
        let error = store(&destination(server), &dump, &run, future::pending())
            .await
            .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::Upload, "{mode:?}: {error}");
        assert!(files(&dir).is_empty(), "{mode:?}: {:?}", files(&dir));
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn failed_cleanup_reports_the_partial_file() {
    for mode in MODES {
        let (dump, run) = staged(32 << 20);
        let dir = directory("/srv/backups", &run);
        let (server, proxy) = proxied(mode, &dir, Fault::StallThenRefuse(8 << 20)).await;
        let error = store(&destination(server), &dump, &run, proxy.stalled.notified())
            .await
            .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::CleanupFailed, "{mode:?}: {error}");
        assert!(error.message.contains("cancelled"), "{mode:?}: {error}");
        let files = files(&dir);
        assert!(
            files.len() == 1 && files[0].ends_with(".partial"),
            "{mode:?}: {files:?}"
        );
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs SSH: run scripts/test-integration.sh"]
async fn scp_without_a_shell_names_the_requirement() {
    let (dump, run) = staged(1 << 10);
    let server = config::Ssh {
        user: "sftponly".to_owned(),
        ..server(SshMode::Scp, "/srv/backups")
    };
    let error = store(&destination(server), &dump, &run, future::pending())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::Upload, "{error}");
    assert!(error.message.contains("POSIX login shell"), "{error}");
    run.remove().unwrap();
}
