use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek as _};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sha2::{Digest as _, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::core::Digest;
use crate::integrity::{digest_reader, finish_sha256};

#[derive(Debug)]
pub(crate) struct ContentStore {
    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    directory: Option<TempDir>,
    paths: BTreeMap<Digest, PathBuf>,
}

impl ContentStore {
    pub(crate) fn new() -> Result<Self> {
        Self::create(tempfile::Builder::new().prefix("runlab-content-").tempdir())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn new_in(parent: &Path) -> Result<Self> {
        Self::create(
            tempfile::Builder::new()
                .prefix("content-")
                .tempdir_in(parent),
        )
    }

    fn create(directory: std::io::Result<TempDir>) -> Result<Self> {
        Ok(Self {
            directory: Some(directory.context("failed to create filesystem content store")?),
            paths: BTreeMap::new(),
        })
    }

    #[cfg(any(test, target_os = "linux"))]
    pub(crate) fn digest_only() -> Self {
        Self {
            directory: None,
            paths: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn put_bytes(&mut self, bytes: &[u8]) -> Result<Digest> {
        self.put_reader(bytes).map(|(digest, _)| digest)
    }

    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    pub(crate) fn put_reader(&mut self, mut reader: impl Read) -> Result<(Digest, u64)> {
        let mut temporary = self
            .directory
            .as_ref()
            .map(|directory| NamedTempFile::new_in(directory.path()))
            .transpose()
            .context("failed to stage filesystem content")?;
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .context("failed to read filesystem content")?;
            if read == 0 {
                break;
            }
            if let Some(temporary) = &mut temporary {
                std::io::Write::write_all(temporary, &buffer[..read])
                    .context("failed to write filesystem content")?;
            }
            hasher.update(&buffer[..read]);
            size = size
                .checked_add(u64::try_from(read).context("filesystem content size overflow")?)
                .context("filesystem content size overflow")?;
        }
        let digest = finish_sha256(hasher);
        if self.paths.contains_key(&digest) {
            return Ok((digest, size));
        }
        let Some(mut temporary) = temporary else {
            return Ok((digest, size));
        };
        temporary
            .as_file_mut()
            .sync_all()
            .context("failed to fsync filesystem content")?;
        let path = self
            .directory
            .as_ref()
            .expect("content-backed store has a directory")
            .path()
            .join(digest.hex());
        temporary
            .persist_noclobber(&path)
            .map_err(|error| error.error)
            .context("failed to publish filesystem content")?;
        self.paths.insert(digest.clone(), path);
        Ok((digest, size))
    }

    pub(crate) fn open(&self, digest: &Digest, expected_size: u64) -> Result<File> {
        let path = self
            .paths
            .get(digest)
            .with_context(|| format!("filesystem content is unavailable: {digest}"))?;
        let mut file = File::open(path)
            .with_context(|| format!("failed to open filesystem content: {digest}"))?;
        let (actual, size) = digest_reader(&mut file)?;
        if &actual != digest || size != expected_size {
            bail!(
                "filesystem content failed verification for {digest}: size {size}, expected {expected_size}"
            );
        }
        file.rewind()?;
        Ok(file)
    }
}
