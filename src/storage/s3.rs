//! Amazon S3 and S3-compatible storage. See "Storage" in README.md.

use std::time::Duration;

use aws_config::{BehaviorVersion, Region, retry::RetryConfig, timeout::TimeoutConfig};
use aws_sdk_s3::{
    Client,
    config::http::HttpResponse,
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::{ByteStream, Length},
    types::{ChecksumAlgorithm, CompletedMultipartUpload, CompletedPart},
};
use aws_smithy_types::base64;
use sha2::{Digest, Sha256};
use tokio::{fs::File, io::AsyncReadExt};

use super::{Destination, Error, Failure, Staged};
use crate::config;

/// The smallest part of a multipart upload. Smaller files take one request.
const PART_SIZE: u64 = 64 << 20;
/// The largest number of parts in a multipart upload.
const MAX_PARTS: u64 = 10_000;
/// The buffer for reading the staged file.
const BUFFER: usize = 1 << 20;

/// An S3 bucket. The credentials come from the AWS credential chain.
pub struct S3 {
    client: Client,
    bucket: String,
    /// The multipart upload in progress: its key, and its ID once S3 returned one.
    pending: Option<(String, Option<String>)>,
}

impl S3 {
    /// Reads the AWS configuration and the credential settings. Makes no request.
    pub async fn new(destination: &config::Destination) -> S3 {
        // Set in code, so that AWS_MAX_ATTEMPTS and similar variables cannot remove the bounds.
        let mut loader = aws_config::defaults(BehaviorVersion::v2026_01_12())
            .retry_config(RetryConfig::standard().with_max_attempts(3))
            // No read timeout: in the SDK it also covers sending a part. Stalled-stream
            // protection ends a transfer that stops.
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .operation_attempt_timeout(Duration::from_secs(15 * 60))
                    .build(),
            );
        if let Some(region) = &destination.region {
            loader = loader.region(Region::new(region.clone()));
        }
        let mut config = aws_sdk_s3::config::Builder::from(&loader.load().await);
        if let Some(endpoint) = &destination.endpoint {
            config = config.endpoint_url(endpoint).force_path_style(true);
        }
        S3 {
            client: Client::from_conf(config.build()),
            bucket: destination.bucket.clone(),
            pending: None,
        }
    }

    /// Uploads a small file in one request. S3 checks the file against its SHA-256.
    async fn put_object(&mut self, key: &str, file: &Staged<'_>) -> Result<(), Error> {
        let expected = base64::encode(file.sha256);
        let output = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body(file, 0, file.size).await?)
            .checksum_sha256(&expected)
            .metadata("sha256", hex(&file.sha256))
            .send()
            .await
            .map_err(|error| failure(&format!("upload {key}"), error))?;
        confirm(key, output.checksum_sha256(), &expected)
    }

    /// Uploads a large file in parts. S3 checks each part against its SHA-256, and the
    /// file must still match the staged SHA-256.
    async fn put_parts(&mut self, key: &str, file: &Staged<'_>) -> Result<(), Error> {
        self.pending = Some((key.to_owned(), None));
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
            .metadata("sha256", hex(&file.sha256))
            .send()
            .await
            .map_err(|error| {
                // An error answer means that S3 created no upload, so there is nothing to abort.
                if matches!(error, SdkError::ServiceError(_)) {
                    self.pending = None;
                }
                failure(&format!("start the upload of {key}"), error)
            })?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| {
                Error::new(
                    Failure::Upload,
                    format!("S3 returned no upload ID for {key}"),
                )
            })?
            .to_owned();
        self.pending = Some((key.to_owned(), Some(upload_id.clone())));

        let part_size = part_size(file.size);
        let mut reader = File::open(file.path)
            .await
            .map_err(|error| read_error(file, error))?;
        let mut whole = Sha256::new();
        let mut digests = Vec::new();
        let mut parts = Vec::new();
        for (index, offset) in (0..file.size).step_by(part_size as usize).enumerate() {
            let length = part_size.min(file.size - offset);
            let digest = hash(&mut reader, length, &mut whole)
                .await
                .map_err(|error| read_error(file, error))?;
            let checksum = base64::encode(digest);
            let number = index as i32 + 1;
            let output = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(number)
                .body(body(file, offset, length).await?)
                .checksum_sha256(&checksum)
                .send()
                .await
                .map_err(|error| failure(&format!("upload part {number} of {key}"), error))?;
            confirm(key, output.checksum_sha256(), &checksum)?;
            parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(output.e_tag().map(str::to_owned))
                    .checksum_sha256(checksum)
                    .build(),
            );
            digests.extend(digest);
        }
        if whole.finalize()[..] != file.sha256 {
            return Err(Error::new(
                Failure::Upload,
                format!("{} changed after the dump", file.path.display()),
            ));
        }

        let output = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|error| failure(&format!("complete the upload of {key}"), error))?;
        self.pending = None;
        // A multipart checksum is the SHA-256 of the part checksums, with `-<parts>` appended.
        let returned = output
            .checksum_sha256()
            .map(|checksum| checksum.split('-').next().unwrap_or(checksum));
        confirm(key, returned, &base64::encode(Sha256::digest(&digests)))
    }

    /// The IDs of the unfinished uploads to `key`.
    async fn uploads(&self, key: &str) -> Result<Vec<String>, Error> {
        // shortcut: reads one page of up to 1000 uploads under `key`. A run uses unique keys.
        let output = self
            .client
            .list_multipart_uploads()
            .bucket(&self.bucket)
            .prefix(key)
            .send()
            .await
            .map_err(|error| cleanup_failed(key, failure("list the uploads", error)))?;
        Ok(output
            .uploads()
            .iter()
            .filter(|upload| upload.key() == Some(key))
            .filter_map(|upload| upload.upload_id().map(str::to_owned))
            .collect())
    }
}

impl Destination for S3 {
    async fn put(&mut self, key: &str, file: &Staged<'_>) -> Result<(), Error> {
        if file.size <= part_size(file.size) {
            self.put_object(key, file).await
        } else {
            self.put_parts(key, file).await
        }
    }

    async fn abort(&mut self, key: &str) -> Result<(), Error> {
        // A single PutObject is complete or absent, so only a multipart upload can remain.
        let Some((pending, upload_id)) = self.pending.take() else {
            return Ok(());
        };
        if pending != key {
            return Ok(());
        }
        // Without an ID, the request that created the upload did not answer.
        let upload_ids = match upload_id {
            Some(upload_id) => vec![upload_id],
            None => self.uploads(key).await?,
        };
        for upload_id in upload_ids {
            match self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await
            {
                Err(error) if error.code() != Some("NoSuchUpload") => {
                    return Err(cleanup_failed(
                        key,
                        failure(&format!("abort the upload {upload_id}"), error),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The part size for a file: at least [`PART_SIZE`], and large enough for [`MAX_PARTS`].
fn part_size(size: u64) -> u64 {
    PART_SIZE.max(size.div_ceil(MAX_PARTS).next_multiple_of(1 << 20))
}

/// A body that streams a range of the file. The SDK reopens the file to send it again.
async fn body(file: &Staged<'_>, offset: u64, length: u64) -> Result<ByteStream, Error> {
    ByteStream::read_from()
        .path(file.path)
        .offset(offset)
        .length(Length::Exact(length))
        .buffer_size(BUFFER)
        .build()
        .await
        .map_err(|error| {
            Error::new(
                Failure::Upload,
                format!("cannot read {}: {error}", file.path.display()),
            )
        })
}

/// The SHA-256 of the next `length` bytes. Also adds them to `whole`.
async fn hash(reader: &mut File, length: u64, whole: &mut Sha256) -> std::io::Result<[u8; 32]> {
    let mut part = Sha256::new();
    let mut buffer = vec![0; BUFFER];
    let mut left = length;
    while left > 0 {
        let read = reader
            .read(&mut buffer[..BUFFER.min(left as usize)])
            .await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        part.update(&buffer[..read]);
        whole.update(&buffer[..read]);
        left -= read as u64;
    }
    Ok(part.finalize().into())
}

/// Fails unless S3 returned the expected checksum.
fn confirm(key: &str, returned: Option<&str>, expected: &str) -> Result<(), Error> {
    match returned {
        Some(returned) if returned == expected => Ok(()),
        returned => Err(Error::new(
            Failure::Upload,
            format!(
                "S3 returned the SHA-256 checksum {} for {key}, not {expected}",
                returned.unwrap_or("none")
            ),
        )),
    }
}

/// Classifies an S3 error. Access errors need other credentials or permissions, so a retry
/// cannot help.
fn failure<E>(action: &str, error: SdkError<E, HttpResponse>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    let denied = error
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 403)
        || matches!(
            error.code(),
            Some("AccessDenied" | "InvalidAccessKeyId" | "SignatureDoesNotMatch" | "ExpiredToken")
        );
    let kind = if denied {
        Failure::AccessDenied
    } else {
        Failure::Upload
    };
    Error::new(
        kind,
        format!("cannot {action}: {}", DisplayErrorContext(&error)),
    )
}

fn cleanup_failed(key: &str, error: Error) -> Error {
    Error::new(
        Failure::CleanupFailed,
        format!("an unfinished upload of {key} may remain: {error}"),
    )
}

fn read_error(file: &Staged<'_>, error: std::io::Error) -> Error {
    Error::new(
        Failure::Upload,
        format!("cannot read {}: {error}", file.path.display()),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_size_grows_to_fit_the_part_limit() {
        assert_eq!(part_size(0), PART_SIZE);
        assert_eq!(part_size(PART_SIZE * MAX_PARTS), PART_SIZE);
        let large = 16u64 << 40;
        let size = part_size(large);
        assert!(size * MAX_PARTS >= large, "{size}");
        assert_eq!(size % (1 << 20), 0);
        assert!(size < 5 << 30, "S3 limits a part to 5 GiB");
    }
}
