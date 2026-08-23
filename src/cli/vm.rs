//! `runlab vm`: running Linux `RunLab` in a managed Lima VM, from both sides.
//!
//! `VmCommand` is what a user types on the host. `GuestCommand` is the hidden
//! control surface the host uses to drive the guest `runlab` inside the VM:
//! those subcommands are not part of the product's interface but the wire
//! format of the host-to-guest protocol, which is why `managed_vm` declares
//! their names and this module only parses arguments and prints results. They
//! are all `hide = true` and reachable only from the host side of an
//! operation.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use uuid::Uuid;

use crate::managed_vm::{self, HostVm, command as vm_command};

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

#[derive(Debug, Subcommand)]
pub(super) enum VmCommand {
    /// Create a same-architecture plain Lima VM with no host mounts.
    Create {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, default_value_t = 4)]
        cpus: u16,
        #[arg(long, default_value_t = 4)]
        memory_gib: u16,
        #[arg(long, default_value_t = 20)]
        disk_gib: u16,
    },
    /// Inspect the VM boundary, guest protocol, runtime, and reference-profile facts.
    Status {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Start and validate an existing plain, unmounted Lima VM.
    Start {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Install exact RunLab/runtime inputs and provision the rootful reference profile.
    Install {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, value_name = "LINUX_RUNLAB")]
        binary: PathBuf,
        #[arg(long, value_name = "LINUX_RUNC")]
        runc: PathBuf,
    },
    /// Execute one `RunLab` command in an isolated guest state namespace.
    Exec {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        namespace: String,
        /// Host file copied to the corresponding @input/N argument.
        #[arg(long, value_name = "HOST_FILE")]
        input: Vec<PathBuf>,
        /// Input slot containing an OCI Runtime Config whose @input/N mount sources are sealed in the guest.
        #[arg(long, value_name = "INDEX")]
        runtime_config_input: Vec<usize>,
        /// New host file copied from the corresponding @output/N argument.
        #[arg(long, value_name = "HOST_FILE")]
        output: Vec<PathBuf>,
        /// Return an operation identity without waiting for completion.
        #[arg(long)]
        detach: bool,
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Inspect, attach to, or cancel a recoverable guest operation.
    Operation {
        #[command(subcommand)]
        command: VmOperationCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum VmOperationCommand {
    /// Inspect an operation without changing it.
    Get {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
    },
    /// Wait, retrieve exact streams and declared outputs, then remove transport state.
    Attach {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, value_name = "HOST_FILE")]
        output: Vec<PathBuf>,
    },
    /// Deliver SIGINT through the explicit guest control path.
    Cancel {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
    },
    /// Remove terminal transport state without retrieving streams or outputs.
    Discard {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
    },
}

pub(super) fn run_vm(state: Option<&PathBuf>, command: VmCommand) -> Result<u8> {
    ensure_vm_owns_state(state)?;
    match command {
        VmCommand::Create {
            instance,
            cpus,
            memory_gib,
            disk_gib,
        } => {
            emit(&HostVm::new(instance.as_deref())?.create(cpus, memory_gib, disk_gib)?)?;
            Ok(0)
        }
        VmCommand::Status { instance } => {
            emit(&HostVm::new(instance.as_deref())?.status()?)?;
            Ok(0)
        }
        VmCommand::Start { instance } => {
            emit(&HostVm::new(instance.as_deref())?.start()?)?;
            Ok(0)
        }
        VmCommand::Install {
            instance,
            binary,
            runc,
        } => {
            emit(&HostVm::new(instance.as_deref())?.install(&binary, &runc)?)?;
            Ok(0)
        }
        VmCommand::Exec {
            instance,
            namespace,
            input,
            runtime_config_input,
            output,
            detach,
            argv,
        } => {
            let (started, attached) = HostVm::new(instance.as_deref())?.execute(
                &namespace,
                &input,
                &runtime_config_input,
                &output,
                &argv,
                detach,
            )?;
            let Some(attached) = attached else {
                emit(&started)?;
                return Ok(0);
            };
            std::io::stdout().lock().write_all(&attached.stdout)?;
            std::io::stderr().lock().write_all(&attached.stderr)?;
            let exit_code = attached.status.exit_code.unwrap_or(1);
            HostVm::new(instance.as_deref())?.complete(attached.operation_id)?;
            Ok(exit_code)
        }
        VmCommand::Operation { command } => match command {
            VmOperationCommand::Get {
                operation_id,
                instance,
            } => {
                emit(&HostVm::new(instance.as_deref())?.operation_status(operation_id)?)?;
                Ok(0)
            }
            VmOperationCommand::Attach {
                operation_id,
                instance,
                output,
            } => {
                let attached = HostVm::new(instance.as_deref())?.attach(operation_id, &output)?;
                std::io::stdout().lock().write_all(&attached.stdout)?;
                std::io::stderr().lock().write_all(&attached.stderr)?;
                let exit_code = attached.status.exit_code.unwrap_or(1);
                HostVm::new(instance.as_deref())?.complete(attached.operation_id)?;
                Ok(exit_code)
            }
            VmOperationCommand::Cancel {
                operation_id,
                instance,
            } => {
                emit(&HostVm::new(instance.as_deref())?.cancel(operation_id)?)?;
                Ok(0)
            }
            VmOperationCommand::Discard {
                operation_id,
                instance,
            } => {
                emit(&HostVm::new(instance.as_deref())?.discard(operation_id)?)?;
                Ok(0)
            }
        },
    }
}

pub(super) fn ensure_vm_owns_state(state: Option<&PathBuf>) -> Result<()> {
    if state.is_some() {
        bail!("vm commands do not accept host --state; use --namespace for guest state")
    }
    Ok(())
}
