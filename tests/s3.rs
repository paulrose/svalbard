//! Tests against the Floci S3 emulator. Run them in Docker with `scripts/test-integration.sh`.
//!
//! Svalbard signs with the `floci` key from the environment, so bucket policies apply to it.
//! The setup uses the `test` key, which Floci never checks.

mod common;

use std::{
    future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use aws_sdk_s3::types::ChecksumMode;
use common::{Fault, floci::*, hex, proxy, staged, store};
use sha2::{Digest, Sha256};
use svalbard::{
    config::{self, ObjectStore, Provider, Target},
    storage::Failure,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn destination(bucket: &str, endpoint: String, upload_timeout: Duration) -> config::Destination {
    config::Destination {
        target: Target::ObjectStore(ObjectStore {
            provider: Provider::S3,
            bucket: bucket.to_owned(),
            region: None,
            endpoint: Some(endpoint),
        }),
        prefix: "app/".to_owned(),
        upload_timeout,
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
    let proxy = proxy(&upstream(), Fault::StallAfter(8 << 20)).await;
    let destination = destination(
        &bucket,
        format!("http://{}", proxy.address),
        Duration::from_secs(60),
    );
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
    let proxy = proxy(&upstream(), Fault::HoldFirstResponse).await;
    let destination = destination(
        &bucket,
        format!("http://{}", proxy.address),
        Duration::from_secs(2),
    );
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
    let proxy = proxy(&upstream(), Fault::StallThenRefuse(8 << 20)).await;
    let destination = destination(
        &bucket,
        format!("http://{}", proxy.address),
        Duration::from_secs(60),
    );
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
