use anyhow::Result;
use clap::Subcommand;

use crate::managed_vm::ManagedVm;

use super::emit;

#[derive(Clone, Copy, Debug, Subcommand)]
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
    };
    emit(&status)?;
    Ok(0)
}
