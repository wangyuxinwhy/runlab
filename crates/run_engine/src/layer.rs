use std::io::Read;

use flate2::read::MultiGzDecoder;
use oci_spec::image::MediaType;

/// Error returned while selecting or initializing an OCI Layer decoder.
#[derive(Debug, thiserror::Error)]
pub enum LayerDecodeError {
    /// The descriptor does not use one of the six supported OCI Layer media types.
    #[error("unsupported OCI Layer media type: {0}")]
    UnsupportedMediaType(String),
    /// A compressed stream decoder could not be initialized.
    #[error("failed to initialize OCI Layer decoder: {0}")]
    Initialization(#[source] std::io::Error),
}

/// Returns the complete OCI Layer media-type set understood by the Engine.
#[must_use]
pub fn supported_layer_media_types() -> &'static [MediaType] {
    const MEDIA_TYPES: &[MediaType] = &[
        MediaType::ImageLayer,
        MediaType::ImageLayerGzip,
        MediaType::ImageLayerZstd,
        MediaType::ImageLayerNonDistributable,
        MediaType::ImageLayerNonDistributableGzip,
        MediaType::ImageLayerNonDistributableZstd,
    ];
    MEDIA_TYPES
}

/// Wraps exact Layer content in the decoder selected by its OCI media type.
///
/// Gzip decoding consumes every concatenated member, matching OCI `DiffID`
/// verification and filesystem materialization semantics.
///
/// # Errors
///
/// Returns [`LayerDecodeError`] for an unsupported media type or decoder
/// initialization failure. Stream corruption is reported by the returned reader.
pub fn decode_layer<'a>(
    media_type: &MediaType,
    content: impl Read + 'a,
) -> Result<Box<dyn Read + 'a>, LayerDecodeError> {
    match media_type {
        MediaType::ImageLayer | MediaType::ImageLayerNonDistributable => Ok(Box::new(content)),
        MediaType::ImageLayerGzip | MediaType::ImageLayerNonDistributableGzip => {
            Ok(Box::new(MultiGzDecoder::new(content)))
        }
        MediaType::ImageLayerZstd | MediaType::ImageLayerNonDistributableZstd => Ok(Box::new(
            zstd::stream::read::Decoder::new(content).map_err(LayerDecodeError::Initialization)?,
        )),
        media_type => Err(LayerDecodeError::UnsupportedMediaType(
            media_type.to_string(),
        )),
    }
}
