#[cfg(not(target_os = "macos"))]
use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

#[cfg(not(target_os = "macos"))]
use crate::state::State;

#[derive(Debug, Subcommand)]
pub(super) enum SchemaCommand {
    /// List public SQL Relations and their descriptions.
    List,
    /// Get one public SQL Relation's columns and semantics.
    Get {
        /// Public Relation name discovered with schema list.
        object: String,
        /// Omit descriptions and return names, types, and nullability only.
        #[arg(long)]
        compact: bool,
    },
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(state_path: &Path, command: SchemaCommand) -> Result<u8> {
    let state = State::open(state_path)?;
    let (object, include_descriptions) = match command {
        SchemaCommand::List => (None, true),
        SchemaCommand::Get { object, compact } => (Some(object), !compact),
    };
    let report = state.database().with_connection(|connection| {
        crate::public_schema::report(connection, object.as_deref(), include_descriptions)
    })?;
    super::emit(&report)?;
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: SchemaCommand) -> Result<u8> {
    let vm = crate::managed_vm::ManagedVm::new();
    let output = match command {
        SchemaCommand::List => vm.forward_schema_list()?,
        SchemaCommand::Get { object, compact } => vm.forward_schema_get(&object, compact)?,
    };
    super::emit_forwarded(&output)
}
