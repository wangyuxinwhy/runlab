use std::io::Read;
use std::str::FromStr as _;

use anyhow::{Context, Result, bail};
use oci_spec::image::Digest;
use sha2::{Digest as _, Sha256};

use super::usize_to_u64;
pub(super) fn copy_and_digest(
    mut reader: impl Read,
    mut writer: impl std::io::Write,
    expected_size: Option<u64>,
) -> Result<(Digest, u64)> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(usize_to_u64(read))
            .context("content size overflow")?;
        if expected_size.is_some_and(|expected| size > expected) {
            bail!("content exceeds declared size");
        }
        writer.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    if let Some(expected) = expected_size
        && size != expected
    {
        bail!("content size mismatch: expected {expected}, received {size}");
    }
    Ok((finish_sha256(hasher), size))
}

#[cfg(test)]
pub(super) fn sha256_digest(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finish_sha256(hasher)
}

pub(super) fn finish_sha256(hasher: Sha256) -> Digest {
    let mut hexadecimal = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        write!(&mut hexadecimal, "{byte:02x}").expect("writing to String cannot fail");
    }
    Digest::from_str(&format!("sha256:{hexadecimal}"))
        .expect("SHA-256 always forms a valid OCI digest")
}
