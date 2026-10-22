//! Private staging. See "Private staging" in docs/database-safety.md.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// A private directory for one run. It holds the credential file and the staged dump.
#[derive(Debug)]
pub struct Run {
    /// A random value, unique to the run. The database sessions of the run carry it as a label.
    pub id: String,
    pub dir: PathBuf,
}

impl Run {
    /// Creates a new run directory with mode 0700 in `staging`.
    pub fn create(staging: &Path) -> io::Result<Run> {
        let mut bytes = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let dir = staging.join(format!("svalbard-{id}"));
        DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Run { id, dir })
    }

    /// Deletes the run directory and its files.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }
}

/// Creates a file with mode 0600. Fails if the file exists.
pub fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// The bytes that Svalbard can use on the filesystem that holds `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let stat = rustix::fs::statvfs(path)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}
