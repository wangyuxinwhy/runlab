use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

#[cfg(not(target_os = "macos"))]
use crate::state::State;

const MAX_OBSERVATION_DOCUMENT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Subcommand)]
pub(super) enum ObservationCommand {
    /// Register, discover, and inspect immutable Observation Types.
    Type {
        #[command(subcommand)]
        command: ObservationTypeCommand,
    },
    /// Validate and append one immutable Observation to a terminal Run.
    #[command(
        long_about = "Validate an Observation payload against its registered Type and append it to a terminal Run. The caller supplies a canonical Observation UUID, so retrying the same document is idempotent. RunLab trusts the declared Method; source discovery and interpretation remain that Method's responsibility.",
        after_long_help = "WORKFLOW:\n  1. Discover or register the Type.\n  2. Use a Method to derive a payload.\n  3. Submit one Observation document.\n  4. Query common columns and use SQLite json_extract(payload, ...) for Type-specific fields.\n\nEXAMPLE:\n  runlab observation submit --document observation.json\n  runlab query run \"SELECT observation_id, type, json_extract(payload, '$.score') AS score FROM observations WHERE state = 'active'\""
    )]
    Submit {
        /// Exact Observation JSON document; use - for stdin, at most 1 MiB.
        #[arg(long, value_name = "FILE")]
        document: PathBuf,
    },
    /// Append a reasoned retraction for one active Observation.
    Retract {
        /// Exact Observation retraction JSON document; use - for stdin, at most 1 MiB.
        #[arg(long, value_name = "FILE")]
        document: PathBuf,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub(super) enum ObservationTypeCommand {
    /// Register one immutable Type definition; an identical retry is idempotent.
    Register {
        /// Exact Observation Type JSON document; use - for stdin, at most 1 MiB.
        #[arg(long, value_name = "FILE")]
        document: PathBuf,
    },
    /// Inspect one registered Type definition.
    Get {
        /// Versioned Type identity in namespace/name@vN form.
        #[arg(value_name = "TYPE")]
        observation_type: String,
    },
    /// List registered Types in stable identity order.
    List {
        /// Maximum number of Types returned; must be between 1 and 1000.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this Type identity.
        #[arg(long)]
        after: Option<String>,
    },
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(state_path: &std::path::Path, command: &ObservationCommand) -> Result<u8> {
    match command {
        ObservationCommand::Type { command } => match command {
            ObservationTypeCommand::Register { document } => {
                let bytes = read_document(document, "Observation Type document")?;
                let document =
                    crate::observation::parse_type_definition(&bytes).map_err(|error| {
                        crate::error::invalid_input(error, "observation_type_register")
                    })?;
                let state = State::open(state_path)?;
                super::emit(&crate::observation::register_type(
                    state.database(),
                    &document,
                )?)?;
            }
            ObservationTypeCommand::Get { observation_type } => {
                let state = State::open(state_path)?;
                super::emit(&crate::observation::get_type(
                    state.database(),
                    observation_type,
                )?)?;
            }
            ObservationTypeCommand::List { limit, after } => {
                let state = State::open(state_path)?;
                super::emit(&crate::observation::list_types(
                    state.database(),
                    *limit,
                    after.as_deref(),
                )?)?;
            }
        },
        ObservationCommand::Submit { document } => {
            let bytes = read_document(document, "Observation document")?;
            let document = crate::observation::parse_submission(&bytes)
                .map_err(|error| crate::error::invalid_input(error, "observation_input"))?;
            let state = State::open(state_path)?;
            super::emit(&crate::observation::submit(state.database(), &document)?)?;
        }
        ObservationCommand::Retract { document } => {
            let bytes = read_document(document, "Observation retraction document")?;
            let document = crate::observation::parse_retraction(&bytes)
                .map_err(|error| crate::error::invalid_input(error, "observation_input"))?;
            let state = State::open(state_path)?;
            super::emit(&crate::observation::retract(state.database(), &document)?)?;
        }
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: &ObservationCommand) -> Result<u8> {
    let vm = crate::managed_vm::ManagedVm::new();
    let output = match command {
        ObservationCommand::Type { command } => match command {
            ObservationTypeCommand::Register { document } => {
                let bytes = read_document(document, "Observation Type document")?;
                crate::observation::parse_type_definition(&bytes).map_err(|error| {
                    crate::error::invalid_input(error, "observation_type_register")
                })?;
                vm.forward_observation_document(&["type", "register"], &bytes)?
            }
            ObservationTypeCommand::Get { observation_type } => {
                vm.forward_observation_type_get(observation_type)?
            }
            ObservationTypeCommand::List { limit, after } => {
                vm.forward_observation_type_list(*limit, after.as_deref())?
            }
        },
        ObservationCommand::Submit { document } => {
            let bytes = read_document(document, "Observation document")?;
            crate::observation::parse_submission(&bytes)
                .map_err(|error| crate::error::invalid_input(error, "observation_input"))?;
            vm.forward_observation_document(&["submit"], &bytes)?
        }
        ObservationCommand::Retract { document } => {
            let bytes = read_document(document, "Observation retraction document")?;
            crate::observation::parse_retraction(&bytes)
                .map_err(|error| crate::error::invalid_input(error, "observation_input"))?;
            vm.forward_observation_document(&["retract"], &bytes)?
        }
    };
    super::emit_forwarded(&output)
}

fn read_document(source: &std::path::Path, label: &str) -> Result<Vec<u8>> {
    super::input::read_bounded(source, MAX_OBSERVATION_DOCUMENT_BYTES, label)
        .map_err(|error| crate::error::invalid_input(error, "observation_input"))
}
