//! Helpers for the integration tests. Each test crate uses a part of them.
#![allow(dead_code)]

pub mod cli;
pub mod floci;
pub mod mysql;
pub mod postgresql;
pub mod ssh;

use std::{
    future::{self, Future},
    io::Write,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use sha2::{Digest, Sha256};
use svalbard::{
    config::{self, Target},
    db::Dump,
    staging::{self, Run},
    storage::{self, Error, S3, Ssh, Stored},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

/// A staged dump of `size` bytes in a new run directory.
pub fn staged(size: usize) -> (Dump, Run) {
    let run = Run::create(Path::new(env!("CARGO_TARGET_TMPDIR"))).unwrap();
    let path = run.dir.join("dump");
    let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    // Private, like a staged dump.
    staging::create_private(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
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

/// Stores `dump` with the adapter of the destination.
pub async fn store(
    destination: &config::Destination,
    dump: &Dump,
    run: &Run,
    cancel: impl Future<Output = ()>,
) -> Result<Stored, Error> {
    let (prefix, timeout) = (&destination.prefix, destination.upload_timeout);
    match &destination.target {
        Target::ObjectStore(target) => {
            let mut s3 = S3::new(target).await;
            storage::store(&mut s3, prefix, dump, run, timeout, cancel).await
        }
        Target::Ssh(target) => {
            let mut ssh = Ssh::new(target.clone());
            storage::store(&mut ssh, prefix, dump, run, timeout, cancel).await
        }
    }
}

/// What the test proxy does to the traffic.
#[derive(Clone, Copy)]
pub enum Fault {
    /// Stops the connection that sends more than this many bytes in total.
    StallAfter(u64),
    /// Like `StallAfter`, and then refuses every new connection, so cleanup cannot reach the
    /// server.
    StallThenRefuse(u64),
    /// Closes the connection that sends more than this many bytes in total.
    CloseAfter(u64),
    /// Closes every connection that sends more than this many bytes of its own.
    CloseEachAfter(u64),
    /// Never returns the answer to the first request.
    HoldFirstResponse,
}

/// A TCP proxy. Connections that open after the fault pass unchanged, so cleanup requests
/// reach the server.
pub struct Proxy {
    pub address: SocketAddr,
    /// Notified when the proxy stalls a connection.
    pub stalled: Arc<Notify>,
}

/// A proxy to `upstream`, a host and port.
pub async fn proxy(upstream: &str, fault: Fault) -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let upstream = upstream.to_owned();
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
                let answers = tokio::spawn(async move {
                    if hold {
                        future::pending::<()>().await;
                    }
                    let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
                });
                let mut buffer = vec![0; 64 << 10];
                let mut own = 0;
                loop {
                    let Ok(read @ 1..) = client_read.read(&mut buffer).await else {
                        return;
                    };
                    own += read as u64;
                    let total = sent.fetch_add(read as u64, Ordering::SeqCst) + read as u64;
                    if let Fault::CloseEachAfter(limit) = fault
                        && own > limit
                    {
                        answers.abort();
                        return;
                    }
                    if let Fault::StallAfter(limit) | Fault::StallThenRefuse(limit) = fault
                        && active
                        && total > limit
                        && !faulted.swap(true, Ordering::SeqCst)
                    {
                        notify.notify_one();
                        future::pending::<()>().await;
                    }
                    if let Fault::CloseAfter(limit) = fault
                        && active
                        && total > limit
                        && !faulted.swap(true, Ordering::SeqCst)
                    {
                        answers.abort();
                        return;
                    }
                    if server_write.write_all(&buffer[..read]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Proxy { address, stalled }
}
