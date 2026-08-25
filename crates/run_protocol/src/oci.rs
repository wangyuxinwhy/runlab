use std::fmt;

use oci_spec::image::{Descriptor, MediaType};
use thiserror::Error;

/// A Descriptor that does not identify an OCI Image Manifest.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("expected an OCI Image Manifest descriptor, received {media_type}")]
pub struct ImageDescriptorError {
    media_type: String,
}

impl ImageDescriptorError {
    /// Returns the media type that failed the Image Manifest requirement.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }
}

/// A complete OCI Descriptor that points to an Image Manifest.
#[derive(Clone, Eq, PartialEq)]
pub struct ImageDescriptor(Descriptor);

impl ImageDescriptor {
    /// Accepts a complete OCI Descriptor after verifying its target media type.
    ///
    /// # Errors
    ///
    /// Returns [`ImageDescriptorError`] unless the Descriptor targets an OCI
    /// Image Manifest.
    pub fn new(descriptor: Descriptor) -> Result<Self, ImageDescriptorError> {
        if descriptor.media_type() != &MediaType::ImageManifest {
            return Err(ImageDescriptorError {
                media_type: descriptor.media_type().to_string(),
            });
        }
        Ok(Self(descriptor))
    }

    /// Returns every required and optional field of the OCI Descriptor.
    #[must_use]
    pub fn as_oci(&self) -> &Descriptor {
        &self.0
    }

    /// Consumes the wrapper without discarding optional Descriptor fields.
    #[must_use]
    pub fn into_oci(self) -> Descriptor {
        self.0
    }
}

impl fmt::Debug for ImageDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageDescriptor")
            .field("media_type", self.0.media_type())
            .field("digest", self.0.digest())
            .field("size", &self.0.size())
            .field("urls", self.0.urls())
            .field("annotations", self.0.annotations())
            .field("platform", self.0.platform())
            .field("artifact_type", self.0.artifact_type())
            .field("has_embedded_data", &self.0.data().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use oci_spec::image::Descriptor;

    use super::*;

    #[test]
    fn complete_descriptor_preserves_every_optional_field() {
        let descriptor: Descriptor = serde_json::from_str(
            r#"{
                "mediaType":"application/vnd.oci.image.manifest.v1+json",
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size":123,
                "urls":["https://example.test/manifest"],
                "annotations":{"example":"value"},
                "platform":{"architecture":"arm64","os":"linux","variant":"v8"},
                "artifactType":"application/example",
                "data":"e30="
            }"#,
        )
        .expect("complete OCI Descriptor");
        let image = ImageDescriptor::new(descriptor.clone()).expect("Image Manifest");

        assert_eq!(image.into_oci(), descriptor);
    }

    #[test]
    fn non_manifest_descriptor_is_rejected_without_assuming_input_or_output_slot() {
        let descriptor: Descriptor = serde_json::from_str(
            r#"{
                "mediaType":"application/vnd.oci.image.config.v1+json",
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size":2
            }"#,
        )
        .expect("OCI Descriptor");

        let error = ImageDescriptor::new(descriptor).expect_err("not a Manifest");
        assert_eq!(
            error.media_type(),
            "application/vnd.oci.image.config.v1+json"
        );
    }
}
