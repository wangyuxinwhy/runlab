use std::path::Path;

use anyhow::{Result, bail};

use crate::catalog::{CatalogMetadata, LocalImageCatalog, normalize_reference};
use crate::core::{OciDescriptor, Platform};
use crate::distribution::{DistributionClient, DistributionPullResult, RemoteReference};
use crate::image::ImageService;
use crate::ingress::{ImportSourceKind, ingest_image};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImagePullResult {
    pub(crate) remote_reference: String,
    pub(crate) source_index: Option<OciDescriptor>,
    pub(crate) selected_manifest: OciDescriptor,
    pub(crate) platform: Platform,
    pub(crate) downloaded_blobs: u64,
    pub(crate) downloaded_bytes: u64,
    pub(crate) local_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageImportResult {
    pub(crate) source_kind: ImportSourceKind,
    pub(crate) source_index: OciDescriptor,
    pub(crate) selected_manifest: OciDescriptor,
    pub(crate) platform: Platform,
    pub(crate) verified_blobs: u64,
    pub(crate) verified_bytes: u64,
    pub(crate) local_reference: String,
}

pub(crate) struct ImageIngress<'a> {
    images: &'a ImageService,
}

impl<'a> ImageIngress<'a> {
    #[must_use]
    pub(crate) const fn new(images: &'a ImageService) -> Self {
        Self { images }
    }

    pub(crate) fn pull(
        &self,
        remote: &str,
        platform: Platform,
        local_reference: Option<&str>,
        description: Option<&str>,
    ) -> Result<ImagePullResult> {
        let client = DistributionClient::https()?;
        self.pull_with(&client, remote, platform, local_reference, description)
    }

    pub(crate) fn pull_with(
        &self,
        client: &DistributionClient,
        remote: &str,
        platform: Platform,
        local_reference: Option<&str>,
        description: Option<&str>,
    ) -> Result<ImagePullResult> {
        let remote = RemoteReference::parse(remote)?;
        let local_reference = normalize_reference(
            &local_reference.map_or_else(|| remote.default_local_reference(), ToOwned::to_owned),
        )?;
        let DistributionPullResult {
            remote_reference,
            source_index,
            selected_manifest,
            source,
            downloaded_blobs,
            downloaded_bytes,
        } = client.pull(self.images.layout(), &remote, platform)?;
        let image = self.images.inspect(&selected_manifest.digest)?;
        if image.manifest != selected_manifest {
            bail!("selected OCI Manifest changed during local verification");
        }
        if image.platform != platform {
            bail!(
                "selected OCI Manifest platform mismatch: expected {platform}, received {}",
                image.platform
            );
        }
        LocalImageCatalog::new(self.images.layout()).upsert(
            &local_reference,
            &selected_manifest,
            platform,
            &CatalogMetadata {
                description: description.map(ToOwned::to_owned),
                source: Some(source),
                maintainer: Some("local".to_owned()),
            },
        )?;
        Ok(ImagePullResult {
            remote_reference,
            source_index,
            selected_manifest,
            platform,
            downloaded_blobs,
            downloaded_bytes,
            local_reference,
        })
    }

    pub(crate) fn import(
        &self,
        source: &Path,
        platform: Platform,
        manifest: Option<&crate::core::Digest>,
        source_reference: Option<&str>,
        local_reference: &str,
        description: Option<&str>,
    ) -> Result<ImageImportResult> {
        let local_reference = normalize_reference(local_reference)?;
        let ingested = ingest_image(
            self.images.layout(),
            source,
            platform,
            manifest,
            source_reference,
        )?;
        let image = self.images.inspect(&ingested.selected_manifest.digest)?;
        if image.manifest != ingested.selected_manifest {
            bail!("selected OCI Manifest changed during local verification");
        }
        if image.platform != platform {
            bail!(
                "selected OCI Manifest platform mismatch: expected {platform}, received {}",
                image.platform
            );
        }
        let source_kind = match ingested.source_kind {
            ImportSourceKind::Layout => "oci-layout",
            ImportSourceKind::Archive => "oci-archive",
        };
        LocalImageCatalog::new(self.images.layout()).upsert(
            &local_reference,
            &ingested.selected_manifest,
            platform,
            &CatalogMetadata {
                description: description.map(ToOwned::to_owned),
                source: Some(format!("{source_kind}@{}", ingested.source_index.digest)),
                maintainer: Some("local".to_owned()),
            },
        )?;
        Ok(ImageImportResult {
            source_kind: ingested.source_kind,
            source_index: ingested.source_index,
            selected_manifest: ingested.selected_manifest,
            platform,
            verified_blobs: ingested.verified_blobs,
            verified_bytes: ingested.verified_bytes,
            local_reference,
        })
    }
}
