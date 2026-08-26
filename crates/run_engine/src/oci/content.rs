use std::fmt::Write as _;
use std::io::{self, Read};
#[cfg(test)]
use std::io::{Seek, SeekFrom};

use oci_spec::image::{Descriptor, Digest, MediaType};
use sha2::{Digest as _, Sha256};

use super::{OciError, VerifiedContent, image_error};
use crate::OciContentStore;

/// Reads bounded exact bytes and verifies all identity fields of `descriptor`.
pub(crate) fn read_small_verified(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    expected_media_types: &[MediaType],
    limit: u64,
    path: impl Into<String>,
) -> Result<VerifiedContent, OciError> {
    let path = path.into();
    verify_media_type(descriptor, expected_media_types, &path)?;
    if descriptor.size() > limit {
        return Err(OciError::JsonLimit {
            path,
            limit,
            size: descriptor.size(),
        });
    }
    let mut content = store.open(descriptor).map_err(|source| OciError::Content {
        operation: "open",
        path: path.clone(),
        source,
    })?;
    let capacity = usize::try_from(descriptor.size()).map_err(|_| OciError::JsonLimit {
        path: path.clone(),
        limit,
        size: descriptor.size(),
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let read_limit = descriptor_read_limit(descriptor, &path)?;
    content
        .by_ref()
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|source| layer_io_error(&path, source))?;
    verify_bytes(descriptor, &bytes, &path)?;
    verify_embedded_bytes(descriptor, &bytes, &path)?;
    Ok(VerifiedContent {
        descriptor: descriptor.clone(),
        bytes,
    })
}

/// Opens and stream-verifies content without retaining its bytes.
pub(crate) fn verify_content(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    expected_media_types: &[MediaType],
    path: impl Into<String>,
) -> Result<(), OciError> {
    let path = path.into();
    verify_media_type(descriptor, expected_media_types, &path)?;
    let mut content = store.open(descriptor).map_err(|source| OciError::Content {
        operation: "open",
        path: path.clone(),
        source,
    })?;
    verify_reader(descriptor, &mut content, &path)
}

/// Verifies and atomically publishes exact bytes under a complete Descriptor.
///
/// A successful return also proves that the published content can be read back
/// through the same complete Descriptor.
pub(crate) fn publish_expected(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    content: &mut dyn Read,
    expected_media_types: &[MediaType],
    path: impl Into<String>,
) -> Result<(), OciError> {
    let path = path.into();
    verify_media_type(descriptor, expected_media_types, &path)?;
    store
        .publish(descriptor, content)
        .map_err(|source| OciError::Content {
            operation: "publish",
            path: path.clone(),
            source,
        })?;
    verify_content(store, descriptor, expected_media_types, path)
}

/// Computes a complete sha256 Descriptor and atomically publishes its bytes.
#[cfg(test)]
pub(crate) fn publish_content<R: Read + Seek>(
    store: &dyn OciContentStore,
    media_type: MediaType,
    content: &mut R,
    path: impl Into<String>,
) -> Result<Descriptor, OciError> {
    let path = path.into();
    let (size, digest) = digest_stream(content, &path)?;
    content
        .seek(SeekFrom::Start(0))
        .map_err(|source| layer_io_error(&path, source))?;
    let descriptor = Descriptor::new(media_type.clone(), size, digest);
    publish_expected(store, &descriptor, content, &[media_type], path)?;
    Ok(descriptor)
}

pub(super) fn enforce_generated_json_limit(
    bytes: &[u8],
    limit: u64,
    path: &str,
) -> Result<(), OciError> {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > limit {
        return Err(OciError::JsonLimit {
            path: path.to_owned(),
            limit,
            size,
        });
    }
    Ok(())
}

pub(super) fn verify_media_type(
    descriptor: &Descriptor,
    expected: &[MediaType],
    path: &str,
) -> Result<(), OciError> {
    if expected.iter().any(|item| item == descriptor.media_type()) {
        return Ok(());
    }
    Err(OciError::MediaType {
        path: path.to_owned(),
        expected: expected
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        actual: descriptor.media_type().to_string(),
    })
}

fn verify_bytes(descriptor: &Descriptor, bytes: &[u8], path: &str) -> Result<(), OciError> {
    let actual_size = u64::try_from(bytes.len()).map_err(|_| OciError::Size {
        path: path.to_owned(),
        expected: descriptor.size(),
        actual: u64::MAX,
    })?;
    if descriptor.size() != actual_size {
        return Err(OciError::Size {
            path: path.to_owned(),
            expected: descriptor.size(),
            actual: actual_size,
        });
    }
    let Some(expected) = descriptor.as_digest_sha256() else {
        return Err(OciError::DigestAlgorithm {
            path: path.to_owned(),
            digest: descriptor.digest().to_string(),
        });
    };
    let actual_hex = hex_sha256(bytes);
    if expected != actual_hex {
        return Err(OciError::Digest {
            path: path.to_owned(),
            expected: descriptor.digest().to_string(),
            actual: format!("sha256:{actual_hex}"),
        });
    }
    Ok(())
}

pub(super) fn verify_reader(
    descriptor: &Descriptor,
    reader: &mut dyn Read,
    path: &str,
) -> Result<(), OciError> {
    let Some(expected) = descriptor.as_digest_sha256() else {
        return Err(OciError::DigestAlgorithm {
            path: path.to_owned(),
            digest: descriptor.digest().to_string(),
        });
    };
    let embedded = decode_descriptor_data(descriptor, path)?;
    if let Some(bytes) = &embedded
        && u64::try_from(bytes.len()).unwrap_or(u64::MAX) != descriptor.size()
    {
        return Err(image_error(
            format!("{path}.data"),
            format!(
                "decodes to {} bytes but descriptor.size is {}",
                bytes.len(),
                descriptor.size()
            ),
        ));
    }
    let read_limit = descriptor_read_limit(descriptor, path)?;
    let mut bounded = reader.take(read_limit);
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while let count @ 1.. = bounded
        .read(&mut buffer)
        .map_err(|source| layer_io_error(path, source))?
    {
        if let Some(bytes) = &embedded {
            let offset = usize::try_from(actual_size)
                .map_err(|_| image_error(format!("{path}.data"), "comparison offset overflow"))?;
            let end = offset.saturating_add(count);
            if bytes.get(offset..end) != Some(&buffer[..count]) {
                return Err(image_error(
                    format!("{path}.data"),
                    "decoded bytes do not equal the target content",
                ));
            }
        }
        actual_size = actual_size
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| image_error(path, "content size overflow"))?;
        hasher.update(&buffer[..count]);
    }
    let actual_digest = Digest::try_from(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
        .expect("a SHA-256 result is always a valid OCI digest");
    if descriptor.size() != actual_size {
        return Err(OciError::Size {
            path: path.to_owned(),
            expected: descriptor.size(),
            actual: actual_size,
        });
    }
    if expected != actual_digest.digest() {
        return Err(OciError::Digest {
            path: path.to_owned(),
            expected: descriptor.digest().to_string(),
            actual: actual_digest.to_string(),
        });
    }
    Ok(())
}

fn descriptor_read_limit(descriptor: &Descriptor, path: &str) -> Result<u64, OciError> {
    descriptor
        .size()
        .checked_add(1)
        .ok_or_else(|| image_error(format!("{path}.size"), "cannot be bounded by size + 1"))
}

fn verify_embedded_bytes(
    descriptor: &Descriptor,
    bytes: &[u8],
    path: &str,
) -> Result<(), OciError> {
    if let Some(embedded) = decode_descriptor_data(descriptor, path)?
        && embedded != bytes
    {
        return Err(image_error(
            format!("{path}.data"),
            "decoded bytes do not equal the target content",
        ));
    }
    Ok(())
}

fn decode_descriptor_data(
    descriptor: &Descriptor,
    path: &str,
) -> Result<Option<Vec<u8>>, OciError> {
    descriptor
        .data()
        .as_deref()
        .map(|encoded| {
            let expected_encoded_size =
                descriptor
                    .size()
                    .div_ceil(3)
                    .checked_mul(4)
                    .ok_or_else(|| {
                        image_error(format!("{path}.data"), "encoded size calculation overflow")
                    })?;
            if u64::try_from(encoded.len()).unwrap_or(u64::MAX) != expected_encoded_size {
                return Err(image_error(
                    format!("{path}.data"),
                    format!(
                        "has {} encoded bytes; descriptor.size {} requires {expected_encoded_size}",
                        encoded.len(),
                        descriptor.size()
                    ),
                ));
            }
            decode_base64(encoded).map_err(|reason| image_error(format!("{path}.data"), reason))
        })
        .transpose()
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    let input = encoded.as_bytes();
    if !input.len().is_multiple_of(4) {
        return Err("must be padded RFC 4648 base64".to_owned());
    }
    let mut decoded = Vec::with_capacity(input.len() / 4 * 3);
    for (chunk_index, chunk) in input.chunks_exact(4).enumerate() {
        let last = chunk_index + 1 == input.len() / 4;
        let first = base64_value(chunk[0])?;
        let second = base64_value(chunk[1])?;
        decoded.push((first << 2) | (second >> 4));
        match (chunk[2], chunk[3]) {
            (b'=', b'=') if last && second.trailing_zeros() >= 4 => {}
            (third, b'=') if last => {
                let third = base64_value(third)?;
                if third.trailing_zeros() < 2 {
                    return Err("has non-zero padding bits".to_owned());
                }
                decoded.push((second << 4) | (third >> 2));
            }
            (b'=', _) => return Err("has invalid padding".to_owned()),
            (third, fourth) => {
                let third = base64_value(third)?;
                let fourth = base64_value(fourth)?;
                decoded.push((second << 4) | (third >> 2));
                decoded.push((third << 6) | fourth);
            }
        }
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!("contains invalid base64 byte 0x{byte:02x}")),
    }
}

#[cfg(test)]
fn digest_stream<R: Read + ?Sized>(reader: &mut R, path: &str) -> Result<(u64, Digest), OciError> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| layer_io_error(path, source))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| image_error(path, "content size overflow"))?;
        hasher.update(&buffer[..count]);
    }
    let digest = Digest::try_from(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
        .expect("a SHA-256 result is always a valid OCI digest");
    Ok((size, digest))
}

pub(super) fn descriptor_for_bytes(media_type: MediaType, bytes: &[u8]) -> Descriptor {
    let size = u64::try_from(bytes.len()).expect("usize always fits in u64 on supported targets");
    Descriptor::new(
        media_type,
        size,
        Digest::try_from(format!("sha256:{}", hex_sha256(bytes)))
            .expect("a SHA-256 result is always a valid OCI digest"),
    )
}

pub(super) fn hex_sha256(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

pub(super) fn lowercase_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

pub(super) fn layer_io_error(path: &str, source: io::Error) -> OciError {
    OciError::Io {
        path: path.to_owned(),
        source,
    }
}
