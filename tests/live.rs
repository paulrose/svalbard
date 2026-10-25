//! Compatibility tests against a real bucket in Cloudflare R2 or Amazon S3. Run them in Docker
//! with `scripts/test-live.sh r2` or `scripts/test-live.sh aws`.
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
use aws_smithy_types::base64;
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
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set: run scripts/test-live.sh"))
}

#[derive(Clone, Copy, PartialEq)]
enum Service {
    R2,
    Aws,
}

fn service() -> Service {
    match var("SVALBARD_TEST_LIVE").as_str() {
        "r2" => Service::R2,
        "aws" => Service::Aws,
        other => panic!("SVALBARD_TEST_LIVE is `{other}`: use r2 or aws"),
    }
}

fn bucket() -> String {
    match service() {
        Service::R2 => var("SVALBARD_TEST_R2_BUCKET"),
        Service::Aws => var("SVALBARD_TEST_AWS_BUCKET"),
    }
}

/// The destination of `run`, configured as a user would. For S3, the region comes from
/// `AWS_REGION`.
fn destination(run: &Run) -> config::Destination {
    let (r#type, endpoint) = match service() {
        Service::R2 => ("r2", Some(var("SVALBARD_TEST_R2_ENDPOINT"))),
        Service::Aws => ("s3", None),
    };
    let flags = Settings {
        source: SourceSettings {
            r#type: Some("postgresql".to_owned()),
            host: Some("db.example.com".to_owned()),
            database: Some("app".to_owned()),
            user: Some("backup".to_owned()),
            ..SourceSettings::default()
        },
        destination: DestinationSettings {
            r#type: Some(r#type.to_owned()),
            bucket: Some(bucket()),
            prefix: Some(format!("svalbard-test/{}/", run.id)),
            endpoint,
            ..DestinationSettings::default()
        },
        ..Settings::default()
    };
    config::load(None, &flags, None)
        .expect("the configuration must be valid")
        .destination
}

/// A client with the same credentials, for checking the bucket.
async fn client() -> Client {
    let defaults = aws_config::defaults(BehaviorVersion::latest());
    if service() == Service::Aws {
        return Client::new(&defaults.load().await);
    }
    let config = defaults.region(Region::new("auto")).load().await;
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
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
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
        let checksum = head.checksum_sha256();
        match (service(), size == 1 << 20) {
            (_, true) => assert_eq!(
                checksum,
                Some(base64::encode(Sha256::digest(&data)).as_str()),
                "{head:?}"
            ),
            // R2 keeps the MD5 ETag of a multipart upload.
            (Service::R2, false) => assert!(
                head.e_tag().is_some_and(|tag| tag.ends_with("-3\"")),
                "{head:?}"
            ),
            // S3 keeps the SHA-256 of each part, and their composite.
            (Service::Aws, false) => {
                assert!(checksum.is_some_and(|sum| sum.ends_with("-3")), "{head:?}");
                let part = client
                    .head_object()
                    .bucket(bucket())
                    .key(&stored.data)
                    .part_number(1)
                    .checksum_mode(ChecksumMode::Enabled)
                    .send()
                    .await
                    .unwrap();
                let length = usize::try_from(part.content_length().unwrap()).unwrap();
                assert_eq!(
                    part.checksum_sha256(),
                    Some(base64::encode(Sha256::digest(&data[..length])).as_str()),
                    "{part:?}"
                );
            }
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
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
async fn dump_that_changed_after_staging_is_never_stored() {
    let client = client().await;

    // The service checks the single request. Svalbard checks the parts before it completes the
    // upload.
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
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
async fn cancellation_aborts_the_upload() {
    let client = client().await;
    let (dump, run) = staged(130 << 20);
    let destination = destination(&run);
    // Cancels as soon as the service lists the multipart upload.
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
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
async fn bucket_outside_the_r2_token_scope_is_access_denied() {
    // An R2 token can be limited to one bucket. The AWS test account has no bucket to deny.
    if service() != Service::R2 {
        println!("skipped for aws: no bucket to deny");
        return;
    }
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

/// The destination for a CLI run under `prefix`.
fn cli_destination(prefix: &str) -> String {
    match service() {
        Service::R2 => cli::r2(&var("SVALBARD_TEST_R2_ENDPOINT"), &bucket(), prefix),
        Service::Aws => format!(
            "[destination]\ntype = \"s3\"\nbucket = \"{}\"\nprefix = \"{prefix}\"\n",
            bucket()
        ),
    }
}

/// The storage secrets that must not appear: the secret key, and the session token of temporary
/// credentials.
fn storage_secrets() -> Vec<String> {
    ["AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .collect()
}

/// The secrets that must not appear: the source's, and the storage secrets.
fn secrets<'a>(source: &[&'a str], storage: &'a [String]) -> Vec<&'a str> {
    let storage: Vec<&str> = storage.iter().map(String::as_str).collect();
    [source, &storage].concat()
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
        "matrix: {source} {} ({}) -> {}: restored {} bytes",
        result["server_version"].as_str().unwrap(),
        result["client_version"].as_str().unwrap(),
        match service() {
            Service::R2 => "r2",
            Service::Aws => "aws s3",
        },
        result["size"]
    );
}

#[tokio::test]
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
async fn postgresql_backup_restores() {
    let client = client().await;
    let prefix = cli_prefix("cli-postgresql");
    let mut fixture = postgresql::Fixture::create("live_cli").await;
    fixture.add_larger().await;
    let setup = Setup::new(
        "live-cli-postgresql",
        &cli::postgresql_source(&fixture),
        &cli_destination(&prefix),
        "",
    );
    let storage = storage_secrets();
    let outcome = setup
        .run(postgresql::PASSWORD, &secrets(POSTGRESQL_SECRETS, &storage))
        .await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dump = check(&client, &prefix, &outcome.result).await;
    fixture.assert_restores(&dump).await;
    report("postgresql", &outcome.result);
    clean(&client, &prefix).await;
}

#[tokio::test]
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
async fn mysql_backup_restores() {
    let client = client().await;
    let prefix = cli_prefix("cli-mysql");
    let mut fixture = mysql::Fixture::create("live_cli").await;
    fixture.add_larger().await;
    let setup = Setup::new(
        "live-cli-mysql",
        &cli::mysql_source(&fixture),
        &cli_destination(&prefix),
        "",
    );
    let storage = storage_secrets();
    let outcome = setup
        .run(mysql::PASSWORD, &secrets(MYSQL_SECRETS, &storage))
        .await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dump = check(&client, &prefix, &outcome.result).await;
    fixture.assert_restores(&dump).await;
    report("mysql", &outcome.result);
    clean(&client, &prefix).await;
}

#[tokio::test]
#[ignore = "needs a real bucket: run scripts/test-live.sh"]
async fn rejected_secret_stores_nothing() {
    let client = client().await;
    let prefix = cli_prefix("cli-secret");
    let fixture = mysql::Fixture::create("live_cli_secret").await;
    let setup = Setup::new(
        "live-cli-secret",
        &cli::mysql_source(&fixture),
        &cli_destination(&prefix),
        "",
    );
    let (storage, wrong) = (storage_secrets(), "0".repeat(64));
    let child = setup.spawn_with(mysql::PASSWORD, &[("AWS_SECRET_ACCESS_KEY", &wrong)]);
    let outcome = setup
        .finish(
            child,
            &[&secrets(MYSQL_SECRETS, &storage)[..], &[&wrong]].concat(),
        )
        .await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "transfer", "{outcome:?}");
    assert_eq!(outcome.result["error"], "access_denied", "{outcome:?}");
    assert!(objects(&client, &prefix).await.is_empty());
}
