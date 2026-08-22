use std::fs::File;
use std::path::Path;

use crate::integrity::{ensure_private_directory, open_regular_lock};
use anyhow::{Context, Result, bail};

const LOCK_FILE: &str = ".mutation.lock";

pub(crate) struct StateOperation {
    _lock: File,
}

pub(crate) struct StateMaintenance {
    _lock: File,
}

impl StateOperation {
    pub(crate) fn enter(state: &Path) -> Result<Self> {
        let lock = open_state_lock(state)?;
        File::lock_shared(&lock).context("failed to enter RunLab state operation")?;
        Ok(Self { _lock: lock })
    }

    pub(crate) fn enter_existing(state: &Path) -> Result<Self> {
        let lock = open_existing_state_lock(state)?;
        File::lock_shared(&lock).context("failed to enter RunLab state operation")?;
        Ok(Self { _lock: lock })
    }
}

impl StateMaintenance {
    pub(crate) fn enter_existing(state: &Path) -> Result<Self> {
        let lock = open_existing_state_lock(state)?;
        lock.lock()
            .context("failed to enter exclusive RunLab state maintenance")?;
        Ok(Self { _lock: lock })
    }
}

fn open_state_lock(state: &Path) -> Result<File> {
    ensure_private_directory(state)?;
    open_lock(state, true)
}

fn open_existing_state_lock(state: &Path) -> Result<File> {
    let metadata = std::fs::symlink_metadata(state)
        .with_context(|| format!("failed to inspect RunLab state {}", state.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("RunLab state is not a real directory: {}", state.display());
    }
    open_lock(state, false)
}

fn open_lock(state: &Path, create: bool) -> Result<File> {
    let path = state.join(LOCK_FILE);
    open_regular_lock(&path, create, "RunLab state lock")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn maintenance_waits_for_ordinary_state_operations() {
        let state = tempfile::tempdir().expect("state");
        let operation = StateOperation::enter(state.path()).expect("state operation");
        let state_path = state.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            started_tx.send(()).expect("started");
            let maintenance = StateMaintenance::enter_existing(&state_path).expect("maintenance");
            entered_tx.send(()).expect("entered");
            drop(maintenance);
        });
        started_rx.recv().expect("thread started");
        assert!(entered_rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(operation);
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("maintenance entered after operation");
        thread.join().expect("maintenance thread");
    }
}
