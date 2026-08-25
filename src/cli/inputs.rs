//! `runlab runtime-config` and `runlab managed-service`: the two files a Run
//! takes as input.
//!
//! Both commands do the same thing to a different declaration -- write one or
//! check one -- and report its digest, so a caller can pin the exact bytes a
//! Run will accept.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;
use schemars::JsonSchema;
use serde::Serialize;

use crate::catalog::ImageSelector;
use crate::core::{Digest, NetworkControl, OciDescriptor, ServiceName, TcpReadinessCondition};
use crate::integrity::{digest_bytes, read_bounded_file, write_new_output};
use crate::runtime::RuntimeConfig;
use crate::topology::ManagedServiceFile;

use super::image::resolve_image;
use super::{NetworkArg, absolute_path, emit, image_service};

pub(super) fn run_runtime_config(state: &Path, command: RuntimeConfigCommand) -> Result<u8> {
    match command {
        RuntimeConfigCommand::Create {
            image,
            output,
            network,
        } => {
            let images = image_service(state)?;
            let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
            let manifest_digest = resolved.manifest.digest;
            let runtime = RuntimeConfig::from_image_config(
                &images.image_config(&manifest_digest)?,
                NetworkControl::from(network),
            )?;
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

#[derive(Debug, Subcommand)]
pub(super) enum RuntimeConfigCommand {
    /// Convert OCI Image defaults into an OCI Runtime config.json.
    Create {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
        /// Run network provisioning this config accompanies; egress inherits a Run-owned namespace.
        #[arg(long, value_enum, default_value_t = NetworkArg::None)]
        network: NetworkArg,
    },
    /// Validate an OCI Runtime config.json without selecting an execution backend.
    Check { path: PathBuf },
}

#[derive(Debug, Subcommand)]
pub(super) enum ManagedServiceCommand {
    /// Validate the declaration, OCI Image, Runtime Config, and TCP readiness condition.
    Check { path: PathBuf },
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct RuntimeConfigCreateResult {
    schema_version: u32,
    requested_reference: Option<String>,
    manifest_digest: Digest,
    output: String,
    size: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct RuntimeConfigCheckResult {
    schema_version: u32,
    valid: bool,
    oci_version: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ContentSummary {
    digest: Digest,
    size: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ManagedServiceCheckResult {
    schema_version: u32,
    valid: bool,
    name: ServiceName,
    requested_reference: Option<String>,
    initial_image: OciDescriptor,
    runtime_config: ContentSummary,
    readiness: TcpReadinessCondition,
}
