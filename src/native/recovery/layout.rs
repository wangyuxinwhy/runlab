use std::fs;

use crate::integrity::{set_private_file, sync_directory};

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::core::RunId;
use crate::integrity::ensure_private_directory;

use super::journal::{parse_recovery_entry_name, validate_journal};
use super::{
    MAX_JOURNAL_BYTES, NativeRecoveryJournal, NativeRecoveryPhase, NativeSharedNetworkPhase,
};

pub(super) fn try_lock(file: &File, run_id: RunId) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => bail!("native recovery attempt is active: {run_id}"),
        Err(TryLockError::Error(error)) => {
            Err(error).context("failed to lock native recovery attempt")
        }
    }
}

pub(super) fn ensure_real_private_directory(path: &Path) -> Result<()> {
    let mut created = false;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("failed to create directory {}", path.display()))?;
            created = true;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect directory {}", path.display()));
        }
    }
    set_private_directory(path)?;
    if created {
        sync_directory(path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn create_private_directory(path: &Path) -> Result<()> {
    create_private_directory_entry(path)?;
    set_private_directory(path)
}

pub(super) fn create_private_directory_entry(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))
}

pub(super) fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

pub(super) fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect directory {}", path.display()))?;
    validate_directory_metadata(path, &metadata)?;
    #[cfg(unix)]
    verify_mode(path, 0o700)?;
    Ok(())
}

pub(super) fn validate_recovery_workspace(workspace: &Path) -> Result<()> {
    validate_workspace_layout(workspace)
}

pub(super) fn validate_managed_workspace(workspace: &Path) -> Result<()> {
    validate_workspace_layout(workspace)
}

pub(super) fn validate_workspace_layout(workspace: &Path) -> Result<()> {
    validate_directory(workspace)?;
    for path in [
        workspace.join("lower"),
        workspace.join("lower/rootfs"),
        workspace.join("bundle"),
        workspace.join("bundle/rootfs"),
        workspace.join("overlay"),
        workspace.join("overlay/upper"),
        workspace.join("overlay/work"),
        workspace.join("runtime"),
    ] {
        validate_optional_directory(&path)?;
    }
    Ok(())
}

pub(super) fn validate_optional_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect directory {}", path.display()))
        }
    }
}

pub(super) fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!(
            "native recovery path must not be a symbolic link: {}",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "native recovery path must be a directory: {}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn validate_staging_directory(directory: &Path) -> Result<()> {
    validate_directory(directory)?;
    validate_same_mount(
        directory
            .parent()
            .context("native recovery staging directory has no parent")?,
        directory,
    )?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("native recovery staging directory name is invalid")?;
    let (run_id, staging) = parse_recovery_entry_name(name)?;
    if !staging {
        bail!("native recovery staging directory has a published name");
    }
    for entry in fs::read_dir(directory).context("failed to inspect native recovery staging")? {
        let entry = entry.context("failed to read native recovery staging entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("native recovery staging entry name is not UTF-8"))?;
        let path = entry.path();
        match name.as_str() {
            "workspace" => validate_staging_workspace(&path)?,
            "lock" | "stdout" | "stderr" | "managed-service-stdout" | "managed-service-stderr" => {
                validate_same_mount(directory, &path)?;
                validate_empty_staging_file(&path)?;
            }
            "journal.json" => {
                validate_same_mount(directory, &path)?;
                validate_staging_journal(&path, run_id)?;
            }
            _ => bail!("unexpected native recovery staging entry: {name}"),
        }
    }
    Ok(())
}

pub(super) fn validate_staging_journal(path: &Path, run_id: RunId) -> Result<()> {
    validate_bounded_staging_file(path, MAX_JOURNAL_BYTES)?;
    let bytes = fs::read(path).context("failed to read native recovery staging journal")?;
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return Ok(());
    }
    let journal: NativeRecoveryJournal = serde_json::from_slice(&bytes)
        .context("complete native recovery staging journal is invalid")?;
    validate_journal(&journal, run_id)?;
    validate_pristine_prepublication_journal(&journal)
}

pub(super) fn validate_pristine_prepublication_journal(
    journal: &NativeRecoveryJournal,
) -> Result<()> {
    let primary_has_facts = journal.process.is_some()
        || journal.stdout.is_some()
        || journal.stderr.is_some()
        || journal.captured_at.is_some()
        || journal.final_image.is_some()
        || journal.terminal_at.is_some()
        || !journal.operation_errors.is_empty();
    let service_has_facts = journal.managed_service.as_ref().is_some_and(|service| {
        service.phase != NativeRecoveryPhase::PreAcceptance
            || service.readiness.is_some()
            || service.process.is_some()
            || service.stdout.is_some()
            || service.stderr.is_some()
            || service.captured_at.is_some()
            || service.final_image.is_some()
            || !service.operation_errors.is_empty()
    });
    let network_has_facts = journal.shared_network.as_ref().is_some_and(|network| {
        network.phase != NativeSharedNetworkPhase::PlanPending
            || network.plan.is_some()
            || network.facts.is_some()
            || network.holder_pid.is_some()
            || network.holder_start_time_ticks.is_some()
            || network.holder_exit_observed_at.is_some()
    });
    if journal.generation != 1
        || journal.phase != NativeRecoveryPhase::PreAcceptance
        || primary_has_facts
        || service_has_facts
        || network_has_facts
        || journal.backend.run_network.is_some()
    {
        bail!("native recovery staging journal contains published resource state");
    }
    Ok(())
}

pub(super) fn validate_staging_workspace(workspace: &Path) -> Result<()> {
    validate_directory(workspace)?;
    validate_same_mount(
        workspace
            .parent()
            .context("native recovery staging workspace has no parent")?,
        workspace,
    )?;
    for entry in
        fs::read_dir(workspace).context("failed to inspect native recovery staging workspace")?
    {
        let entry = entry.context("failed to read native recovery staging workspace entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("native recovery staging entry name is not UTF-8"))?;
        if name != "managed-service" {
            bail!("unexpected native recovery staging workspace entry: {name}");
        }
        let managed = entry.path();
        validate_directory(&managed)?;
        validate_same_mount(workspace, &managed)?;
        if fs::read_dir(&managed)
            .context("failed to inspect native recovery staging Managed Service workspace")?
            .next()
            .transpose()
            .context("failed to read native recovery staging Managed Service workspace")?
            .is_some()
        {
            bail!("native recovery staging Managed Service workspace is not empty");
        }
    }
    Ok(())
}

pub(super) fn validate_same_mount(parent: &Path, child: &Path) -> Result<()> {
    if mount_id(parent)? != mount_id(child)? {
        bail!(
            "native recovery staging crosses a mount boundary: {}",
            child.display()
        );
    }
    Ok(())
}

pub(super) fn mount_id(path: &Path) -> Result<u64> {
    use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

    let status = statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::MNT_ID)
        .with_context(|| format!("failed to inspect mount identity for {}", path.display()))?;
    if status.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        bail!(
            "mount identity is unavailable for native recovery staging: {}",
            path.display()
        );
    }
    Ok(status.stx_mnt_id)
}

pub(super) fn validate_empty_staging_file(path: &Path) -> Result<()> {
    validate_bounded_staging_file(path, 0)
}

pub(super) fn validate_bounded_staging_file(path: &Path, maximum: u64) -> Result<()> {
    validate_regular_file(path)?;
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to inspect native recovery staging file {}",
            path.display()
        )
    })?;
    if metadata.len() > maximum {
        bail!(
            "native recovery staging file exceeds {maximum} bytes: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.nlink() != 1 {
            bail!(
                "native recovery staging file has unexpected link count: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub(super) fn cleanup_staging_directory(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(_) => validate_staging_directory(directory)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).context("failed to inspect native recovery staging directory");
        }
    }
    for name in [
        "lock",
        "stdout",
        "stderr",
        "managed-service-stdout",
        "managed-service-stderr",
        "journal.json",
    ] {
        remove_optional_file(&directory.join(name))?;
    }
    remove_optional_empty_directory(&directory.join("workspace/managed-service"))?;
    remove_optional_empty_directory(&directory.join("workspace"))?;
    fs::remove_dir(directory).with_context(|| {
        format!(
            "failed to remove native recovery staging directory {}",
            directory.display()
        )
    })?;
    sync_directory(
        directory
            .parent()
            .context("native recovery staging directory has no parent")?,
    )
}

pub(super) fn remove_optional_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove staging file {}", path.display()))
        }
    }
}

pub(super) fn remove_optional_empty_directory(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove staging directory {}", path.display())),
    }
}

pub(super) fn set_private_directory(path: &Path) -> Result<()> {
    ensure_private_directory(path)?;
    #[cfg(unix)]
    verify_mode(path, 0o700)?;
    Ok(())
}

pub(super) fn create_private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create private file {}", path.display()))?;
    set_private_file(&file)?;
    Ok(file)
}

pub(super) fn open_private_file(path: &Path) -> Result<File> {
    validate_regular_file(path)?;
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};

        let descriptor = open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open private file {}", path.display()))?;
        File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open private file {}", path.display()))?;
    Ok(file)
}

pub(super) fn validate_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect private file {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "native recovery path must not be a symbolic link: {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "native recovery path must be a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    verify_mode(path, 0o600)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn verify_mode(path: &Path, expected: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let actual = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect permissions for {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if actual != expected {
        bail!(
            "native recovery path has mode {actual:o}, expected {expected:o}: {}",
            path.display()
        );
    }
    Ok(())
}
