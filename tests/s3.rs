//! Tests against the Floci S3 emulator. Run them in Docker with `scripts/test-integration.sh`.
//!
//! Svalbard signs with the `floci` key from the environment, so bucket policies apply to it.
//! The setup uses the `test` key, which Floci never checks.

mod common;

use std::{
    env, future,
    net::SocketAddr,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    types::ChecksumMode,
};
use common::{hex, staged, store};
use sha2::{Digest, Sha256};
use svalbard::{
    config::{self, Provider},
    storage::Failure,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

fn endpoint() -> String {
    env::var("SVALBARD_TEST_S3_ENDPOINT")
        .expect("SVALBARD_TEST_S3_ENDPOINT must be set: run scripts/test-integration.sh")
}

/// A client with the setup key, which Floci never checks.
fn setup() -> Client {
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(endpoint())
        .force_path_style(true)
        .credentials_provider(Credentials::new("test", "test", None, None, "test"))
        .build();
    Client::from_conf(config)
}

/// A new bucket, with a policy that denies `action` on `resource` to every principal.
async fn bucket(name: &str, deny: Option<(&str, &str)>) -> String {
    let bucket = format!("svalbard-{name}");
    let client = setup();
    client.create_bucket().bucket(&bucket).send().await.unwrap();
    if let Some((action, resource)) = deny {
        let policy = format!(
            r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Deny","Principal":"*",
              "Action":"{action}","Resource":"arn:aws:s3:::{bucket}/{resource}"}}]}}"#
        );
        client
            .put_bucket_policy()
            .bucket(&bucket)
            .policy(policy)
            .send()
            .await
            .unwrap();
    }
    bucket
}

fn destination(bucket: &str, endpoint: String, upload_timeout: Duration) -> config::Destination {
    config::Destination {
        provider: Provider::S3,
        bucket: bucket.to_owned(),
        prefix: "app/".to_owned(),
        region: None,
        endpoint: Some(endpoint),
        upload_timeout,
    }
}

/// The keys of the objects in `bucket`.
async fn objects(bucket: &str) -> Vec<String> {
    let output = setup()
        .list_objects_v2()
        .bucket(bucket)
        .send()
        .await
        .unwrap();
    output
        .contents()
        .iter()
        .filter_map(|object| object.key().map(str::to_owned))
        .collect()
}

/// The keys of the unfinished multipart uploads in `bucket`.
async fn uploads(bucket: &str) -> Vec<String> {
    let output = setup()
        .list_multipart_uploads()
        .bucket(bucket)
        .send()
        .await
        .unwrap();
    output
        .uploads()
        .iter()
        .filter_map(|upload| upload.key().map(str::to_owned))
        .collect()
}

/// Downloads an object with curl, independently of the AWS SDK.
fn download(bucket: &str, key: &str) -> Vec<u8> {
    let output = Command::new("curl")
        .args(["--silent", "--show-error", "--fail"])
        .args(["--aws-sigv4", "aws:amz:us-east-1:s3", "--user", "test:test"])
        .arg(format!("{}/{bucket}/{key}", endpoint()))
        .output()
        .expect("curl must start");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// What the test proxy does to the traffic to Floci.
#[derive(Clone, Copy)]
enum Fault {
    /// Stops the connection that sends more than this many bytes in total.
    StallAfter(u64),
    /// Like `StallAfter`, and then refuses every new connection, so cleanup cannot reach Floci.
    StallThenRefuse(u64),
    /// Never returns the answer to the first request.
    HoldFirstResponse,
}

/// A TCP proxy to Floci. Connections that open after the fault pass unchanged, so cleanup
/// requests reach Floci.
struct Proxy {
    endpoint: String,
    /// Notified when the proxy stalls a connection.
    stalled: Arc<Notify>,
}

async fn proxy(fault: Fault) -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let upstream = endpoint()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned();
    let stalled = Arc::new(Notify::new());
    let sent = Arc::new(AtomicU64::new(0));
    let faulted = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let notify = stalled.clone();
    tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.unwrap();
            if matches!(fault, Fault::StallThenRefuse(_)) && faulted.load(Ordering::SeqCst) {
                continue;
            }
            let server = TcpStream::connect(&upstream).await.unwrap();
            let first = connections.fetch_add(1, Ordering::SeqCst) == 0;
            let active = !faulted.load(Ordering::SeqCst);
            let (sent, faulted, notify) = (sent.clone(), faulted.clone(), notify.clone());
            tokio::spawn(async move {
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                let hold = first && matches!(fault, Fault::HoldFirstResponse);
                tokio::spawn(async move {
                    if hold {
                        future::pending::<()>().await;
                    }
                    let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
                });
                let mut buffer = vec![0; 64 << 10];
                loop {
                    let Ok(read @ 1..) = client_read.read(&mut buffer).await else {
                        return;
                    };
                    let total = sent.fetch_add(read as u64, Ordering::SeqCst) + read as u64;
                    if let Fault::StallAfter(limit) | Fault::StallThenRefuse(limit) = fault
                        && active
                        && total > limit
                        && !faulted.swap(true, Ordering::SeqCst)
                    {
                        notify.notify_one();
                        future::pending::<()>().await;
                    }
                    if server_write.write_all(&buffer[..read]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Proxy {
        endpoint: format!("http://{address}"),
        stalled,
    }
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn dump_and_record_round_trip() {
    let bucket = bucket("round-trip", None).await;
    let destination = destination(&bucket, endpoint(), Duration::from_secs(60));

    // One request, and a multipart upload of three parts.
    for size in [1 << 20, 130 << 20] {
        let (dump, run) = staged(size);
        let stored = store(&destination, &dump, &run, future::pending())
            .await
            .expect("the upload must succeed");

        assert!(stored.data.starts_with("app/"), "{stored:?}");
        assert!(
            stored.data.ends_with(&format!("-{}.test", run.id)),
            "{stored:?}"
        );
        assert_eq!(stored.record, stored.data.replace(".test", ".json"));
        let data = download(&bucket, &stored.data);
        assert_eq!(data.len(), size);
        assert_eq!(hex(&Sha256::digest(&data)), dump.sha256);

        let record: serde_json::Value =
            serde_json::from_slice(&download(&bucket, &stored.record)).unwrap();
        assert_eq!(record["version"], 1);
        assert_eq!(record["data"], stored.data);
        assert_eq!(record["run_id"], run.id);
        assert_eq!(record["sha256"], dump.sha256);
        assert_eq!(record["size"], size);
        assert_eq!(record["format"], "test");
        assert_eq!(record["server_version"], "server 1.0");

        let head = setup()
            .head_object()
            .bucket(&bucket)
            .key(&stored.data)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await
            .unwrap();
        assert!(head.checksum_sha256().is_some(), "{head:?}");
        assert_eq!(
            head.metadata().and_then(|metadata| metadata.get("sha256")),
            Some(&dump.sha256)
        );
        run.remove().unwrap();
    }
    assert!(uploads(&bucket).await.is_empty());
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn denied_writes_store_nothing() {
    for size in [1 << 20, 130 << 20] {
        let bucket = bucket(&format!("denied-{size}"), Some(("s3:PutObject", "*"))).await;
        let destination = destination(&bucket, endpoint(), Duration::from_secs(60));
        let (dump, run) = staged(size);
        let error = store(&destination, &dump, &run, future::pending())
            .await
            .expect_err("the upload must fail");

        assert_eq!(error.kind, Failure::AccessDenied, "{error}");
        assert!(objects(&bucket).await.is_empty());
        assert!(uploads(&bucket).await.is_empty());
        run.remove().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn denied_record_leaves_an_incomplete_backup() {
    let bucket = bucket("record-denied", Some(("s3:PutObject", "*.json"))).await;
    let destination = destination(&bucket, endpoint(), Duration::from_secs(60));
    let (dump, run) = staged(1 << 20);
    let error = store(&destination, &dump, &run, future::pending())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::AccessDenied, "{error}");
    let objects = objects(&bucket).await;
    assert_eq!(objects.len(), 1, "{objects:?}");
    assert!(objects[0].ends_with(".test"), "{objects:?}");
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn cancellation_aborts_the_upload() {
    let bucket = bucket("cancel", None).await;
    let proxy = proxy(Fault::StallAfter(8 << 20)).await;
    let destination = destination(&bucket, proxy.endpoint, Duration::from_secs(60));
    let (dump, run) = staged(130 << 20);
    let error = store(&destination, &dump, &run, proxy.stalled.notified())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::Cancelled, "{error}");
    assert!(objects(&bucket).await.is_empty());
    assert!(uploads(&bucket).await.is_empty());
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn timeout_before_the_upload_id_finds_and_aborts_the_upload() {
    let bucket = bucket("timeout", None).await;
    // Floci creates the upload, but Svalbard never receives its ID.
    let proxy = proxy(Fault::HoldFirstResponse).await;
    let destination = destination(&bucket, proxy.endpoint, Duration::from_secs(2));
    let (dump, run) = staged(130 << 20);
    let error = store(&destination, &dump, &run, future::pending())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::UploadTimeout, "{error}");
    assert!(uploads(&bucket).await.is_empty());
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn failed_abort_reports_the_remaining_upload() {
    let bucket = bucket("abort-fails", None).await;
    let proxy = proxy(Fault::StallThenRefuse(8 << 20)).await;
    let destination = destination(&bucket, proxy.endpoint, Duration::from_secs(60));
    let (dump, run) = staged(130 << 20);
    let error = store(&destination, &dump, &run, proxy.stalled.notified())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::CleanupFailed, "{error}");
    assert!(error.message.contains("cancelled"), "{error}");
    assert_eq!(uploads(&bucket).await.len(), 1);
    run.remove().unwrap();
}

/// A server that answers every request with `status` and the S3 error `code`. Returns its
/// endpoint and a count of the requests.
async fn always(status: &'static str, code: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            let body = format!("<Error><Code>{code}</Code><Message>test</Message></Error>");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut buffer = vec![0; 64 << 10];
            let _ = stream.read(&mut buffer).await;
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{address}"), requests)
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn server_errors_are_retried_a_bounded_number_of_times() {
    let (endpoint, requests) = always("500 Internal Server Error", "InternalError").await;
    let destination = destination("svalbard-retries", endpoint, Duration::from_secs(60));
    let (dump, run) = staged(1 << 10);
    let error = store(&destination, &dump, &run, future::pending())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::Upload, "{error}");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs S3: run scripts/test-integration.sh"]
async fn refused_upload_start_needs_no_cleanup() {
    // Floci never refuses CreateMultipartUpload, so this server does. Cleanup must not list the
    // uploads, which needs an optional permission.
    let (endpoint, requests) = always("403 Forbidden", "AccessDenied").await;
    let destination = destination("svalbard-refused", endpoint, Duration::from_secs(60));
    let (dump, run) = staged(65 << 20);
    let error = store(&destination, &dump, &run, future::pending())
        .await
        .expect_err("the upload must fail");

    assert_eq!(error.kind, Failure::AccessDenied, "{error}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    run.remove().unwrap();
}
