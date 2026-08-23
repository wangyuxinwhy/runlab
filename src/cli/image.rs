//! `runlab image` and `runlab docker`: what is in the local OCI Layout.
//!
//! Acquiring an Image, naming it in the catalog, inspecting or diffing what is
//! already there, and the Docker adapter's Image side. Every decision about
//! content belongs to `image` and `ingress`; this module turns arguments into
//! one of their calls and prints the result.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::Serialize;

use crate::catalog::{
    CatalogDescriptionUpdate, CatalogEntry, ImageSelector, LocalImageCatalog, normalize_reference,
};
use crate::core::{Architecture, Digest, ImageView, NetworkControl, OciDescriptor, Platform};
use crate::docker::{DockerBackend, DockerImageAdapter};
use crate::image::{ImageService, ImageStructureDiff};
use crate::ingress::local::ImportSourceKind;
use crate::ingress::{
    ImageImportResult as IngressImportResult, ImagePullResult as IngressPullResult,
};
use crate::integrity::ensure_private_directory;
use crate::oci::OciLayout;
use crate::render::FilesystemChange;
use crate::state::StateOperation;

use super::{absolute_path, emit, image_service, resolve_state};

pub(super) fn run_image(state: &Path, command: ImageCommand) -> Result<u8> {
    if let ImageCommand::Import { source, .. } = &command {
        crate::ingress::local::validate_source_destination(source, &state.join("oci"))?;
    }
    let _operation = enter_image_state(state, &command)?;
    match command {
        ImageCommand::Import {
            source,
            platform,
            manifest,
            source_reference,
            name,
            description,
        } => {
            let platform = match platform {
                Some(platform) => platform.into(),
                None => host_platform()?,
            };
            let images = image_service(state)?;
            let result = crate::ingress::ImageIngress::new(&images).import(
                &source,
                platform,
                manifest.as_ref(),
                source_reference.as_deref(),
                &name,
                description.as_deref(),
            )?;
            emit(&ImageImportResult::from(result))?;
        }
        ImageCommand::Pull {
            remote_reference,
            platform,
            name,
            description,
        } => {
            let platform = match platform {
                Some(platform) => platform.into(),
                None => host_platform()?,
            };
            let images = image_service(state)?;
            let result = crate::ingress::ImageIngress::new(&images).pull(
                &remote_reference,
                platform,
                name.as_deref(),
                description.as_deref(),
            )?;
            emit(&ImagePullResult::from(result))?;
        }
        ImageCommand::Catalog { command } => run_image_catalog(state, command)?,
        ImageCommand::Inspect { image } => {
            let images = image_service(state)?;
            let (image, _) = resolve_image(state, &images, &image)?;
            emit(&ImageInspectResult::from(image))?;
        }
        ImageCommand::Diff {
            from,
            to,
            limit,
            after_path_hex,
        } => run_image_diff(state, &from, &to, limit, after_path_hex.as_deref())?,
        ImageCommand::Export { image, output } => {
            let images = image_service(state)?;
            let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
            let manifest_digest = resolved.manifest.digest;
            let (digest, size) = images.export_tar(&manifest_digest, &output)?;
            emit(&ImageExportResult {
                schema_version: 1,
                requested_reference,
                manifest_digest,
                output: absolute_path(&output)?,
                digest,
                size,
                format: ImageExportFormat::Tar,
            })?;
        }
        ImageCommand::File { command } => match command {
            ImageFileCommand::Get {
                image,
                source,
                output,
            } => {
                let images = image_service(state)?;
                let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
                let manifest_digest = resolved.manifest.digest;
                let (digest, size) = images.get_file(&manifest_digest, &source, &output)?;
                emit(&ImageFileGetResult {
                    schema_version: 1,
                    requested_reference,
                    manifest_digest,
                    source,
                    output: absolute_path(&output)?,
                    digest,
                    size,
                })?;
            }
        },
    }
    Ok(0)
}

pub(super) fn enter_image_state(state: &Path, command: &ImageCommand) -> Result<StateOperation> {
    if matches!(
        command,
        ImageCommand::Import { .. }
            | ImageCommand::Pull { .. }
            | ImageCommand::Catalog {
                command: ImageCatalogCommand::Set { .. } | ImageCatalogCommand::Remove { .. }
            }
    ) {
        StateOperation::enter(state)
    } else {
        StateOperation::enter_existing(state)
    }
}

pub(super) fn run_docker(state: &Path, command: DockerCommand) -> Result<u8> {
    match command {
        DockerCommand::Image { command } => run_docker_image(state, command),
    }
}

pub(super) fn run_docker_image(state: &Path, command: DockerImageCommand) -> Result<u8> {
    let images = image_service(state)?;
    let docker = local_docker()?;
    let adapter = DockerImageAdapter::new(&images, &docker);
    match command {
        DockerImageCommand::Import { docker_image } => {
            emit(&ImageOperationResult::from(adapter.import(&docker_image)?))?;
        }
        DockerImageCommand::Materialize { manifest_digest } => {
            let docker_image = adapter.materialize(&manifest_digest)?;
            emit(&DockerImageMaterializeResult {
                schema_version: 1,
                manifest_digest,
                docker_image,
            })?;
        }
        DockerImageCommand::Checkout { command } => match command {
            CheckoutCommand::Create { manifest_digest } => {
                let (container, parent) = adapter.create_checkout(&manifest_digest)?;
                emit(&DockerImageCheckoutCreateResult {
                    schema_version: 1,
                    checkout_id: container.clone(),
                    parent_manifest: parent,
                    exec_argv: vec![
                        "docker".to_owned(),
                        "exec".to_owned(),
                        "-it".to_owned(),
                        container,
                        "/bin/sh".to_owned(),
                    ],
                })?;
            }
            CheckoutCommand::Commit { checkout_id } => emit(&ImageOperationResult::from(
                adapter.freeze_checkout(&checkout_id)?,
            ))?,
        },
    }
    Ok(0)
}

pub(super) fn run_image_catalog(state: &Path, command: ImageCatalogCommand) -> Result<()> {
    let layout = catalog_layout(state)?;
    let catalog = LocalImageCatalog::new(&layout);
    match command {
        ImageCatalogCommand::List { limit, after } => {
            if !(1..=1000).contains(&limit) {
                bail!("--limit must be between 1 and 1000");
            }
            let after = after.as_deref().map(normalize_reference).transpose()?;
            let mut entries = catalog
                .list()?
                .into_iter()
                .filter(|entry| {
                    after
                        .as_ref()
                        .is_none_or(|after| entry.reference.as_str() > after.as_str())
                })
                .take(limit + 1)
                .collect::<Vec<_>>();
            let has_more = entries.len() > limit;
            if has_more {
                entries.truncate(limit);
            }
            let next_after = has_more
                .then(|| entries.last().map(|entry| entry.reference.clone()))
                .flatten();
            emit(&ImageCatalogListResult {
                schema_version: 1,
                entries: entries.into_iter().map(Into::into).collect(),
                next_after,
            })?;
        }
        ImageCatalogCommand::Show { reference } => {
            let reference = normalize_reference(&reference)?;
            let entry = catalog
                .resolve(&reference)?
                .with_context(|| format!("local OCI reference is unknown: {reference}"))?;
            let image = image_service(state)?.inspect(&entry.manifest.digest)?;
            if image.manifest != entry.manifest {
                bail!("Catalog descriptor does not match resolved OCI Manifest: {reference}");
            }
            emit(&ImageCatalogShowResult {
                schema_version: 1,
                entry: entry.into(),
            })?;
        }
        ImageCatalogCommand::Set {
            reference,
            manifest,
            description,
            clear_description,
        } => {
            let reference = normalize_reference(&reference)?;
            let image = image_service(state)?.inspect(&manifest)?;
            let description = match (clear_description, description.as_deref()) {
                (true, _) => CatalogDescriptionUpdate::Clear,
                (false, Some(description)) => CatalogDescriptionUpdate::Set(description),
                (false, None) => CatalogDescriptionUpdate::Preserve,
            };
            let update = catalog.set(&reference, &image.manifest, image.platform, description)?;
            emit(&ImageCatalogSetResult {
                schema_version: 1,
                changed: update.changed,
                previous: update.previous.map(Into::into),
                entry: update.entry.into(),
            })?;
        }
        ImageCatalogCommand::Remove { reference } => {
            let reference = normalize_reference(&reference)?;
            let previous = catalog.remove(&reference)?;
            emit(&ImageCatalogRemoveResult {
                schema_version: 1,
                reference,
                removed: previous.is_some(),
                previous: previous.map(Into::into),
            })?;
        }
    }
    Ok(())
}

pub(super) fn run_image_diff(
    state: &Path,
    from: &ImageSelector,
    to: &ImageSelector,
    limit: usize,
    after_path_hex: Option<&str>,
) -> Result<()> {
    if !(1..=1000).contains(&limit) {
        bail!("--limit must be between 1 and 1000");
    }
    let after_path_hex = after_path_hex.map(validate_path_hex_cursor).transpose()?;
    let images = image_service(state)?;
    let (from, from_requested_reference) = resolve_image(state, &images, from)?;
    let (to, to_requested_reference) = resolve_image(state, &images, to)?;
    let diff = images.diff(&from.manifest.digest, &to.manifest.digest)?;
    let total_changes = diff.filesystem.changes.len();
    let mut changes = diff
        .filesystem
        .changes
        .into_iter()
        .filter(|change| {
            after_path_hex
                .as_ref()
                .is_none_or(|after| change.path_hex.as_str() > after.as_str())
        })
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = changes.len() > limit;
    if has_more {
        changes.truncate(limit);
    }
    let next_after_path_hex = has_more
        .then(|| changes.last().map(|change| change.path_hex.clone()))
        .flatten();
    emit(&ImageDiffResult {
        schema_version: diff.schema_version,
        from: ResolvedImageResult {
            requested_reference: from_requested_reference,
            manifest: diff.from,
        },
        to: ResolvedImageResult {
            requested_reference: to_requested_reference,
            manifest: diff.to,
        },
        structure: diff.structure,
        filesystem: ImageFilesystemDiffResult {
            total_changes,
            changes,
            next_after_path_hex,
        },
    })?;
    Ok(())
}

pub(super) fn resolve_image(
    state: &Path,
    images: &ImageService,
    selector: &ImageSelector,
) -> Result<(ImageView, Option<String>)> {
    match selector {
        ImageSelector::Digest(digest) => Ok((images.inspect(digest)?, None)),
        ImageSelector::Reference(reference) => {
            let layout = catalog_layout(state)?;
            let entry = LocalImageCatalog::new(&layout)
                .resolve(reference)?
                .with_context(|| format!("local OCI reference is unknown: {reference}"))?;
            let image = images.inspect(&entry.manifest.digest)?;
            if image.manifest != entry.manifest {
                bail!("Catalog descriptor does not match resolved OCI Manifest: {reference}");
            }
            Ok((image, Some(reference.clone())))
        }
    }
}

pub(super) fn validate_path_hex_cursor(value: &str) -> Result<String> {
    if value.len() < 2
        || !value.len().is_multiple_of(2)
        || !value.starts_with("2f")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("--after-path-hex must be lowercase hex for an absolute raw path");
    }
    Ok(value.to_owned())
}

pub(super) fn catalog_layout(state: &Path) -> Result<OciLayout> {
    ensure_private_directory(state)?;
    OciLayout::open(state.join("oci"))
}

pub(super) fn host_platform() -> Result<Platform> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Platform::linux(Architecture::Amd64)),
        "aarch64" => Ok(Platform::linux(Architecture::Arm64)),
        architecture => {
            bail!("host architecture {architecture} has no default OCI platform; supply --platform")
        }
    }
}

#[derive(Debug, Subcommand)]
pub(super) enum ImageCommand {
    /// Import and verify one Image from an OCI Layout directory or plain tar archive.
    Import {
        #[arg(value_name = "SOURCE")]
        source: PathBuf,
        /// Exact Linux platform expected from the selected Image Manifest.
        #[arg(long, value_enum)]
        platform: Option<PlatformArg>,
        /// Exact reachable Image Manifest; required when a platform is ambiguous.
        #[arg(long, conflicts_with = "source_reference")]
        manifest: Option<Digest>,
        /// Exact `org.opencontainers.image.ref.name` on the source root index.
        #[arg(long, value_name = "SOURCE_REFERENCE", conflicts_with = "manifest")]
        source_reference: Option<String>,
        /// Local Catalog reference created or moved after complete verification.
        #[arg(long, value_name = "LOCAL_REFERENCE")]
        name: String,
        /// Mutable local Catalog description.
        #[arg(long)]
        description: Option<String>,
    },
    /// Pull and verify one remote OCI Image into the local Catalog.
    Pull {
        remote_reference: String,
        /// Exact Linux platform selected from an OCI Image Index.
        #[arg(long, value_enum)]
        platform: Option<PlatformArg>,
        /// Local Catalog reference; defaults to the remote repository and selector.
        #[arg(long, value_name = "LOCAL_REFERENCE")]
        name: Option<String>,
        /// Mutable local Catalog description.
        #[arg(long)]
        description: Option<String>,
    },
    /// List or resolve entries in the Local Image Catalog.
    Catalog {
        #[command(subcommand)]
        command: ImageCatalogCommand,
    },
    /// Verify and inspect one OCI Image selected by digest or local reference.
    Inspect {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
    },
    /// Compare OCI structure and the resolved filesystem of two Images.
    Diff {
        #[arg(value_name = "FROM_IMAGE")]
        from: ImageSelector,
        #[arg(value_name = "TO_IMAGE")]
        to: ImageSelector,
        /// Maximum filesystem changes returned in one response.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this raw absolute path encoded as lowercase hex.
        #[arg(long, value_name = "PATH_HEX")]
        after_path_hex: Option<String>,
    },
    /// Export the resolved filesystem as a deterministic plain tar archive.
    Export {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Read a regular file from an OCI Image.
    File {
        #[command(subcommand)]
        command: ImageFileCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum DockerCommand {
    /// Operate on OCI Images through the Docker compatibility adapter.
    Image {
        #[command(subcommand)]
        command: DockerImageCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum DockerImageCommand {
    /// Import one native Linux `DOCKER_IMAGE` as an OCI Image Manifest.
    Import { docker_image: String },
    /// Materialize an OCI Image in the disposable Docker cache.
    Materialize { manifest_digest: Digest },
    /// Author a new OCI Image through an ordinary mutable Docker container.
    Checkout {
        #[command(subcommand)]
        command: CheckoutCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum CheckoutCommand {
    /// Start a mutable checkout from `MANIFEST_DIGEST`.
    Create { manifest_digest: Digest },
    /// Commit `CHECKOUT_ID` as a new OCI Image Manifest.
    Commit { checkout_id: String },
}

#[derive(Debug, Subcommand)]
pub(super) enum ImageFileCommand {
    /// Copy SOURCE from an image digest or local reference to a new `--output`.
    Get {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
        source: String,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum ImageCatalogCommand {
    /// List Catalog entries in stable reference order.
    List {
        /// Maximum entries returned in one response.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this local reference.
        #[arg(long, value_name = "LOCAL_REFERENCE")]
        after: Option<String>,
    },
    /// Resolve one local reference and verify its complete OCI Image.
    Show {
        #[arg(value_name = "LOCAL_REFERENCE")]
        reference: String,
    },
    /// Create or move one Catalog reference to an existing verified Manifest.
    Set {
        #[arg(value_name = "LOCAL_REFERENCE")]
        reference: String,
        #[arg(value_name = "MANIFEST_DIGEST")]
        manifest: Digest,
        /// Set the mutable local description.
        #[arg(long, conflicts_with = "clear_description")]
        description: Option<String>,
        /// Remove the mutable local description.
        #[arg(long, conflicts_with = "description")]
        clear_description: bool,
    },
    /// Remove one Catalog reference without deleting OCI content.
    Remove {
        #[arg(value_name = "LOCAL_REFERENCE")]
        reference: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum PlatformArg {
    #[value(name = "linux/amd64")]
    LinuxAmd64,
    #[value(name = "linux/arm64")]
    LinuxArm64,
}

impl From<PlatformArg> for Platform {
    fn from(value: PlatformArg) -> Self {
        match value {
            PlatformArg::LinuxAmd64 => Self::linux(Architecture::Amd64),
            PlatformArg::LinuxArm64 => Self::linux(Architecture::Arm64),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageOperationResult {
    schema_version: u32,
    manifest: crate::core::OciDescriptor,
    platform: crate::core::Platform,
    config: crate::core::OciDescriptor,
    layers: Vec<Digest>,
    parent_manifest: Option<Digest>,
    added_layers: Vec<Digest>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageInspectResult {
    manifest: OciDescriptor,
    config: OciDescriptor,
    platform: Platform,
    layers: Vec<OciDescriptor>,
    diff_ids: Vec<Digest>,
    parent_manifest: Option<Digest>,
    added_layers: Vec<Digest>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImagePullResult {
    schema_version: u32,
    remote_reference: String,
    source_index: Option<OciDescriptor>,
    selected_manifest: OciDescriptor,
    platform: Platform,
    downloaded_blobs: u64,
    downloaded_bytes: u64,
    local_reference: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageImportResult {
    schema_version: u32,
    source_kind: ImportSourceKind,
    source_index: OciDescriptor,
    selected_manifest: OciDescriptor,
    platform: Platform,
    verified_blobs: u64,
    verified_bytes: u64,
    local_reference: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CatalogEntryResult {
    reference: String,
    name: String,
    tag: String,
    manifest: OciDescriptor,
    platform: Option<Platform>,
    description: Option<String>,
    source: Option<String>,
    maintainer: Option<String>,
}

impl From<CatalogEntry> for CatalogEntryResult {
    fn from(value: CatalogEntry) -> Self {
        let (name, tag) = value
            .reference
            .rsplit_once(':')
            .expect("Catalog references are normalized with an explicit tag");
        Self {
            reference: value.reference.clone(),
            name: name.to_owned(),
            tag: tag.to_owned(),
            manifest: value.manifest,
            platform: value.platform,
            description: value.metadata.description,
            source: value.metadata.source,
            maintainer: value.metadata.maintainer,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageCatalogListResult {
    schema_version: u32,
    entries: Vec<CatalogEntryResult>,
    next_after: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageCatalogShowResult {
    schema_version: u32,
    entry: CatalogEntryResult,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageCatalogSetResult {
    schema_version: u32,
    changed: bool,
    previous: Option<CatalogEntryResult>,
    entry: CatalogEntryResult,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageCatalogRemoveResult {
    schema_version: u32,
    reference: String,
    removed: bool,
    previous: Option<CatalogEntryResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ResolvedImageResult {
    requested_reference: Option<String>,
    manifest: OciDescriptor,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageFilesystemDiffResult {
    total_changes: usize,
    changes: Vec<FilesystemChange>,
    next_after_path_hex: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageDiffResult {
    schema_version: u32,
    from: ResolvedImageResult,
    to: ResolvedImageResult,
    structure: ImageStructureDiff,
    filesystem: ImageFilesystemDiffResult,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(super) enum ImageExportFormat {
    Tar,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageExportResult {
    schema_version: u32,
    requested_reference: Option<String>,
    manifest_digest: Digest,
    output: String,
    digest: Digest,
    size: u64,
    format: ImageExportFormat,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ImageFileGetResult {
    schema_version: u32,
    requested_reference: Option<String>,
    manifest_digest: Digest,
    source: String,
    output: String,
    digest: Digest,
    size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DockerImageMaterializeResult {
    schema_version: u32,
    manifest_digest: Digest,
    docker_image: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DockerImageCheckoutCreateResult {
    schema_version: u32,
    checkout_id: String,
    parent_manifest: Digest,
    exec_argv: Vec<String>,
}

impl From<IngressImportResult> for ImageImportResult {
    fn from(value: IngressImportResult) -> Self {
        Self {
            schema_version: 1,
            source_kind: value.source_kind,
            source_index: value.source_index,
            selected_manifest: value.selected_manifest,
            platform: value.platform,
            verified_blobs: value.verified_blobs,
            verified_bytes: value.verified_bytes,
            local_reference: value.local_reference,
        }
    }
}

impl From<IngressPullResult> for ImagePullResult {
    fn from(value: IngressPullResult) -> Self {
        Self {
            schema_version: 1,
            remote_reference: value.remote_reference,
            source_index: value.source_index,
            selected_manifest: value.selected_manifest,
            platform: value.platform,
            downloaded_blobs: value.downloaded_blobs,
            downloaded_bytes: value.downloaded_bytes,
            local_reference: value.local_reference,
        }
    }
}

impl From<ImageView> for ImageInspectResult {
    fn from(value: ImageView) -> Self {
        Self {
            manifest: value.manifest,
            config: value.config,
            platform: value.platform,
            layers: value.layers,
            diff_ids: value.diff_ids,
            parent_manifest: value.parent_manifest,
            added_layers: value.added_layers,
        }
    }
}

impl From<ImageView> for ImageOperationResult {
    fn from(value: ImageView) -> Self {
        Self {
            schema_version: 1,
            manifest: value.manifest,
            platform: value.platform,
            config: value.config,
            layers: value.layers.into_iter().map(|layer| layer.digest).collect(),
            parent_manifest: value.parent_manifest,
            added_layers: value.added_layers,
        }
    }
}

pub(super) fn run_docker_with_state(
    explicit: Option<PathBuf>,
    command: DockerCommand,
) -> Result<u8> {
    let state = resolve_state(explicit)?;
    let imports_image = matches!(
        command,
        DockerCommand::Image {
            command: DockerImageCommand::Import { .. }
        }
    );
    let _operation = if imports_image {
        StateOperation::enter(&state)?
    } else {
        StateOperation::enter_existing(&state)?
    };
    run_docker(&state, command)
}

pub(super) fn local_docker() -> Result<DockerBackend> {
    let docker = DockerBackend::discover()?;
    docker.preflight(NetworkControl::None)?;
    Ok(docker)
}
