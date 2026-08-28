use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;

use crate::image::{ImageSelector, Images};
use crate::state::State;

use super::{MetadataArgs, emit};

#[derive(Debug, Subcommand)]
pub(super) enum ImageCommand {
    /// Import one standard OCI Image Layout directory or archive as a local Catalog entry.
    #[command(
        long_about = "Import one standard OCI Image Layout directory or archive, verify and store its complete OCI content, then assign a local Catalog name. Optional description and labels belong to the mutable Catalog entry: they help Agents select the Image, are not verified Image capabilities, and do not change the OCI Manifest digest.",
        after_long_help = "Examples:\n  runlab image import ./python-uv.oci --name python-uv --description 'Python 3.12 + uv; no Agent installed'\n  runlab image import ./agent.oci --name pi-swebench --label runtime=python --label agent=pi"
    )]
    Import {
        /// OCI Image Layout directory or uncompressed tar archive containing one Image Manifest.
        source: PathBuf,
        /// Local name assigned after the complete Image is verified and stored.
        #[arg(long)]
        name: String,
        #[command(flatten)]
        metadata: MetadataArgs,
    },
    /// List local Image Catalog entries and their metadata in stable order.
    List {
        /// Maximum number of Image Catalog entries returned; must be between 1 and 1000.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this Image name.
        #[arg(long)]
        after: Option<String>,
    },
    /// Resolve and inspect one Image; Catalog metadata is present only when selected by name.
    Get {
        /// Local Image name or complete sha256 Manifest digest.
        image: ImageSelector,
    },
}

pub(super) fn execute(state_path: &Path, command: ImageCommand) -> Result<u8> {
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    match command {
        ImageCommand::Import {
            source,
            name,
            metadata,
        } => emit(&images.import(&source, &name, &metadata.resolve()?)?)?,
        ImageCommand::List { limit, after } => emit(&images.list(limit, after.as_deref())?)?,
        ImageCommand::Get { image } => emit(&images.get(&image)?)?,
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: ImageCommand) -> Result<u8> {
    let vm = crate::managed_vm::ManagedVm::new();
    let output = match command {
        ImageCommand::Import {
            source,
            name,
            metadata,
        } => vm.forward_image_import(&source, &name, &metadata.resolve()?)?,
        ImageCommand::List { limit, after } => vm.forward_image_list(limit, after.as_deref())?,
        ImageCommand::Get { image } => vm.forward_image_get(&image.to_string())?,
    };
    super::emit_forwarded(&output)
}
