use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{ArgGroup, Args, Subcommand};

use crate::image::{ImageSelector, Images};
use crate::run::RunId;
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
    /// Export a Catalog or Final Image as a standard OCI Image Layout archive.
    #[command(
        group = ArgGroup::new("source").required(true).multiple(false),
        after_long_help = "Examples:\n  runlab image export --image all --output ./all.oci.tar\n  runlab image export --run 550e8400-e29b-41d4-a716-446655440000 --output ./final.oci.tar"
    )]
    Export(ImageExportArgs),
}

#[derive(Debug, Args)]
pub(super) struct ImageExportArgs {
    /// Catalog name or complete sha256 Manifest digest.
    #[arg(long, group = "source")]
    image: Option<ImageSelector>,
    /// Persistent Run whose Final Image is exported.
    #[arg(long, group = "source")]
    run: Option<RunId>,
    /// Program whose Final Image is exported; defaults to primary with --run.
    #[arg(long, requires = "run")]
    program: Option<String>,
    /// New uncompressed OCI archive path; an existing path is never overwritten.
    #[arg(long)]
    output: PathBuf,
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
        ImageCommand::Export(arguments) => {
            let result = match (arguments.image, arguments.run) {
                (Some(image), None) => images.export(&image, &arguments.output)?,
                (None, Some(run_id)) => {
                    let run_id = run_id.to_string();
                    let record = state.database().run_get(&run_id)?.ok_or_else(|| {
                        crate::error::classify(
                            anyhow::anyhow!("Run does not exist: {run_id}"),
                            crate::error::ErrorFacts::before_run(
                                crate::error::ErrorCategory::NotFound,
                                "run_lookup",
                            ),
                        )
                    })?;
                    let manifest = crate::filesystem::final_environment(
                        &record,
                        arguments.program.as_deref().unwrap_or("primary"),
                    )?;
                    images.export_manifest(manifest, &arguments.output)?
                }
                _ => unreachable!("Clap requires exactly one Image export source"),
            };
            emit(&result)?;
        }
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
        ImageCommand::Export(arguments) => {
            let image = arguments.image.map(|value| value.to_string());
            let run = arguments.run.map(|value| value.to_string());
            return super::emit_forwarded(&vm.forward_image_export(
                image.as_deref(),
                run.as_deref(),
                arguments.program.as_deref(),
                &arguments.output,
            )?);
        }
    };
    super::emit_forwarded(&output)
}
