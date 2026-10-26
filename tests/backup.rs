//! The backup command end to end: the real binary against the servers in Docker. Run the tests
//! with `scripts/test-integration.sh`.
//!
//! Each test writes a configuration file with its own staging directory, runs
//! `svalbard backup --json`, and checks that no secret reaches the output.

mod common;

use std::{env, fs, path::Path};

use common::{
    Fault,
    cli::{
        self, MYSQL_SECRETS, POSTGRESQL_SECRETS, Setup, floci as s3, mysql_source,
        postgresql_source, ssh_destination, within,
    },
    floci, hex, mysql, postgresql, proxy, ssh,
};
use rustix::process::{Pid, Signal, kill_process};
use sha2::{Digest, Sha256};
use svalbard::config::Source;

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
async fn excluded_tables_appear_in_the_record_and_the_result() {
    let fixture = postgresql::Fixture::create("cli_exclude").await;
    let bucket = floci::bucket("cli-exclude", None).await;
    let source = Source {
        exclude_tables: vec!["public.events".to_owned()],
        ..fixture.source(postgresql::verify_full())
    };
    let setup = Setup::new("cli-exclude", &cli::source(&source), &s3(&bucket), "");
    let outcome = setup.run(postgresql::PASSWORD, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let result = &outcome.result;
    assert_eq!(
        result["excluded_tables"],
        serde_json::json!(["public.events"])
    );
    let record = floci::download(&bucket, result["record"].as_str().unwrap());
    let record: serde_json::Value = serde_json::from_slice(&record).unwrap();
    assert_eq!(record["excluded_tables"], result["excluded_tables"]);
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
        &ssh_destination("sftp", "127.0.0.1", proxy.address.port(), &dir, "client"),
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
        &ssh_destination("sftp", "127.0.0.1", proxy.address.port(), &dir, "client"),
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

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn sigint_during_the_transfer_cancels_and_cleans_up() {
    let fixture = postgresql::Fixture::create("cli_interrupt").await;
    let bucket = floci::bucket("cli-interrupt", None).await;
    let proxy = proxy(&floci::upstream(), Fault::StallAfter(16 << 10)).await;
    let endpoint = format!("http://{}", proxy.address);
    let setup = Setup::new(
        "cli-interrupt",
        &postgresql_source(&fixture),
        &cli::s3(&bucket, &endpoint),
        "",
    );
    let child = setup.spawn(postgresql::PASSWORD);
    within(proxy.stalled.notified()).await;
    let pid = Pid::from_raw(child.id().unwrap() as i32).unwrap();
    kill_process(pid, Signal::INT).unwrap();
    let outcome = setup.finish(child, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "transfer", "{outcome:?}");
    assert_eq!(outcome.result["error"], "cancelled", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
    assert!(floci::uploads(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn full_staging_filesystem_fails_without_leftovers() {
    let mut fixture = mysql::Fixture::create("cli_full").await;
    fixture.add_larger().await;
    let bucket = floci::bucket("cli-full", None).await;
    // A 256 KiB tmpfs, filled by the dump.
    let small = Setup::at(
        "cli-full",
        Path::new("/staging-small").to_owned(),
        &mysql_source(&fixture),
        &s3(&bucket),
        "min_free_space = \"0B\"\n",
    );
    let outcome = small.run(mysql::PASSWORD, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "dump", "{outcome:?}");
    assert_eq!(outcome.result["error"], "staging_limit", "{outcome:?}");
    assert!(
        outcome.result["message"]
            .as_str()
            .unwrap()
            .contains("is full"),
        "{outcome:?}"
    );
    let mut admin = fixture.client().await;
    assert!(fixture.backup_sessions(&mut admin).await.is_empty());

    // Not enough free space for the limit: the dump never starts.
    let limited = Setup::new(
        "cli-limit",
        &mysql_source(&fixture),
        &s3(&bucket),
        "min_free_space = \"1TiB\"\n",
    );
    let outcome = limited.run(mysql::PASSWORD, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "staging_limit", "{outcome:?}");
    let message = outcome.result["message"].as_str().unwrap();
    assert!(
        message.contains("less than safety.min_free_space"),
        "{outcome:?}"
    );
    assert!(floci::objects(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn rejected_s3_credentials_store_nothing() {
    let fixture = mysql::Fixture::create("cli_s3_key").await;
    let bucket = floci::bucket("cli-s3-key", None).await;
    let setup = Setup::new("cli-s3-key", &mysql_source(&fixture), &s3(&bucket), "");
    let secret = "not-the-floci-secret";
    let child = setup.spawn_with(
        mysql::PASSWORD,
        &[
            ("AWS_ACCESS_KEY_ID", "unknown"),
            ("AWS_SECRET_ACCESS_KEY", secret),
        ],
    );
    let outcome = setup
        .finish(child, &[MYSQL_SECRETS, &[secret]].concat())
        .await;

    // The S3 check only loads the credentials, so the bucket rejects them after the dump.
    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "transfer", "{outcome:?}");
    assert_eq!(outcome.result["error"], "access_denied", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn rejected_ssh_key_fails_before_the_dump() {
    let fixture = mysql::Fixture::create("cli_ssh_key").await;
    let dir = ssh::directory("/srv/backups", "cli-ssh-key");
    let host = ssh::var("SVALBARD_TEST_SSH_HOST");
    let setup = Setup::new(
        "cli-ssh-key",
        &mysql_source(&fixture),
        &ssh_destination("sftp", &host, 22, &dir, "stranger"),
        "",
    );
    let outcome = setup.run(mysql::PASSWORD, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "destination", "{outcome:?}");
    assert_eq!(outcome.result["error"], "access_denied", "{outcome:?}");
    assert!(!outcome.stderr.contains("dumping"), "{outcome:?}");
    assert!(ssh::files(&dir).is_empty());
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn broken_transfers_store_nothing() {
    let fixture = mysql::Fixture::create("cli_broken").await;
    // Every connection that sends more than 64 KiB closes: both attempts of the upload fail,
    // and the SSH check and cleanup, which send less, pass.
    let bucket = floci::bucket("cli-broken", None).await;
    let s3_proxy = proxy(&floci::upstream(), Fault::CloseEachAfter(64 << 10)).await;
    let endpoint = format!("http://{}", s3_proxy.address);
    let dir = ssh::directory("/srv/backups", "cli-broken");
    let ssh_proxy = proxy(&ssh::upstream(), Fault::CloseEachAfter(64 << 10)).await;
    for (name, destination) in [
        ("cli-broken-s3", cli::s3(&bucket, &endpoint)),
        (
            "cli-broken-sftp",
            ssh_destination(
                "sftp",
                "127.0.0.1",
                ssh_proxy.address.port(),
                &dir,
                "client",
            ),
        ),
    ] {
        let setup = Setup::new(name, &mysql_source(&fixture), &destination, "");
        let outcome = setup.run(mysql::PASSWORD, MYSQL_SECRETS).await;

        assert_eq!(outcome.code, Some(4), "{name}: {outcome:?}");
        assert_eq!(outcome.result["stage"], "transfer", "{name}: {outcome:?}");
        assert_eq!(outcome.result["error"], "upload", "{name}: {outcome:?}");
        assert!(
            outcome.stderr.contains("trying again"),
            "{name}: {outcome:?}"
        );
    }
    // A dump below 64 MiB goes in one request, so S3 has no unfinished upload either way.
    assert!(floci::objects(&bucket).await.is_empty());
    assert!(floci::uploads(&bucket).await.is_empty());
    assert!(ssh::files(&dir).is_empty());
}
