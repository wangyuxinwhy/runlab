use std::path::Path;

use anyhow::Result;

use crate::integrity::{digest_bytes, read_bounded_file, write_new_output};
use crate::runtime::RuntimeConfig;
use crate::topology::ManagedServiceFile;

use super::image::resolve_image;
use super::{
    ContentSummary, ManagedServiceCheckResult, ManagedServiceCommand, RuntimeConfigCheckResult,
    RuntimeConfigCommand, RuntimeConfigCreateResult, absolute_path, emit, image_service,
};

pub(super) fn run_runtime_config(state: &Path, command: RuntimeConfigCommand) -> Result<u8> {
    match command {
        RuntimeConfigCommand::Create { image, output } => {
            let images = image_service(state)?;
            let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
            let manifest_digest = resolved.manifest.digest;
            let runtime =
                RuntimeConfig::from_image_config(&images.image_config(&manifest_digest)?)?;
            let bytes = runtime.encoded()?;
            write_new_output(&output, &bytes)?;
            emit(&RuntimeConfigCreateResult {
                schema_version: 1,
                requested_reference,
                manifest_digest,
                output: absolute_path(&output)?,
                size: bytes.len(),
            })?;
        }
        RuntimeConfigCommand::Check { path } => return check_runtime_config(&path),
    }
    Ok(0)
}

pub(super) fn check_runtime_config(path: &Path) -> Result<u8> {
    let bytes = read_bounded_file(path, 16 * 1024 * 1024)?;
    let runtime = RuntimeConfig::load(&bytes)?;
    emit(&RuntimeConfigCheckResult {
        schema_version: 1,
        valid: true,
        oci_version: runtime.oci_version().to_owned(),
    })?;
    Ok(0)
}

pub(super) fn run_managed_service(state: &Path, command: ManagedServiceCommand) -> Result<u8> {
    let ManagedServiceCommand::Check { path } = command;
    let service = ManagedServiceFile::load(&path)?;
    let runtime_source = read_bounded_file(&service.runtime_config_file, 16 * 1024 * 1024)?;
    let runtime = RuntimeConfig::load(&runtime_source)?;
    runtime.validate_native_managed_profile()?;
    let runtime_bytes = runtime.encoded()?;
    let images = image_service(state)?;
    let (image, requested_reference) =
        resolve_image(state, &images, &service.initial_image.parse()?)?;
    emit(&ManagedServiceCheckResult {
        schema_version: 1,
        valid: true,
        name: service.name,
        requested_reference,
        initial_image: image.manifest,
        runtime_config: ContentSummary {
            digest: digest_bytes(&runtime_bytes),
            size: runtime_bytes.len(),
        },
        readiness: service.readiness,
    })?;
    Ok(0)
}
