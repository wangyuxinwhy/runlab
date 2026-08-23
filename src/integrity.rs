//! Exact bytes: digests, canonical JSON, bounded reads, and durable private
//! writes.
//!
//! Every artifact this crate persists goes through here, which is what makes
//! "owner-only, crash-atomic, fsynced" one decision rather than a habit each
//! module reimplements. `write_new_private` and `replace_private` publish
//! through a temporary file and fsync the parent directory, so a reader sees
//! either no file or all the bytes.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags, open};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::core::Digest;

pub fn canonical_json(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).context("failed to encode deterministic JSON")
}

pub fn digest_bytes(bytes: &[u8]) -> Digest {
    Digest::of(bytes)
}

pub fn digest_reader(mut reader: impl Read) -> Result<(Digest, u64)> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("failed to read bytes for digest")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(u64::try_from(read).context("digest size overflow")?)
            .context("digest size overflow")?;
    }
    Ok((finish_sha256(hasher), size))
}

pub fn finish_sha256(hasher: Sha256) -> Digest {
    let bytes = hasher.finalize();
    let mut hexadecimal = String::with_capacity(64);
    for byte in bytes {
        write!(&mut hexadecimal, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Digest::parse(format!("sha256:{hexadecimal}"))
        .expect("SHA-256 formatting always produces a valid digest")
}

pub fn write_new_output(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        bail!("output path already exists: {}", path.display());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create output beside {}", path.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("failed to write output {}", path.display()))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to fsync output {}", path.display()))?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    sync_directory(parent)
}

pub fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))?;
    let directory = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| {
            format!(
                "private directory is not a real directory: {}",
                path.display()
            )
        })?,
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure directory {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn open_regular_lock(path: &Path, create: bool, description: &str) -> Result<File> {
    let mut flags = OFlags::RDWR | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if create {
        flags |= OFlags::CREATE;
    }
    let file = File::from(
        open(path, flags, Mode::RUSR | Mode::WUSR)
            .with_context(|| format!("failed to open {description} {}", path.display()))?,
    );
    if !file
        .metadata()
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?
        .is_file()
    {
        bail!("{description} is not a regular file: {}", path.display());
    }
    Ok(file)
}

/// Durably record a directory entry change. Every publish in this crate pairs a
/// file `sync_all` with this call; without it a crash can lose the rename that
/// made the file reachable.
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync directory {}", path.display()))
}

/// Restrict a file to its owner. The crate keeps every derived artifact private
/// because Run state can contain captured process output.
pub(crate) fn set_private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to set owner-only file permissions")?;
    }
    Ok(())
}

/// Publish `bytes` at `path` as a new private file, refusing to replace an
/// existing one. Crash-atomic: readers observe either no file or all the bytes.
pub(crate) fn write_new_private(path: &Path, bytes: &[u8]) -> Result<()> {
    persist_private(path, bytes, Publish::New)
}

/// Atomically replace `path` with `bytes`, keeping the file private.
pub(crate) fn replace_private(path: &Path, bytes: &[u8]) -> Result<()> {
    persist_private(path, bytes, Publish::Replace)
}

#[derive(Clone, Copy)]
enum Publish {
    New,
    Replace,
}

fn persist_private(path: &Path, bytes: &[u8], publish: Publish) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create replacement for {}", path.display()))?;
    set_private_file(temporary.as_file())?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to fsync {}", path.display()))?;
    match publish {
        Publish::New => {
            temporary
                .persist_noclobber(path)
                .map_err(|error| error.error)
                .with_context(|| format!("failed to publish {}", path.display()))?;
        }
        Publish::Replace => {
            let temporary = temporary.into_temp_path();
            fs::rename(&temporary, path)
                .with_context(|| format!("failed to publish replacement for {}", path.display()))?;
        }
    }
    sync_directory(parent)
}

/// Read at most `limit` bytes from `path`, refusing anything larger.
pub(crate) fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let name = path.display().to_string();
    let file = File::open(path).with_context(|| format!("failed to open {name}"))?;
    read_bounded(file, limit, &name)
}

/// Read at most `limit` bytes from `reader`, refusing anything larger.
pub(crate) fn read_bounded(reader: impl Read, limit: u64, name: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {name}"))?;
    if u64::try_from(bytes.len()).context("content size overflow")? > limit {
        bail!("{name} exceeds the {limit}-byte limit");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn deterministic_json_sorts_object_keys() {
        assert_eq!(
            canonical_json(&json!({"b": 1, "a": 2})).expect("JSON"),
            br#"{"a":2,"b":1}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_never_follows_a_final_symlink() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().expect("temporary directory");
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755))
            .expect("outside permissions");
        let link = directory.path().join("private");
        symlink(&outside, &link).expect("directory symlink");

        let error = ensure_private_directory(&link).expect_err("symlink must fail closed");
        assert!(format!("{error:#}").contains("private directory is not a real directory"));
        assert_eq!(
            fs::metadata(outside)
                .expect("outside metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}
