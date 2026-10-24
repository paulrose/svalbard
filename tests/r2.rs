//! Compatibility tests against a real Cloudflare R2 bucket. Run them in Docker with
//! `scripts/test-r2.sh`.
//!
//! Each backup goes under `svalbard-test/`, and each test removes what it wrote. A lifecycle rule
//! on the test buckets removes what a failed test leaves. The CLI tests restore with the native
//! clients and print one evidence line each.

mod common;

use std::{
    env, future,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{Client, presigning::PresigningConfig, types::ChecksumMode};
use common::{
    cli::{self, MYSQL_SECRETS, POSTGRESQL_SECRETS, Setup},
    hex, mysql, postgresql, staged, store,
};
use sha2::{Digest, Sha256};
use svalbard::{
    config::{self, DestinationSettings, Settings, SourceSettings, Target},
    staging::Run,
    storage::Failure,
};

fn var(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set: run scripts/test-r2.sh"))
}

fn bucket() -> String {
    var("SVALBARD_TEST_R2_BUCKET")
}

/// The destination of `run`, configured as a user would: the type, the endpoint and the bucket.
fn destination(run: &Run) -> config::Destination {
    let flags = Settings {
        source: SourceSettings {
            r#type: Some("postgresql".to_owned()),
            host: Some("db.example.com".to_owned()),
            database: Some("app".to_owned()),
            user: Some("backup".to_owned()),
            ..SourceSettings::default()
        },
        destination: DestinationSettings {
            r#type: Some("r2".to_owned()),
            bucket: Some(bucket()),
            prefix: Some(format!("svalbard-test/{}/", run.id)),
            endpoint: Some(var("SVALBARD_TEST_R2_ENDPOINT")),
            ..DestinationSettings::default()
        },
        ..Settings::default()
    };
    config::load(None, &flags, None)
        .expect("the configuration must be valid")
        .destination
}

/// A client with the same keys, for checking the bucket.
async fn client() -> Client {
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new("auto"))
        .load()
        .await;
    let config = aws_sdk_s3::config::Builder::from(&config)
        .endpoint_url(var("SVALBARD_TEST_R2_ENDPOINT"))
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

/// The keys of the objects under `prefix`.
async fn objects(client: &Client, prefix: &str) -> Vec<String> {
    let output = client
        .list_objects_v2()
        .bucket(bucket())
        .prefix(prefix)
        .send()
        .await
        .unwrap();
    output
        .contents()
        .iter()
        .filter_map(|object| object.key().map(str::to_owned))
        .collect()
}

/// The keys and IDs of the unfinished multipart uploads under `prefix`.
async fn uploads(client: &Client, prefix: &str) -> Vec<(String, String)> {
    let output = client
        .list_multipart_uploads()
        .bucket(bucket())
        .prefix(prefix)
        .send()
        .await
        .unwrap();
    output
        .uploads()
        .iter()
        .filter_map(|upload| Some((upload.key()?.to_owned(), upload.upload_id()?.to_owned())))
        .collect()
}

/// Removes what a test wrote under `prefix`.
async fn clean(client: &Client, prefix: &str) {
    for key in objects(client, prefix).await {
        let request = client.delete_object().bucket(bucket()).key(key);
        request.send().await.unwrap();
    }
    for (key, upload_id) in uploads(client, prefix).await {
        let request = client.abort_multipart_upload().bucket(bucket());
        request.key(key).upload_id(upload_id).send().await.unwrap();
    }
}

/// Downloads an object with curl, independently of Svalbard. curl reads the signed URL on
/// stdin, so neither the keys nor the URL appear in its arguments.
async fn download(client: &Client, key: &str) -> Vec<u8> {
    let request = client
        .get_object()
        .bucket(bucket())
        .key(key)
        .presigned(PresigningConfig::expires_in(Duration::from_secs(300)).unwrap())
        .await
        .unwrap();
    let mut curl = Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("curl must start");
    writeln!(curl.stdin.take().unwrap(), "url = \"{}\"", request.uri()).unwrap();
    let output = curl.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn dump_and_record_round_trip() {
    let client = client().await;

    // One request, and a multipart upload of three parts.
    for size in [1 << 20, 130 << 20] {
        let (dump, run) = staged(size);
        let destination = destination(&run);
        let stored = store(&destination, &dump, &run, future::pending())
            .await
            .expect("the upload must succeed");

        let data = download(&client, &stored.data).await;
        assert_eq!(data.len(), size);
        assert_eq!(hex(&Sha256::digest(&data)), dump.sha256);
        let record: serde_json::Value =
            serde_json::from_slice(&download(&client, &stored.record).await).unwrap();
        assert_eq!(record["data"], stored.data);
        assert_eq!(record["sha256"], dump.sha256);
        assert_eq!(record["size"], size);

        let head = client
            .head_object()
            .bucket(bucket())
            .key(&stored.data)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await
            .unwrap();
        // R2 keeps the SHA-256 of a single request, and the MD5 ETag of a multipart upload.
        if size == 1 << 20 {
            assert!(head.checksum_sha256().is_some(), "{head:?}");
        } else {
            assert!(
                head.e_tag().is_some_and(|tag| tag.ends_with("-3\"")),
                "{head:?}"
            );
        }
        assert_eq!(
            head.metadata().and_then(|metadata| metadata.get("sha256")),
            Some(&dump.sha256)
        );
        assert!(uploads(&client, &destination.prefix).await.is_empty());
        clean(&client, &destination.prefix).await;
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn dump_that_changed_after_staging_is_never_stored() {
    let client = client().await;

    // R2 checks the single request. Svalbard checks the parts before it completes the upload.
    for (size, reason) in [
        (1 << 20, "BadDigest"),
        (130 << 20, "changed after the dump"),
    ] {
        let (mut dump, run) = staged(size);
        dump.sha256 = hex(&Sha256::digest(b"other content"));
        let destination = destination(&run);
        let error = store(&destination, &dump, &run, future::pending())
            .await
            .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::Upload, "{error}");
        assert!(error.message.contains(reason), "{error}");
        assert!(objects(&client, &destination.prefix).await.is_empty());
        assert!(uploads(&client, &destination.prefix).await.is_empty());
        clean(&client, &destination.prefix).await;
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn cancellation_aborts_the_upload() {
    let client = client().await;
    let (dump, run) = staged(130 << 20);
    let destination = destination(&run);
    // Cancels as soon as R2 lists the multipart upload.
    let started = async { while uploads(&client, &destination.prefix).await.is_empty() {} };
    let error = store(&destination, &dump, &run, started)
        .await
        .expect_err("the upload must be cancelled before it completes");

    assert_eq!(error.kind, Failure::Cancelled, "{error}");
    assert!(uploads(&client, &destination.prefix).await.is_empty());
    assert!(objects(&client, &destination.prefix).await.is_empty());
    clean(&client, &destination.prefix).await;
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn bucket_outside_the_token_scope_is_access_denied() {
    let (dump, run) = staged(1 << 10);
    let mut destination = destination(&run);
    let Target::ObjectStore(bucket) = &mut destination.target else {
        panic!("an R2 destination is an object store");
    };
    bucket.bucket = format!("svalbard-denied-{}", &run.id[..16]);
    let error = store(&destination, &dump, &run, future::pending())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::AccessDenied, "{error}");
    run.remove().unwrap();
}

/// A new prefix under `svalbard-test/` for one CLI run.
fn cli_prefix(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("svalbard-test/{test}-{nanos}/")
}

/// The R2 destination for a CLI run under `prefix`.
fn cli_destination(prefix: &str) -> String {
    cli::r2(&var("SVALBARD_TEST_R2_ENDPOINT"), &bucket(), prefix)
}

/// The secrets that must not appear: the source's, and the R2 secret.
fn secrets<'a>(source: &[&'a str], r2: &'a str) -> Vec<&'a str> {
    [source, &[r2]].concat()
}

/// Checks the stored files against the CLI result, and returns the dump.
async fn check(client: &Client, prefix: &str, result: &serde_json::Value) -> Vec<u8> {
    assert_eq!(result["result"], "complete", "{result}");
    let (data, record) = (
        result["data"].as_str().unwrap(),
        result["record"].as_str().unwrap(),
    );
    let (mut stored, mut expected) = (objects(client, prefix).await, [data, record]);
    stored.sort();
    expected.sort();
    assert_eq!(stored, expected);
    assert!(uploads(client, prefix).await.is_empty());
    let dump = download(client, data).await;
    assert_eq!(result["size"], dump.len());
    assert_eq!(result["sha256"], hex(&Sha256::digest(&dump)));
    let record: serde_json::Value =
        serde_json::from_slice(&download(client, record).await).unwrap();
    assert_eq!(record["sha256"], result["sha256"]);
    dump
}

fn report(source: &str, result: &serde_json::Value) {
    println!(
        "matrix: {source} {} ({}) -> r2: restored {} bytes",
        result["server_version"].as_str().unwrap(),
        result["client_version"].as_str().unwrap(),
        result["size"]
    );
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn postgresql_backup_to_r2_restores() {
    let client = client().await;
    let prefix = cli_prefix("cli-postgresql");
    let mut fixture = postgresql::Fixture::create("r2_cli").await;
    fixture.add_larger().await;
    let setup = Setup::new(
        "r2-cli-postgresql",
        &cli::postgresql_source(&fixture),
        &cli_destination(&prefix),
        "",
    );
    let r2_secret = var("AWS_SECRET_ACCESS_KEY");
    let outcome = setup
        .run(
            postgresql::PASSWORD,
            &secrets(POSTGRESQL_SECRETS, &r2_secret),
        )
        .await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dump = check(&client, &prefix, &outcome.result).await;
    fixture.assert_restores(&dump).await;
    report("postgresql", &outcome.result);
    clean(&client, &prefix).await;
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn mysql_backup_to_r2_restores() {
    let client = client().await;
    let prefix = cli_prefix("cli-mysql");
    let mut fixture = mysql::Fixture::create("r2_cli").await;
    fixture.add_larger().await;
    let setup = Setup::new(
        "r2-cli-mysql",
        &cli::mysql_source(&fixture),
        &cli_destination(&prefix),
        "",
    );
    let r2_secret = var("AWS_SECRET_ACCESS_KEY");
    let outcome = setup
        .run(mysql::PASSWORD, &secrets(MYSQL_SECRETS, &r2_secret))
        .await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dump = check(&client, &prefix, &outcome.result).await;
    fixture.assert_restores(&dump).await;
    report("mysql", &outcome.result);
    clean(&client, &prefix).await;
}

#[tokio::test]
#[ignore = "needs R2: run scripts/test-r2.sh"]
async fn rejected_r2_secret_stores_nothing() {
    let client = client().await;
    let prefix = cli_prefix("cli-secret");
    let fixture = mysql::Fixture::create("r2_cli_secret").await;
    let setup = Setup::new(
        "r2-cli-secret",
        &cli::mysql_source(&fixture),
        &cli_destination(&prefix),
        "",
    );
    let (r2_secret, wrong) = (var("AWS_SECRET_ACCESS_KEY"), "0".repeat(64));
    let child = setup.spawn_with(mysql::PASSWORD, &[("AWS_SECRET_ACCESS_KEY", &wrong)]);
    let outcome = setup
        .finish(child, &[MYSQL_SECRETS, &[&r2_secret, &wrong]].concat())
        .await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "transfer", "{outcome:?}");
    assert_eq!(outcome.result["error"], "access_denied", "{outcome:?}");
    assert!(objects(&client, &prefix).await.is_empty());
}
