use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;

use crate::image::{ImageSelector, Images};
use crate::state::State;

use super::emit;

#[derive(Debug, Subcommand)]
pub(super) enum ImageCommand {
    /// Import one standard OCI Image Layout directory or archive.
    Import {
        /// OCI Image Layout directory or uncompressed tar archive containing one Image Manifest.
        source: PathBuf,
        /// Local name assigned after the complete Image is verified and stored.
        #[arg(long)]
        name: String,
    },
    /// List local Image names in stable order.
    List {
        /// Maximum number of Image names returned; must be between 1 and 1000.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this Image name.
        #[arg(long)]
        after: Option<String>,
    },
    /// Resolve and inspect one Image selected by local name or Manifest digest.
    Get {
        /// Local Image name or complete sha256 Manifest digest.
        image: ImageSelector,
    },
}

pub(super) fn execute(state_path: &Path, command: ImageCommand) -> Result<u8> {
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    match command {
        ImageCommand::Import { source, name } => emit(&images.import(&source, &name)?)?,
        ImageCommand::List { limit, after } => emit(&images.list(limit, after.as_deref())?)?,
        ImageCommand::Get { image } => emit(&images.get(&image)?)?,
    }
    Ok(0)
}
