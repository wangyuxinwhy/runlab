//! Linux-private OCI root filesystem materialization and stopped-tree capture.
//!
//! A caller must keep the workspace private, remove all runtime mounts before
//! capture, and remove the workspace only after proving that no mount remains.
//! In particular, [`Rootfs`] does not recursively clean anything from `Drop` or
//! from a failed materialization attempt.

use std::error::Error as StdError;
use std::fmt;
use std::fs::File;

use anyhow::Result;
use oci_spec::image::{Descriptor, Digest, MediaType};

use crate::oci::{ImagePlan, VerifiedImage};

/// Stable crate-private classification for rootfs materialization failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootfsErrorKind {
    InvalidInput,
    UnsupportedInput,
    Content,
    Internal,
}

/// A classified rootfs materialization failure with its full causal chain.
#[derive(Debug)]
pub(crate) struct RootfsError {
    kind: RootfsErrorKind,
    source: anyhow::Error,
}

impl RootfsError {
    fn new(kind: RootfsErrorKind, source: anyhow::Error) -> Self {
        Self { kind, source }
    }

    pub(crate) const fn kind(&self) -> RootfsErrorKind {
        self.kind
    }

    pub(crate) const fn cause(&self) -> &anyhow::Error {
        &self.source
    }
}

impl fmt::Display for RootfsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if formatter.alternate() {
            write!(formatter, "{:#}", self.source)
        } else {
            fmt::Display::fmt(&self.source, formatter)
        }
    }
}

impl StdError for RootfsError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Exact verified input for one ordered OCI Image Layer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VerifiedLayer<'a> {
    pub(crate) descriptor: &'a Descriptor,
    pub(crate) expected_diff_id: &'a Digest,
}

pub(crate) fn cached_image_validation(
    cache_root: &std::path::Path,
    image: &ImagePlan,
) -> Result<Option<Vec<u64>>> {
    platform::cached_image_validation(cache_root, image)
}

pub(crate) fn record_image_validation(
    cache_root: &std::path::Path,
    image: &VerifiedImage,
) -> Result<()> {
    platform::record_image_validation(cache_root, image)
}

/// Explicit resource limits shared by materialization and capture.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RootfsLimits {
    pub(crate) layers: u64,
    pub(crate) entries: u64,
    pub(crate) total_compressed_bytes: u64,
    pub(crate) total_uncompressed_bytes: u64,
    pub(crate) entry_bytes: u64,
    pub(crate) path_bytes: u64,
    pub(crate) total_path_bytes: u64,
    pub(crate) link_target_bytes: u64,
    pub(crate) xattr_names_bytes: usize,
    pub(crate) xattr_value_bytes: usize,
    pub(crate) extension_bytes: u64,
    pub(crate) total_xattr_bytes: u64,
    pub(crate) total_content_bytes: u64,
    pub(crate) tar_bytes: u64,
    pub(crate) pending_hardlinks: u64,
    pub(crate) depth: u64,
    pub(crate) cleanup_entries: u64,
}

impl Default for RootfsLimits {
    fn default() -> Self {
        Self {
            layers: 1_024,
            entries: 1_000_000,
            total_compressed_bytes: 64 * 1024 * 1024 * 1024,
            total_uncompressed_bytes: 64 * 1024 * 1024 * 1024,
            entry_bytes: 64 * 1024 * 1024 * 1024,
            path_bytes: 16 * 1024,
            total_path_bytes: 1024 * 1024 * 1024,
            link_target_bytes: 16 * 1024,
            xattr_names_bytes: 1024 * 1024,
            xattr_value_bytes: 16 * 1024 * 1024,
            extension_bytes: 1024 * 1024,
            total_xattr_bytes: 1024 * 1024 * 1024,
            total_content_bytes: 64 * 1024 * 1024 * 1024,
            tar_bytes: 64 * 1024 * 1024 * 1024,
            pending_hardlinks: 1_000_000,
            depth: 1_024,
            cleanup_entries: 1_000_000,
        }
    }
}

/// Deterministic, uncompressed OCI Layer produced from a stopped rootfs.
#[derive(Debug)]
pub(crate) struct CapturedLayer {
    pub(crate) media_type: MediaType,
    pub(crate) path: tempfile::TempPath,
    pub(crate) size: u64,
    pub(crate) diff_id: Digest,
}

impl CapturedLayer {
    pub(crate) fn open(&self) -> Result<File> {
        File::open(&self.path).map_err(Into::into)
    }
}

#[path = "rootfs/linux.rs"]
mod platform;

pub(crate) use platform::Rootfs;
#[cfg(test)]
pub(crate) use platform::{MaterializationFault, with_materialization_fault};
