//! The Floci S3 emulator. The setup key `test` skips its IAM checks.

use std::{env, process::Command};

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
};

pub fn endpoint() -> String {
    env::var("SVALBARD_TEST_S3_ENDPOINT")
        .expect("SVALBARD_TEST_S3_ENDPOINT must be set: run scripts/test-integration.sh")
}

/// A client with the setup key, which Floci never checks.
pub fn setup() -> Client {
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
pub async fn bucket(name: &str, deny: Option<(&str, &str)>) -> String {
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

/// The keys of the objects in `bucket`.
pub async fn objects(bucket: &str) -> Vec<String> {
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
pub async fn uploads(bucket: &str) -> Vec<String> {
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
pub fn download(bucket: &str, key: &str) -> Vec<u8> {
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
