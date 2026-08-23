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
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finish_sha256(hasher)
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

fn sync_directory(path: &Path) -> Result<()> {
    let directory = fs::File::open(path)
        .with_context(|| format!("failed to open directory {}", path.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("failed to fsync directory {}", path.display()))
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
