use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use crate::managed_vm::ManagedVm;
use crate::managed_vm::config::parse_document;

use super::{emit, input::read_bounded};

const MAX_VM_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Subcommand)]
pub(super) enum VmCommand {
    /// Create the fixed local Linux VM without starting it.
    Create {
        /// Virtual CPUs assigned to the VM.
        #[arg(long, default_value_t = 4)]
        cpus: u16,
        /// Memory assigned to the VM in GiB.
        #[arg(long, default_value_t = 4)]
        memory_gib: u16,
        /// Persistent VM disk size in GiB.
        #[arg(long, default_value_t = 20)]
        disk_gib: u16,
    },
    /// Start the existing local Linux VM.
    Start,
    /// Install and verify the bundled Linux `RunLab` execution appliance.
    Install,
    /// Stop the local Linux VM while preserving its disk.
    Stop,
    /// Report VM lifecycle and compatibility facts without changing it.
    Status,
    /// Inspect, validate, and apply declarative read-only Host shares.
    Config {
        #[command(subcommand)]
        command: VmConfigCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub(super) enum VmConfigCommand {
    /// Return the normalized share declaration currently applied to the VM.
    Get,
    /// Validate a complete share declaration and report whether it can be applied.
    Check {
        /// Complete JSON document, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        document: PathBuf,
    },
    /// Replace the complete share declaration on a stopped VM.
    Apply {
        /// Complete JSON document, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        document: PathBuf,
    },
}

pub(super) fn execute(command: VmCommand) -> Result<u8> {
    let vm = ManagedVm::new();
    let status = match command {
        VmCommand::Create {
            cpus,
            memory_gib,
            disk_gib,
        } => vm.create(cpus, memory_gib, disk_gib)?,
        VmCommand::Start => vm.start()?,
        VmCommand::Install => {
            emit(&vm.install()?)?;
            return Ok(0);
        }
        VmCommand::Stop => vm.stop()?,
        VmCommand::Status => vm.status()?,
        VmCommand::Config { command } => {
            match command {
                VmConfigCommand::Get => emit(&vm.config_get()?)?,
                VmConfigCommand::Check { document } => {
                    let bytes =
                        read_bounded(&document, MAX_VM_CONFIG_BYTES, "VM share configuration")?;
                    let document = parse_document(&bytes)?;
                    emit(&vm.config_check(document)?)?;
                }
                VmConfigCommand::Apply { document } => {
                    let bytes =
                        read_bounded(&document, MAX_VM_CONFIG_BYTES, "VM share configuration")?;
                    let document = parse_document(&bytes)?;
                    emit(&vm.config_apply(document)?)?;
                }
            }
            return Ok(0);
        }
    };
    emit(&status)?;
    Ok(0)
}
