//! The hidden control surface the host uses to drive a guest `runlab` inside
//! the managed VM.
//!
//! These subcommands are not part of the product's interface; they are the
//! wire format of the host-to-guest protocol, which is why `managed_vm`
//! declares their names and this module only parses arguments and prints
//! results. Everything here is `hide = true` and reachable only from the host
//! side of a VM operation.

use anyhow::{Context, Result};
use clap::Subcommand;
use uuid::Uuid;

use crate::managed_vm::{self, command as vm_command};

use super::emit;

#[derive(Debug, Subcommand)]
pub(super) enum GuestCommand {
    #[command(name = vm_command::HANDSHAKE, hide = true)]
    Handshake,
    #[command(name = vm_command::PREPARE, hide = true)]
    Prepare {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        input_identities: String,
        #[arg(long)]
        runtime_config_inputs: String,
        #[arg(long)]
        output_count: usize,
        #[arg(last = true)]
        argv: Vec<String>,
    },
    #[command(name = vm_command::SEAL_INPUTS, hide = true)]
    SealInputs {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = vm_command::START, hide = true)]
    Start {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = vm_command::STATUS, hide = true)]
    Status {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = vm_command::CANCEL, hide = true)]
    Cancel {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = vm_command::DISCARD, hide = true)]
    Discard {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = vm_command::FILE_INFO, hide = true)]
    FileInfo {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        index: usize,
    },
    #[command(name = vm_command::READ_FILE, hide = true)]
    ReadFile {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        index: usize,
    },
    #[command(name = vm_command::READ_STREAM, hide = true)]
    ReadStream {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        stream: String,
    },
    #[command(name = vm_command::STREAM_INFO, hide = true)]
    StreamInfo {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        stream: String,
    },
    #[command(name = vm_command::REMOVE, hide = true)]
    Remove {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = vm_command::ABANDON, hide = true)]
    Abandon {
        #[arg(long)]
        operation_id: Uuid,
    },
}

pub(super) fn run_guest(command: GuestCommand) -> Result<u8> {
    match command {
        GuestCommand::Handshake => emit(&managed_vm::guest_handshake()).map(|()| 0),
        GuestCommand::Prepare {
            operation_id,
            namespace,
            input_identities,
            runtime_config_inputs,
            output_count,
            argv,
        } => prepare(
            operation_id,
            &namespace,
            &input_identities,
            &runtime_config_inputs,
            output_count,
            argv,
        ),
        GuestCommand::SealInputs { operation_id } => {
            managed_vm::guest_seal_inputs(operation_id).map(|()| 0)
        }
        GuestCommand::Start { operation_id } => managed_vm::guest_start(operation_id).map(|()| 0),
        GuestCommand::Status { operation_id } => {
            emit(&managed_vm::guest_status(operation_id)?)?;
            Ok(0)
        }
        GuestCommand::Cancel { operation_id } => {
            emit(&managed_vm::guest_cancel(operation_id)?)?;
            Ok(0)
        }
        GuestCommand::Discard { operation_id } => {
            emit(&managed_vm::guest_discard(operation_id)?)?;
            Ok(0)
        }
        GuestCommand::FileInfo {
            operation_id,
            kind,
            index,
        } => {
            emit(&managed_vm::guest_file_info(operation_id, &kind, index)?)?;
            Ok(0)
        }
        GuestCommand::ReadFile {
            operation_id,
            kind,
            index,
        } => {
            managed_vm::guest_read_file(operation_id, &kind, index)?;
            Ok(0)
        }
        GuestCommand::ReadStream {
            operation_id,
            stream,
        } => {
            managed_vm::guest_read_stream(operation_id, &stream)?;
            Ok(0)
        }
        GuestCommand::StreamInfo {
            operation_id,
            stream,
        } => {
            emit(&managed_vm::guest_stream_info(operation_id, &stream)?)?;
            Ok(0)
        }
        GuestCommand::Remove { operation_id } => {
            managed_vm::guest_remove(operation_id)?;
            Ok(0)
        }
        GuestCommand::Abandon { operation_id } => {
            managed_vm::guest_abandon(operation_id)?;
            Ok(0)
        }
    }
}

fn prepare(
    operation_id: Uuid,
    namespace: &str,
    input_identities: &str,
    runtime_config_inputs: &str,
    output_count: usize,
    argv: Vec<String>,
) -> Result<u8> {
    managed_vm::guest_prepare(
        operation_id,
        namespace,
        serde_json::from_str(input_identities).context("invalid VM input identities")?,
        serde_json::from_str(runtime_config_inputs)
            .context("invalid VM Runtime Config input slots")?,
        output_count,
        argv,
    )?;
    Ok(0)
}
