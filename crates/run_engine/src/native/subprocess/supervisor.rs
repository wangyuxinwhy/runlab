use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult, anyhow, bail};
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitId, WaitIdOptions, kill_process_group, pidfd_open,
    pidfd_send_signal, waitid,
};

pub(in crate::native) const SUPERVISOR_REAP_LIMIT: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone)]
pub(in crate::native) struct InvocationSupervisor {
    state: Arc<Mutex<SupervisorState>>,
}

struct SupervisorState {
    next_id: u64,
    children: BTreeMap<u64, SupervisedChild>,
    #[cfg(test)]
    faults: SupervisorFaults,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "leader observation, leader KILL, group KILL, and reap are independent supervision proofs and must not be collapsed into one lifecycle label"
)]
struct SupervisedChild {
    child: Child,
    pid: Option<Pid>,
    pidfd: Option<OwnedFd>,
    stop_requested: bool,
    termination_started: bool,
    leader_exit_observed: bool,
    leader_kill_delivered: bool,
    group_kill_proved: bool,
    leader_reaped: bool,
}

fn progress_without_pidfd(
    entry: &mut SupervisedChild,
    inject_group_failure: bool,
) -> AnyResult<bool> {
    let pid = entry.pid.context(
        "stable pidfd and representable pid are unavailable; termination cannot be proved",
    )?;
    entry.leader_exit_observed |= waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
    )?
    .is_some();
    if entry.group_kill_proved {
        return Ok(entry.leader_kill_delivered);
    }
    let group_result = if inject_group_failure {
        Err(rustix::io::Errno::PERM)
    } else {
        kill_process_group(pid, Signal::KILL)
    };
    match group_result {
        Ok(()) => {
            entry.group_kill_proved = true;
            if !entry.leader_exit_observed {
                entry.leader_kill_delivered = true;
            }
            Ok(entry.leader_kill_delivered)
        }
        Err(rustix::io::Errno::SRCH) if entry.leader_exit_observed => {
            entry.group_kill_proved = true;
            Ok(false)
        }
        Err(error) => {
            bail!(
                "process-group KILL remains unproved while the unreaped leader retains its pid: {error}"
            );
        }
    }
}

fn stabilize_pidfd_identity(entry: &mut SupervisedChild) -> AnyResult<bool> {
    let pidfd = entry.pidfd.as_ref().expect("pidfd path selected");
    entry.leader_exit_observed |= waitid(
        WaitId::PidFd(pidfd.as_fd()),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
    )?
    .is_some();
    if !entry.leader_exit_observed && !entry.stop_requested {
        pidfd_send_signal(pidfd, Signal::STOP)
            .context("failed to deliver STOP through stable pidfd")?;
        entry.stop_requested = true;
    }
    if entry.leader_exit_observed {
        return Ok(true);
    }
    if entry.leader_kill_delivered {
        entry.leader_exit_observed |= waitid(
            WaitId::PidFd(pidfd.as_fd()),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )?
        .is_some();
        return Ok(entry.leader_exit_observed);
    }
    let status = waitid(
        WaitId::PidFd(pidfd.as_fd()),
        WaitIdOptions::EXITED
            | WaitIdOptions::STOPPED
            | WaitIdOptions::NOWAIT
            | WaitIdOptions::NOHANG,
    )?;
    entry.leader_exit_observed |= status.is_some_and(|status| status.exited());
    Ok(entry.leader_exit_observed || status.is_some_and(|status| status.stopped()))
}

#[derive(Clone, Copy, Debug)]
pub(in crate::native) struct SupervisorToken(u64);

#[derive(Debug)]
pub(in crate::native) enum SupervisorSpawnError {
    NotSpawned(anyhow::Error),
    SpawnedRegistered {
        token: SupervisorToken,
        error: anyhow::Error,
    },
}

impl std::fmt::Display for SupervisorSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSpawned(error) => write!(formatter, "child was not spawned: {error:#}"),
            Self::SpawnedRegistered { error, .. } => {
                write!(formatter, "child was spawned and registered: {error:#}")
            }
        }
    }
}

impl std::error::Error for SupervisorSpawnError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum SupervisorLifecycle {
    Reaped,
    KillDelivered {
        children: usize,
    },
    TerminationUnproven {
        kill_delivered: usize,
        unproved: usize,
    },
}

#[cfg(test)]
#[derive(Default)]
struct SupervisorFaults {
    wait: usize,
    pidfd_open: usize,
    pidfd_signal: usize,
    group_kill: usize,
}

impl InvocationSupervisor {
    pub(in crate::native) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SupervisorState {
                next_id: 0,
                children: BTreeMap::new(),
                #[cfg(test)]
                faults: SupervisorFaults::default(),
            })),
        }
    }

    pub(in crate::native) fn spawn(
        &self,
        command: &mut Command,
    ) -> Result<SupervisorToken, SupervisorSpawnError> {
        let id = {
            let mut state = self.state.lock().expect("supervisor mutex poisoned");
            let start = state.next_id;
            loop {
                let candidate = state.next_id;
                state.next_id = state.next_id.wrapping_add(1);
                if !state.children.contains_key(&candidate) {
                    break candidate;
                }
                if state.next_id == start {
                    return Err(SupervisorSpawnError::NotSpawned(anyhow!(
                        "supervisor id space is exhausted"
                    )));
                }
            }
        };
        let child = command
            .spawn()
            .map_err(anyhow::Error::from)
            .map_err(SupervisorSpawnError::NotSpawned)?;
        let raw_pid = child.id();
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        state.children.insert(
            id,
            SupervisedChild {
                child,
                pid: None,
                pidfd: None,
                stop_requested: false,
                termination_started: false,
                leader_exit_observed: false,
                leader_kill_delivered: false,
                group_kill_proved: false,
                leader_reaped: false,
            },
        );
        drop(state);
        let token = SupervisorToken(id);
        let Some(pid) = Pid::from_raw(raw_pid.cast_signed()) else {
            self.state
                .lock()
                .expect("supervisor mutex poisoned")
                .children
                .get_mut(&id)
                .expect("new supervisor entry")
                .termination_started = true;
            return Err(SupervisorSpawnError::SpawnedRegistered {
                token,
                error: anyhow!("child pid cannot be represented"),
            });
        };
        self.state
            .lock()
            .expect("supervisor mutex poisoned")
            .children
            .get_mut(&id)
            .expect("new supervisor entry")
            .pid = Some(pid);
        #[cfg(test)]
        {
            let mut state = self.state.lock().expect("supervisor mutex poisoned");
            if state.faults.pidfd_open > 0 {
                state.faults.pidfd_open -= 1;
                state
                    .children
                    .get_mut(&id)
                    .expect("registered supervisor entry")
                    .termination_started = true;
                return Err(SupervisorSpawnError::SpawnedRegistered {
                    token,
                    error: anyhow!("injected pidfd_open failure after child registration"),
                });
            }
        }
        let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                self.state
                    .lock()
                    .expect("supervisor mutex poisoned")
                    .children
                    .get_mut(&id)
                    .expect("registered supervisor entry")
                    .termination_started = true;
                return Err(SupervisorSpawnError::SpawnedRegistered {
                    token,
                    error: anyhow!(error)
                        .context("failed to open stable pidfd for registered supervisor child"),
                });
            }
        };
        self.state
            .lock()
            .expect("supervisor mutex poisoned")
            .children
            .get_mut(&id)
            .expect("registered supervisor entry")
            .pidfd = Some(pidfd);
        Ok(token)
    }

    pub(in crate::native) fn with_child<T>(
        &self,
        token: SupervisorToken,
        operation: impl FnOnce(&mut Child) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        let entry = state.children.get_mut(&token.0).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "unknown supervisor token")
        })?;
        operation(&mut entry.child)
    }

    pub(in crate::native) fn try_wait(
        &self,
        token: SupervisorToken,
    ) -> std::io::Result<Option<ExitStatus>> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        #[cfg(test)]
        if state.faults.wait > 0 {
            state.faults.wait -= 1;
            return Err(std::io::Error::other("injected supervisor wait failure"));
        }
        let entry = state.children.get_mut(&token.0).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "unknown supervisor token")
        })?;
        if entry.termination_started && !entry.group_kill_proved {
            if let Some(pidfd) = &entry.pidfd {
                entry.leader_exit_observed |= waitid(
                    WaitId::PidFd(pidfd.as_fd()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                )?
                .is_some();
            } else if let Some(pid) = entry.pid {
                entry.leader_exit_observed |= waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                )?
                .is_some();
            }
            return Ok(None);
        }
        let status = entry.child.try_wait()?;
        entry.leader_exit_observed |= status.is_some();
        entry.leader_reaped |= status.is_some();
        Ok(status)
    }

    pub(in crate::native) fn progress_kill(&self, token: SupervisorToken) -> AnyResult<bool> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        #[cfg(test)]
        if state.faults.pidfd_signal > 0 {
            state.faults.pidfd_signal -= 1;
            bail!("injected supervisor pidfd signal failure");
        }
        #[cfg(test)]
        let inject_group_failure = if state.faults.group_kill > 0 {
            state.faults.group_kill -= 1;
            true
        } else {
            false
        };
        #[cfg(not(test))]
        let inject_group_failure = false;
        let entry = state
            .children
            .get_mut(&token.0)
            .context("unknown supervisor token")?;
        if entry.leader_reaped {
            if !entry.termination_started
                || ((entry.leader_kill_delivered || entry.leader_exit_observed)
                    && entry.group_kill_proved)
            {
                return Ok(entry.leader_kill_delivered);
            }
            bail!("leader was reaped before process-group termination was proved");
        }
        entry.termination_started = true;
        if entry.pidfd.is_none() {
            return progress_without_pidfd(entry, inject_group_failure);
        }
        if entry.leader_kill_delivered && entry.group_kill_proved {
            return Ok(true);
        }
        if !stabilize_pidfd_identity(entry)? {
            return Ok(false);
        }
        let pidfd = entry.pidfd.as_ref().expect("pidfd path selected");
        #[cfg(test)]
        let group_result = if inject_group_failure {
            Err(rustix::io::Errno::PERM)
        } else {
            kill_process_group(
                entry.pid.context("registered child has no pid")?,
                Signal::KILL,
            )
        };
        #[cfg(not(test))]
        let group_result = kill_process_group(
            entry.pid.context("registered child has no pid")?,
            Signal::KILL,
        );
        let leader_result = if entry.leader_exit_observed || entry.leader_kill_delivered {
            Ok(())
        } else {
            pidfd_send_signal(pidfd, Signal::KILL)
        };
        if let Err(group_error) = group_result {
            if leader_result.is_ok() && !entry.leader_exit_observed {
                entry.leader_kill_delivered = true;
            }
            return match leader_result {
                Err(error) => Err(anyhow!(error)
                    .context("process-group KILL failed and stable leader KILL also failed")),
                Ok(()) if entry.leader_exit_observed => Err(anyhow!(
                    "process-group KILL remains unproved after the unreaped leader exited: {group_error}"
                )),
                Ok(()) => Err(anyhow!(
                    "stable leader KILL succeeded but process-group KILL remains unproved: {group_error}"
                )),
            };
        }
        leader_result.context("failed to deliver KILL through stable pidfd")?;
        entry.group_kill_proved = true;
        if !entry.leader_exit_observed {
            entry.leader_kill_delivered = true;
        }
        Ok(entry.leader_kill_delivered)
    }

    pub(in crate::native) fn release_reaped(&self, token: SupervisorToken) -> AnyResult<()> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        let releasable = state
            .children
            .get(&token.0)
            .context("unknown supervisor token")?
            .leader_reaped;
        let proof_complete = state.children.get(&token.0).is_some_and(|child| {
            !child.termination_started
                || ((child.leader_kill_delivered || child.leader_exit_observed)
                    && child.group_kill_proved)
        });
        if !releasable || !proof_complete {
            bail!("cannot release a supervisor child without complete leader/group proof");
        }
        state.children.remove(&token.0);
        Ok(())
    }

    pub(in crate::native) fn finalize(&self, deadline: Instant) -> AnyResult<()> {
        loop {
            let tokens = self
                .state
                .lock()
                .expect("supervisor mutex poisoned")
                .children
                .keys()
                .copied()
                .map(SupervisorToken)
                .collect::<Vec<_>>();
            if tokens.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "supervisor final cleanup deadline exceeded in state {:?}",
                    self.lifecycle()
                );
            }
            for token in &tokens {
                match self.try_wait(*token) {
                    Ok(Some(_)) => {
                        self.release_reaped(*token)?;
                    }
                    Ok(None) | Err(_) => {
                        let _ = self.progress_kill(*token);
                    }
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "supervisor final cleanup deadline exceeded in state {:?}",
                    self.lifecycle()
                );
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    pub(in crate::native) fn lifecycle(&self) -> SupervisorLifecycle {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        if state.children.is_empty() {
            return SupervisorLifecycle::Reaped;
        }
        let kill_delivered = state
            .children
            .values()
            .filter(|child| child.leader_kill_delivered && child.group_kill_proved)
            .count();
        let unproved = state.children.len() - kill_delivered;
        if unproved == 0 {
            SupervisorLifecycle::KillDelivered {
                children: kill_delivered,
            }
        } else {
            SupervisorLifecycle::TerminationUnproven {
                kill_delivered,
                unproved,
            }
        }
    }

    fn best_effort_once(&self) {
        let tokens = self
            .state
            .lock()
            .expect("supervisor mutex poisoned")
            .children
            .keys()
            .copied()
            .map(SupervisorToken)
            .collect::<Vec<_>>();
        for token in tokens {
            match self.try_wait(token) {
                Ok(Some(_)) => {
                    let _ = self.release_reaped(token);
                }
                Ok(None) | Err(_) => {
                    let _ = self.progress_kill(token);
                }
            }
        }
    }

    #[cfg(test)]
    pub(in crate::native) fn inject_faults(
        &self,
        wait_failures: usize,
        pidfd_open_failures: usize,
        pidfd_signal_failures: usize,
        group_kill_failures: usize,
    ) {
        self.state.lock().expect("supervisor mutex poisoned").faults = SupervisorFaults {
            wait: wait_failures,
            pidfd_open: pidfd_open_failures,
            pidfd_signal: pidfd_signal_failures,
            group_kill: group_kill_failures,
        };
    }

    #[cfg(test)]
    pub(in crate::native) fn only_child_facts(&self) -> (bool, bool, bool, bool) {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.values().next().expect("one child");
        assert_eq!(state.children.len(), 1, "expected exactly one child");
        (
            child.leader_exit_observed,
            child.leader_kill_delivered,
            child.group_kill_proved,
            child.leader_reaped,
        )
    }

    #[cfg(test)]
    pub(in crate::native) fn only_child_termination_started(&self) -> bool {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.values().next().expect("one child");
        assert_eq!(state.children.len(), 1, "expected exactly one child");
        child.termination_started
    }

    #[cfg(test)]
    pub(in crate::native) fn only_child_pid(&self) -> Pid {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.values().next().expect("one child");
        assert_eq!(state.children.len(), 1, "expected exactly one child");
        child.pid.expect("registered child pid")
    }

    #[cfg(test)]
    pub(in crate::native) fn only_child_exit_ready(&self) -> bool {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.values().next().expect("one child");
        assert_eq!(state.children.len(), 1, "expected exactly one child");
        let pidfd = child.pidfd.as_ref().expect("registered child pidfd");
        waitid(
            WaitId::PidFd(pidfd.as_fd()),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .expect("observe child exit through pidfd")
        .is_some()
    }
}

impl Drop for InvocationSupervisor {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1 {
            self.best_effort_once();
        }
    }
}
