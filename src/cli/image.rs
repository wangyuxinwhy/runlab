use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::catalog::{
    CatalogDescriptionUpdate, ImageSelector, LocalImageCatalog, normalize_reference,
};
use crate::core::{Architecture, ImageView, Platform};
use crate::docker::DockerImageAdapter;
use crate::image::ImageService;
use crate::integrity::ensure_private_directory;
use crate::oci::OciLayout;
use crate::state::StateOperation;

use super::{
    CheckoutCommand, DockerCommand, DockerImageCheckoutCreateResult, DockerImageCommand,
    DockerImageMaterializeResult, ImageCatalogCommand, ImageCatalogListResult,
    ImageCatalogRemoveResult, ImageCatalogSetResult, ImageCatalogShowResult, ImageCommand,
    ImageDiffResult, ImageExportFormat, ImageExportResult, ImageFileCommand, ImageFileGetResult,
    ImageFilesystemDiffResult, ImageImportResult, ImageInspectResult, ImageOperationResult,
    ImagePullResult, ResolvedImageResult, absolute_path, emit, image_service, local_docker,
};

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
