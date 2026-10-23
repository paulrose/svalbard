//! Helpers for the storage tests.

use std::{fs, future::Future, path::Path};

use sha2::{Digest, Sha256};
use svalbard::{
    config,
    db::Dump,
    staging::Run,
    storage::{self, Error, S3, Stored},
};

/// A staged dump of `size` bytes in a new run directory.
pub fn staged(size: usize) -> (Dump, Run) {
    let run = Run::create(Path::new(env!("CARGO_TARGET_TMPDIR"))).unwrap();
    let path = run.dir.join("dump");
    let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &bytes).unwrap();
    let dump = Dump {
        path,
        format: "test",
        extension: "test",
        client_version: "client 1.0".to_owned(),
        server_version: "server 1.0".to_owned(),
        size: size as u64,
        sha256: hex(&Sha256::digest(&bytes)),
    };
    (dump, run)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Stores `dump` with the S3 adapter.
pub async fn store(
    destination: &config::Destination,
    dump: &Dump,
    run: &Run,
    cancel: impl Future<Output = ()>,
) -> Result<Stored, Error> {
    let mut s3 = S3::new(destination).await;
    storage::store(
        &mut s3,
        &destination.prefix,
        dump,
        run,
        destination.upload_timeout,
        cancel,
    )
    .await
}
