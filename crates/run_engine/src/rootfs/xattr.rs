use std::os::fd::AsFd;
use std::path::Path;

use anyhow::{Result, bail};
use rustix::fs::{flistxattr, llistxattr};

use super::display_bytes;

pub(super) fn list_fd_xattr_names(fd: impl AsFd, limit: usize) -> Result<Vec<u8>> {
    let empty: &mut [u8] = &mut [];
    let required = flistxattr(&fd, empty)?;
    if required > limit {
        bail!("filesystem xattr name list exceeds limit");
    }
    let mut names = vec![0_u8; required];
    let read = flistxattr(&fd, &mut names)?;
    names.truncate(read);
    Ok(names)
}

pub(super) fn list_path_xattr_names(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let empty: &mut [u8] = &mut [];
    let required = llistxattr(path, empty)?;
    if required > limit {
        bail!("filesystem xattr name list exceeds limit");
    }
    let mut names = vec![0_u8; required];
    let read = llistxattr(path, &mut names)?;
    names.truncate(read);
    Ok(names)
}

pub(super) fn split_xattr_names(names: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    if !names.is_empty() && !names.ends_with(&[0]) {
        bail!("filesystem returned malformed xattr names");
    }
    Ok(names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty()))
}
pub(super) fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[usize::from(first >> 2)]);
        encoded.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)]);
        encoded.push(if chunk.len() > 1 {
            TABLE[usize::from((second & 0x0f) << 2 | third >> 6)]
        } else {
            b'='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[usize::from(third & 0x3f)]
        } else {
            b'='
        });
    }
    String::from_utf8(encoded).expect("base64 is ASCII")
}

pub(super) fn decode_base64(bytes: &[u8]) -> Result<Vec<u8>> {
    if !bytes.len().is_multiple_of(4) {
        bail!("invalid base64 xattr value");
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            base64_value(chunk[3])?
        };
        if (chunk[3] == b'=' || chunk[2] == b'=') && (chunk[3] != b'=' || !last) {
            bail!("invalid base64 xattr padding");
        }
        decoded.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            decoded.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            decoded.push((c << 6) | d);
        }
    }
    Ok(decoded)
}

pub(super) fn base64_value(byte: u8) -> Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => bail!("invalid base64 xattr byte"),
    }
}

pub(super) fn validate_pax_xattr_name(name: &[u8]) -> Result<()> {
    if name
        .iter()
        .any(|byte| !(33..=126).contains(byte) || *byte == b'=')
    {
        bail!(
            "Linux xattr name cannot be represented literally in a PAX key: {}",
            display_bytes(name)
        );
    }
    Ok(())
}
