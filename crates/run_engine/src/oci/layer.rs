use std::io::{self, Read, Seek as _, SeekFrom};

use oci_spec::image::{Descriptor, Digest, MediaType};
use sha2::{Digest as _, Sha256};

use super::content::{layer_io_error, lowercase_hex, verify_media_type, verify_reader};
use super::{ImageLimits, OciError, image_error, unsupported_error};
use crate::{LayerDecodeError, OciContentStore, decode_layer, supported_layer_media_types};

pub(super) fn preflight_layers(
    descriptors: &[Descriptor],
    limits: ImageLimits,
) -> Result<u64, OciError> {
    if descriptors.len() > limits.layers {
        return Err(unsupported_error(
            "manifest.layers",
            format!(
                "contains {} entries; limit is {}",
                descriptors.len(),
                limits.layers
            ),
        ));
    }
    let mut total_compressed = 0_u64;
    for (index, descriptor) in descriptors.iter().enumerate() {
        let path = format!("manifest.layers[{index}]");
        verify_media_type(descriptor, supported_layer_media_types(), &path)?;
        total_compressed = total_compressed
            .checked_add(descriptor.size())
            .ok_or_else(|| image_error("manifest.layers", "compressed size overflow"))?;
        if total_compressed > limits.compressed_layer_bytes {
            return Err(OciError::CompressedLayerLimit {
                path: "manifest.layers".to_owned(),
                limit: limits.compressed_layer_bytes,
                actual: total_compressed,
            });
        }
    }
    Ok(total_compressed)
}

pub(super) fn verify_layer(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    expected: &Digest,
    path: &str,
    uncompressed_limit: u64,
) -> Result<u64, OciError> {
    verify_media_type(descriptor, supported_layer_media_types(), path)?;
    if expected.algorithm().as_ref() != "sha256" {
        return Err(unsupported_error(path, "only sha256 DiffIDs are supported"));
    }
    let mut content = store.open(descriptor).map_err(|source| OciError::Content {
        operation: "open",
        path: path.to_owned(),
        source,
    })?;
    verify_reader(descriptor, &mut content, path)?;
    content
        .seek(SeekFrom::Start(0))
        .map_err(|source| layer_io_error(path, source))?;
    let mut bounded_content = content.take(descriptor.size());
    let (uncompressed, actual) = layer_diff_id(
        descriptor.media_type(),
        &mut bounded_content,
        path,
        uncompressed_limit,
    )?;
    if &actual != expected {
        return Err(OciError::DiffId {
            path: path.to_owned(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(uncompressed)
}

fn layer_diff_id(
    media_type: &MediaType,
    content: &mut dyn Read,
    path: &str,
    uncompressed_limit: u64,
) -> Result<(u64, Digest), OciError> {
    let mut reader = decode_layer(media_type, content).map_err(|error| match error {
        LayerDecodeError::UnsupportedMediaType(actual) => OciError::MediaType {
            path: path.to_owned(),
            expected: supported_layer_media_types()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            actual,
        },
        LayerDecodeError::Initialization(source) => layer_decode_error(path, source),
    })?;
    digest_stream_limited(&mut reader, path, uncompressed_limit)
}

pub(super) fn digest_stream_limited<R: Read + ?Sized>(
    reader: &mut R,
    path: &str,
    limit: u64,
) -> Result<(u64, Digest), OciError> {
    let observation_limit = limit
        .checked_add(1)
        .ok_or_else(|| image_error(path, "streaming limit cannot be bounded by limit + 1"))?;
    let mut bounded = reader.take(observation_limit);
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = bounded
            .read(&mut buffer)
            .map_err(|source| layer_io_error(path, source))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| image_error(path, "content size overflow"))?;
        if size > limit {
            return Err(OciError::LayerLimit {
                path: path.to_owned(),
                limit,
                actual: size,
            });
        }
        hasher.update(&buffer[..count]);
    }
    let digest = Digest::try_from(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
        .expect("a SHA-256 result is always a valid OCI digest");
    Ok((size, digest))
}

fn layer_decode_error(path: &str, source: io::Error) -> OciError {
    OciError::LayerDecode {
        path: path.to_owned(),
        source,
    }
}
