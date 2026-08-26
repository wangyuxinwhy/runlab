//! Linux OCI execution through a caller-selected `runc` executable.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::IoSliceMut;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult, anyhow, bail};
use chrono::{DateTime, FixedOffset, Local};
use oci_spec::image::{Descriptor, MediaType};
use run_protocol::{
    Availability, CreateFacts, EngineError, ExecutionInterval, ExecutionOutput, ImageDescriptor,
    InputPath, MAX_CAPTURED_STREAM_BYTES, Network, OperationError, OperationReport, OperationStage,
    OperationStatus, ProcessResult, ProgramId, ProgramInput, ProgramOutput, RunInput, RunOutput,
    StartFacts, StdinOutput, StdinWriteFacts, StopAction, StopActionResult, StopSignal,
    StreamFacts,
};
use rustix::net::netlink::{CONNECTOR as NETLINK_CONNECTOR, SocketAddrNetlink};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendFlags,
    SocketFlags, SocketType, bind, getsockname, recvfrom, recvmsg, sendto, socket_with,
};
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitId, WaitIdOptions, geteuid, kill_process_group, pidfd_open,
    pidfd_send_signal, waitid,
};
use rustix::{fs::OFlags, fs::fcntl_getfl, fs::fcntl_setfl};
use serde::Deserialize;
use tempfile::TempDir;

use crate::oci::{
    OciErrorKind, VerifiedImage, inspect_image, publish_expected, publish_final_image,
};
use crate::rootfs::{CapturedLayer, Rootfs, RootfsLimits, VerifiedLayer};
use crate::{
    CancellationToken, ContentError, ContentErrorKind, OciContent, OciContentStore,
    OperationTimeouts, RunEngine, STOP_GRACE_PERIOD,
};

const MAX_PROGRAMS: usize = 8;
#[allow(
    clippy::duration_suboptimal_units,
    reason = "the declared MSRV lacks Duration::from_hours"
)]
const MAX_EXECUTION_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HELPER_OUTPUT_LIMIT: usize = 1024 * 1024;
const PIPE_PUMP_BYTE_BUDGET: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MIN_HELPER_START_REMAINING: Duration = Duration::from_millis(20);
const SUPERVISOR_REAP_LIMIT: Duration = Duration::from_millis(250);
static INVOCATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct InvocationSupervisor {
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

#[derive(Clone, Copy, Debug)]
struct SupervisorToken(u64);

#[derive(Debug)]
enum SupervisorSpawnError {
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
enum SupervisorLifecycle {
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
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SupervisorState {
                next_id: 0,
                children: BTreeMap::new(),
                #[cfg(test)]
                faults: SupervisorFaults::default(),
            })),
        }
    }

    fn spawn(&self, command: &mut Command) -> Result<SupervisorToken, SupervisorSpawnError> {
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

    fn with_child<T>(
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

    fn try_wait(&self, token: SupervisorToken) -> std::io::Result<Option<ExitStatus>> {
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

    fn progress_kill(&self, token: SupervisorToken) -> AnyResult<bool> {
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
        let Some(pidfd) = entry.pidfd.as_ref() else {
            return progress_without_pidfd(entry, inject_group_failure);
        };
        if entry.leader_kill_delivered && entry.group_kill_proved {
            return Ok(true);
        }
        if !entry.stop_requested {
            pidfd_send_signal(pidfd, Signal::STOP)
                .context("failed to deliver STOP through stable pidfd")?;
            entry.stop_requested = true;
        }
        let identity_stable = if entry.leader_kill_delivered {
            entry.leader_exit_observed |= waitid(
                WaitId::PidFd(pidfd.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            )?
            .is_some();
            entry.leader_exit_observed
        } else {
            waitid(
                WaitId::PidFd(pidfd.as_fd()),
                WaitIdOptions::STOPPED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            )?
            .is_some_and(|status| status.stopped())
        };
        if !identity_stable {
            return Ok(false);
        }
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
        let leader_result = if entry.leader_kill_delivered {
            Ok(())
        } else {
            pidfd_send_signal(pidfd, Signal::KILL)
        };
        if let Err(group_error) = group_result {
            if leader_result.is_ok() {
                entry.leader_kill_delivered = true;
            }
            return leader_result
                .context("process-group KILL failed and stable leader KILL also failed")
                .and_then(|()| {
                    Err(anyhow!(
                        "stable leader KILL succeeded but process-group KILL remains unproved: {group_error}"
                    ))
                });
        }
        leader_result.context("failed to deliver KILL through stable pidfd")?;
        entry.group_kill_proved = true;
        entry.leader_kill_delivered = true;
        Ok(true)
    }

    fn release_reaped(&self, token: SupervisorToken) -> AnyResult<()> {
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

    fn finalize(&self, deadline: Instant) -> AnyResult<()> {
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

    fn lifecycle(&self) -> SupervisorLifecycle {
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
    fn inject_faults(
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
    fn only_child_facts(&self) -> (bool, bool, bool, bool) {
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
    fn only_child_termination_started(&self) -> bool {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.values().next().expect("one child");
        assert_eq!(state.children.len(), 1, "expected exactly one child");
        child.termination_started
    }

    #[cfg(test)]
    fn only_child_pid(&self) -> Pid {
        let state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.values().next().expect("one child");
        assert_eq!(state.children.len(), 1, "expected exactly one child");
        child.pid.expect("registered child pid")
    }
}

impl Drop for InvocationSupervisor {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1 {
            self.best_effort_once();
        }
    }
}

#[derive(Clone, Copy)]
struct OperationBudget {
    deadline: Instant,
    operation: &'static str,
}

impl OperationBudget {
    fn new(duration: Duration, operation: &'static str) -> AnyResult<Self> {
        Ok(Self {
            deadline: checked_deadline(Instant::now(), duration, operation)?,
            operation,
        })
    }

    fn check(self) -> AnyResult<()> {
        if Instant::now() >= self.deadline {
            bail!("{} deadline exceeded", self.operation);
        }
        Ok(())
    }

    fn remaining(self) -> AnyResult<Duration> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }

    fn check_io(self) -> std::io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} deadline exceeded", self.operation),
            ));
        }
        Ok(())
    }

    fn check_content(self) -> Result<(), ContentError> {
        if Instant::now() >= self.deadline {
            return Err(ContentError::new(
                ContentErrorKind::Internal,
                format!("{} deadline exceeded", self.operation),
            ));
        }
        Ok(())
    }
}

struct BudgetedStore {
    inner: Arc<dyn OciContentStore>,
    budget: OperationBudget,
}

impl BudgetedStore {
    fn new(inner: Arc<dyn OciContentStore>, budget: OperationBudget) -> Self {
        Self { inner, budget }
    }
}

impl OciContentStore for BudgetedStore {
    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        self.budget.check_content()?;
        let content = self.inner.open(descriptor)?;
        self.budget.check_content()?;
        Ok(Box::new(BudgetedContent {
            inner: content,
            budget: self.budget,
        }))
    }

    fn publish(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<(), ContentError> {
        self.budget.check_content()?;
        let mut content = BudgetedRead {
            inner: content,
            budget: self.budget,
        };
        self.inner.publish(descriptor, &mut content)?;
        self.budget.check_content()
    }
}

struct BudgetedContent {
    inner: Box<dyn OciContent>,
    budget: OperationBudget,
}

impl Read for BudgetedContent {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.budget.check_io()?;
        let count = self.inner.read(buffer)?;
        self.budget.check_io()?;
        Ok(count)
    }
}

impl Seek for BudgetedContent {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.budget.check_io()?;
        let offset = self.inner.seek(position)?;
        self.budget.check_io()?;
        Ok(offset)
    }
}

struct BudgetedRead<'a> {
    inner: &'a mut dyn Read,
    budget: OperationBudget,
}

impl Read for BudgetedRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.budget.check_io()?;
        let count = self.inner.read(buffer)?;
        self.budget.check_io()?;
        Ok(count)
    }
}

/// Linux reference implementation backed directly by an OCI runtime.
pub struct NativeEngine {
    store: Arc<dyn OciContentStore>,
    workspace_root: PathBuf,
    runc_executable: PathBuf,
    timeouts: OperationTimeouts,
}

impl NativeEngine {
    /// Constructs an Engine with fixed invocation-independent resource paths and deadlines.
    #[must_use]
    pub fn new(
        store: Arc<dyn OciContentStore>,
        workspace_root: impl Into<PathBuf>,
        runc_executable: impl Into<PathBuf>,
        timeouts: OperationTimeouts,
    ) -> Self {
        Self {
            store,
            workspace_root: workspace_root.into(),
            runc_executable: runc_executable.into(),
            timeouts,
        }
    }

    /// Returns the fixed finite deadlines used for Engine-owned operations.
    #[must_use]
    pub fn operation_timeouts(&self) -> OperationTimeouts {
        self.timeouts
    }

    fn run_supervised(
        &self,
        input: &RunInput,
        cancellation: &CancellationToken,
        supervisor: &InvocationSupervisor,
    ) -> Result<RunOutput, EngineError> {
        let budget = OperationBudget::new(self.timeouts.preparation(), "NativeEngine preparation")
            .map_err(|error| EngineError::internal(format!("{error:#}")))?;
        let store = BudgetedStore::new(Arc::clone(&self.store), budget);
        let preflight = self.preflight(input, &store, budget, supervisor);
        let mut prepared = match preflight {
            Ok(prepared) => prepared,
            Err(input_error) => match supervisor.finalize(budget.deadline) {
                Ok(()) => return Err(input_error),
                Err(supervision_error) => {
                    return Err(EngineError::internal(format!(
                        "preflight failed ({input_error}) and supervisor cleanup did not reach Reaped before the preparation deadline: {supervision_error:#}"
                    )));
                }
            },
        };
        budget
            .check()
            .map_err(|error| EngineError::internal(format!("{error:#}")))?;
        execute(self, input, cancellation, &mut prepared)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the single prestart boundary keeps every capability check before any OCI Program start"
    )]
    fn preflight(
        &self,
        input: &RunInput,
        store: &dyn OciContentStore,
        budget: OperationBudget,
        supervisor: &InvocationSupervisor,
    ) -> Result<PreparedInvocation, EngineError> {
        budget
            .check()
            .map_err(|error| EngineError::internal(format!("{error:#}")))?;
        if input.programs().len() > MAX_PROGRAMS {
            return Err(EngineError::unsupported(
                InputPath::field("programs"),
                format!(
                    "NativeEngine supports at most {MAX_PROGRAMS} Programs because each Program retains two independent 100 MiB stream prefixes"
                ),
            ));
        }
        if let Some(timeout) = input.execution_timeout_ms() {
            let duration = Duration::from_millis(timeout.get());
            if duration > MAX_EXECUTION_TIMEOUT {
                return Err(EngineError::unsupported(
                    InputPath::field("execution_timeout_ms"),
                    format!(
                        "NativeEngine supports execution timeouts of at most {} ms",
                        MAX_EXECUTION_TIMEOUT.as_millis()
                    ),
                ));
            }
        }
        if input.network() == Network::Egress {
            return Err(EngineError::unsupported(
                InputPath::field("network"),
                "NativeEngine has not implemented the outbound-only egress boundary",
            ));
        }
        if geteuid().as_raw() != 0 {
            return Err(EngineError::unsupported(
                InputPath::field("programs"),
                "the current NativeEngine profile requires root; rootless OCI semantics have not been proved for this input",
            ));
        }
        let workspace_root = validate_private_directory(&self.workspace_root, "workspace_root")?;
        let runc = validate_runc(&self.runc_executable, budget, supervisor)?;

        let mut images = BTreeMap::new();
        for (program_id, program) in input.programs() {
            validate_runtime(program_id, program)?;
            validate_host_resources(program_id, program)?;
            let image = inspect_image(store, program.initial_environment());
            budget
                .check()
                .map_err(|error| EngineError::internal(format!("{error:#}")))?;
            let image = image.map_err(|error| map_oci_error(program_id, &error))?;
            validate_platform(program_id, &image)?;
            images.insert(program_id.clone(), image);
            budget
                .check()
                .map_err(|error| EngineError::internal(format!("{error:#}")))?;
        }

        let invocation = tempfile::Builder::new()
            .prefix("run-engine-native-")
            .tempdir_in(workspace_root)
            .map_err(|error| {
                EngineError::internal(format!(
                    "failed to create private NativeEngine workspace: {error}"
                ))
            })?;
        fs::set_permissions(invocation.path(), fs::Permissions::from_mode(0o700)).map_err(
            |error| {
                EngineError::internal(format!("failed to protect NativeEngine workspace: {error}"))
            },
        )?;
        let runtime_root = invocation.path().join("runtime");
        create_private_directory(&runtime_root).map_err(|error| {
            EngineError::internal(format!("failed to create private runc root: {error:#}"))
        })?;
        let cgroup_base = current_cgroup_base().map_err(|error| {
            EngineError::internal(format!(
                "failed to establish the Engine-owned default cgroup base: {error:#}"
            ))
        })?;

        let mut programs = BTreeMap::new();
        for (index, (program_id, program)) in input.programs().iter().enumerate() {
            let bundle = invocation.path().join(format!("bundle-{index}"));
            create_private_directory(&bundle).map_err(|error| {
                EngineError::internal(format!("failed to create OCI bundle: {error:#}"))
            })?;
            let image = &images[program_id];
            let layers = image
                .layers()
                .iter()
                .map(|layer| VerifiedLayer {
                    descriptor: layer.descriptor(),
                    expected_diff_id: layer.diff_id(),
                })
                .collect::<Vec<_>>();
            let rootfs =
                Rootfs::materialize_in(&bundle, &layers, RootfsLimits::default(), |descriptor| {
                    store.open(descriptor).map_err(|error| anyhow!(error))
                });
            budget
                .check()
                .map_err(|error| EngineError::internal(format!("{error:#}")))?;
            let rootfs = rootfs.map_err(|error| {
                EngineError::internal(format!(
                    "failed to materialize Program {program_id:?}: {error:#}"
                ))
            })?;
            write_exact_config(&bundle, program.runtime_config().as_bytes()).map_err(|error| {
                EngineError::internal(format!(
                    "failed to write Program {program_id:?} config.json: {error:#}"
                ))
            })?;
            let artifacts = mount_artifacts(rootfs.path(), program.runtime_config().as_json())
                .map_err(|error| {
                    EngineError::internal(format!(
                        "failed to inventory Program {program_id:?} mount destinations: {error:#}"
                    ))
                })?;
            let suffix = INVOCATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let runtime_id = format!("run-engine-{}-{index}-{suffix}", std::process::id());
            let pidfd_path = runtime_root.join(format!("{runtime_id}.pidfd.sock"));
            let runc_log_path = runtime_root.join(format!("{runtime_id}.create.log"));
            let expected_cgroup_path = cgroup_base.join(&runtime_id);
            match fs::symlink_metadata(&expected_cgroup_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(EngineError::internal(format!(
                        "private runtime cgroup already exists: {}",
                        expected_cgroup_path.display()
                    )));
                }
                Err(error) => {
                    return Err(EngineError::internal(format!(
                        "failed to check private runtime cgroup {}: {error}",
                        expected_cgroup_path.display()
                    )));
                }
            }
            preflight_pidfd_socket(&pidfd_path).map_err(|error| {
                EngineError::internal(format!(
                    "failed to prepare Program {program_id:?} pidfd socket: {error:#}"
                ))
            })?;
            programs.insert(
                program_id.clone(),
                PreparedProgram {
                    bundle,
                    runtime_id,
                    pidfd_path,
                    runc_log_path,
                    expected_cgroup_path,
                    rootfs,
                    parent: program.initial_environment().clone(),
                    artifacts,
                },
            );
            budget
                .check()
                .map_err(|error| EngineError::internal(format!("{error:#}")))?;
        }
        Ok(PreparedInvocation {
            workspace: Some(invocation),
            runtime_root,
            runc,
            programs,
            supervisor: supervisor.clone(),
        })
    }
}

impl RunEngine for NativeEngine {
    fn run(
        &self,
        input: RunInput,
        cancellation: CancellationToken,
    ) -> Result<RunOutput, EngineError> {
        let supervisor = InvocationSupervisor::new();
        self.run_supervised(&input, &cancellation, &supervisor)
    }
}

struct PreparedInvocation {
    workspace: Option<TempDir>,
    runtime_root: PathBuf,
    runc: PathBuf,
    programs: BTreeMap<ProgramId, PreparedProgram>,
    supervisor: InvocationSupervisor,
}

struct PreparedProgram {
    bundle: PathBuf,
    runtime_id: String,
    pidfd_path: PathBuf,
    runc_log_path: PathBuf,
    expected_cgroup_path: PathBuf,
    rootfs: Rootfs,
    parent: ImageDescriptor,
    artifacts: Vec<PathBuf>,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "these booleans are independent direct-evidence and resource-ownership facts, not one lifecycle state"
)]
struct ProgramRun {
    supervisor: InvocationSupervisor,
    child: Option<SupervisorToken>,
    state_probe: Option<RunningHelper>,
    state_probe_deadline: Option<Instant>,
    state_probe_failed: bool,
    runtime_coordinates: Option<(PathBuf, PathBuf, String, Duration)>,
    exit_monitor: Option<ProcExitMonitor>,
    exit_monitor_diagnostic: Option<String>,
    stopped_observation: Option<(DateTime<FixedOffset>, Instant)>,
    pidfd: Option<OwnedFd>,
    cgroup_path: Option<PathBuf>,
    runtime_attempted: bool,
    execution_entry: Option<(DateTime<FixedOffset>, Instant)>,
    poll_failed: bool,
    create: OperationReport<CreateFacts>,
    start: OperationReport<StartFacts>,
    process: Option<ProcessResult>,
    stdin_transfer: Option<InputTransfer>,
    stdout_drain: Option<StreamDrain>,
    stderr_drain: Option<StreamDrain>,
    stdin: Option<StdinOutput>,
    stdout: Option<OperationReport<StreamFacts>>,
    stderr: Option<OperationReport<StreamFacts>>,
    stop_actions: Vec<StopAction>,
    errors: Vec<OperationError>,
    writer_stopped: bool,
    supervisor_unreaped: bool,
}

#[allow(
    clippy::too_many_lines,
    reason = "the lifecycle sequence is kept linear so evidence cannot be reordered across phases"
)]
fn execute(
    engine: &NativeEngine,
    input: &RunInput,
    cancellation: &CancellationToken,
    prepared: &mut PreparedInvocation,
) -> Result<RunOutput, EngineError> {
    let mut runs = input
        .programs()
        .keys()
        .cloned()
        .map(|id| {
            (
                id,
                ProgramRun::unattempted_with(prepared.supervisor.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut interval_start = None;
    let mut monotonic_start = None;
    let execution_limit = input
        .execution_timeout_ms()
        .map(|value| Duration::from_millis(value.get()));
    let mut termination_reason = None;

    let order = input
        .programs()
        .keys()
        .filter(|id| !id.is_primary())
        .cloned()
        .chain(std::iter::once(ProgramId::primary()))
        .collect::<Vec<_>>();
    for program_id in order {
        if cancellation.is_cancelled() {
            termination_reason = Some(TerminationReason::Cancelled);
            break;
        }
        if execution_expired(monotonic_start, execution_limit) {
            termination_reason = Some(TerminationReason::TimedOut);
            break;
        }
        let program = &prepared.programs[&program_id];
        let run = start_program(
            &prepared.supervisor,
            &prepared.runc,
            &prepared.runtime_root,
            program,
            &input.programs()[&program_id],
            engine.timeouts,
            cancellation,
            monotonic_start,
            execution_limit,
            &mut runs,
        );
        let started = run.start.status() == OperationStatus::Succeeded;
        if monotonic_start.is_none()
            && let Some((wall_clock, monotonic)) = &run.execution_entry
        {
            interval_start = Some(*wall_clock);
            monotonic_start = Some(*monotonic);
        }
        runs.insert(program_id.clone(), run);
        if cancellation.is_cancelled() {
            termination_reason = Some(TerminationReason::Cancelled);
            break;
        }
        if execution_expired(monotonic_start, execution_limit) {
            termination_reason = Some(TerminationReason::TimedOut);
            break;
        }
        if !started {
            termination_reason = Some(TerminationReason::Lifecycle);
            break;
        }
    }

    if termination_reason.is_none() {
        loop {
            if poll_children(&mut runs) {
                termination_reason = Some(TerminationReason::Lifecycle);
                break;
            }
            if runs[&ProgramId::primary()].process.is_some() {
                termination_reason = Some(TerminationReason::PrimaryEnded);
                break;
            }
            if cancellation.is_cancelled() {
                termination_reason = Some(TerminationReason::Cancelled);
                break;
            }
            if execution_expired(monotonic_start, execution_limit) {
                termination_reason = Some(TerminationReason::TimedOut);
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
    let termination_reason = termination_reason.unwrap_or(TerminationReason::Lifecycle);
    for run in runs.values_mut() {
        run.freeze_stdin();
    }
    let interval_end = now();
    stop_all(
        &prepared.runc,
        &prepared.runtime_root,
        &prepared.programs,
        &mut runs,
        engine.timeouts,
    );
    finalize_children(&mut runs, engine.timeouts);

    let supervisor_deadline = checked_deadline(
        Instant::now(),
        engine.timeouts.cleanup(),
        "invocation supervisor cleanup deadline",
    )
    .map_err(|error| EngineError::internal(format!("{error:#}")))?;
    if let Err(error) = establish_runtime_cleanup_safety(&prepared.supervisor, supervisor_deadline)
    {
        for run in runs.values_mut() {
            run.writer_stopped = false;
        }
        if let Some(workspace) = prepared.workspace.take() {
            return Err(preserve_workspace_after_supervisor_failure(
                workspace, &error,
            ));
        }
        return Err(EngineError::internal(format!("{error:#}")));
    }

    for (program_id, program) in &prepared.programs {
        let run = runs.get_mut(program_id).expect("output slot exists");
        cleanup_runtime(
            &prepared.runc,
            &prepared.runtime_root,
            program,
            run,
            engine.timeouts,
            supervisor_deadline,
        );
    }

    if let Err(error) = prepared.supervisor.finalize(supervisor_deadline) {
        for run in runs.values_mut() {
            run.writer_stopped = false;
        }
        if let Some(workspace) = prepared.workspace.take() {
            return Err(preserve_workspace_after_supervisor_failure(
                workspace, &error,
            ));
        }
        return Err(EngineError::internal(format!(
            "invocation supervisor could not prove every child reaped before capture: {error:#}"
        )));
    }
    for run in runs.values_mut() {
        run.supervisor_unreaped = false;
    }

    for run in runs.values_mut().filter(|run| run.supervisor_unreaped) {
        run.writer_stopped = false;
    }

    let mut outputs = BTreeMap::new();
    for (program_id, program) in &mut prepared.programs {
        let run = runs.get_mut(program_id).expect("output slot exists");
        let final_environment = capture_final(engine, program, run);
        outputs.insert(program_id.clone(), run.output(final_environment)?);
    }

    let all_writers_stopped = runs.values().all(|run| run.writer_stopped);
    let cleanup_budget = OperationBudget::new(engine.timeouts.cleanup(), "invocation cleanup");
    let cleanup_error = match cleanup_budget {
        Err(error) => {
            if let Some(workspace) = prepared.workspace.take() {
                let _preserved = workspace.keep();
            }
            Some(operation_error(
                OperationStage::Cleanup,
                format!("failed to establish cleanup deadline: {error:#}"),
                None,
            ))
        }
        Ok(budget) => cleanup_invocation(prepared, all_writers_stopped, budget),
    };
    let execution_errors = cleanup_error.into_iter().collect::<Vec<_>>();
    let interval = interval_start
        .map_or_else(
            || ExecutionInterval::not_entered("no Program reached an OCI start attempt"),
            |started_at| Ok(ExecutionInterval::entered(started_at, interval_end)),
        )
        .map_err(output_internal)?;
    let execution = ExecutionOutput::new(
        interval,
        termination_reason == TerminationReason::TimedOut,
        termination_reason == TerminationReason::Cancelled,
        execution_errors,
    )
    .map_err(output_internal)?;
    if runs.values().any(|run| run.supervisor_unreaped) {
        return Err(EngineError::internal(
            "NativeEngine could not prove that every runtime helper was terminated and reaped; full cleanup was attempted and no trustworthy RunOutput is available",
        ));
    }
    RunOutput::new(input, execution, outputs).map_err(output_internal)
}

fn establish_runtime_cleanup_safety(
    supervisor: &InvocationSupervisor,
    deadline: Instant,
) -> AnyResult<()> {
    let initial_deadline = checked_deadline(
        Instant::now(),
        SUPERVISOR_REAP_LIMIT,
        "initial supervisor cleanup pass",
    )?
    .min(deadline);
    let _ = supervisor.finalize(initial_deadline);
    if matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::TerminationUnproven { .. }
    ) {
        let closure = supervisor.finalize(deadline);
        if matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::TerminationUnproven { .. }
        ) {
            return Err(closure.expect_err("unproved lifecycle must not report Reaped"));
        }
    }
    if !matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::Reaped | SupervisorLifecycle::KillDelivered { .. }
    ) {
        bail!("invocation supervisor did not establish a safe runtime-cleanup lifecycle");
    }
    if Instant::now() >= deadline {
        bail!(
            "invocation cleanup deadline expired after supervisor safety was established and before runtime cleanup"
        );
    }
    Ok(())
}

fn preserve_workspace_after_supervisor_failure(
    workspace: TempDir,
    error: &anyhow::Error,
) -> EngineError {
    let preserved = workspace.keep();
    EngineError::internal(format!(
        "invocation supervisor could not prove every child reaped before capture; preserved workspace {}: {error:#}",
        preserved.display()
    ))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminationReason {
    PrimaryEnded,
    TimedOut,
    Cancelled,
    Lifecycle,
}

impl ProgramRun {
    #[cfg(test)]
    fn unattempted() -> Self {
        Self::unattempted_with(InvocationSupervisor::new())
    }

    fn unattempted_with(supervisor: InvocationSupervisor) -> Self {
        Self {
            supervisor,
            child: None,
            state_probe: None,
            state_probe_deadline: None,
            state_probe_failed: false,
            runtime_coordinates: None,
            exit_monitor: None,
            exit_monitor_diagnostic: None,
            stopped_observation: None,
            pidfd: None,
            cgroup_path: None,
            runtime_attempted: false,
            execution_entry: None,
            poll_failed: false,
            create: OperationReport::not_attempted("runc create was not attempted")
                .expect("literal reason"),
            start: OperationReport::not_attempted("runc start was not attempted")
                .expect("literal reason"),
            process: None,
            stdin_transfer: None,
            stdout_drain: None,
            stderr_drain: None,
            stdin: None,
            stdout: None,
            stderr: None,
            stop_actions: Vec::new(),
            errors: Vec::new(),
            writer_stopped: true,
            supervisor_unreaped: false,
        }
    }

    fn output(
        &mut self,
        final_environment: Availability<ImageDescriptor>,
    ) -> Result<ProgramOutput, EngineError> {
        let process = self.process.take().unwrap_or_else(|| {
            ProcessResult::never_started("the Program did not reach a proved start")
                .expect("literal reason")
        });
        let stdin = self.stdin.take().unwrap_or_else(unattempted_stdin);
        let stdout = self.stdout.take().unwrap_or_else(unattempted_stream);
        let stderr = self.stderr.take().unwrap_or_else(unattempted_stream);
        ProgramOutput::new(
            self.create.clone(),
            self.start.clone(),
            process,
            stdin,
            stdout,
            stderr,
            std::mem::take(&mut self.stop_actions),
            final_environment,
            std::mem::take(&mut self.errors),
        )
        .map_err(output_internal)
    }

    fn pump_io(&mut self) {
        if let Some(transfer) = &mut self.stdin_transfer {
            transfer.pump();
        }
        self.pump_output();
    }

    fn pump_output(&mut self) {
        if let Some(drain) = &mut self.stdout_drain {
            drain.pump();
        }
        if let Some(drain) = &mut self.stderr_drain {
            drain.pump();
        }
    }

    fn freeze_stdin(&mut self) {
        if let Some(transfer) = &mut self.stdin_transfer {
            transfer.freeze();
        }
    }

    fn observe_runtime_stopped(&mut self, wait_timeout: Duration) {
        self.writer_stopped = true;
        let ended_at = now();
        if self.exit_monitor.is_some() {
            let deadline =
                checked_deadline(Instant::now(), wait_timeout, "raw exit evidence deadline")
                    .expect("validated OperationTimeouts fit Instant");
            self.stopped_observation = Some((ended_at, deadline));
        } else {
            self.process = Some(
                ProcessResult::unknown(
                    "runc state directly proved process termination but does not expose an unflattened process result",
                    Availability::available(ended_at),
                )
                .expect("literal reason"),
            );
        }
    }

    fn fallback_after_stopped(&mut self, reason: impl Into<String>) {
        let ended_at = self
            .stopped_observation
            .take()
            .map_or_else(now, |(ended_at, _)| ended_at);
        self.process = Some(
            ProcessResult::unknown(reason, Availability::available(ended_at))
                .expect("non-empty reason"),
        );
    }

    fn observe_raw_process_result(&mut self, result: ProcessResult) {
        self.writer_stopped = true;
        self.stopped_observation = None;
        self.process = Some(result);
    }

    fn io_complete(&self) -> bool {
        self.stdin_transfer
            .as_ref()
            .is_none_or(|transfer| transfer.pipe.is_none())
            && self
                .stdout_drain
                .as_ref()
                .is_none_or(|drain| drain.pipe.is_none())
            && self
                .stderr_drain
                .as_ref()
                .is_none_or(|drain| drain.pipe.is_none())
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the explicit create/start lifecycle keeps pidfd, stdio, cancellation, and deadline evidence in one ordered supervisor"
)]
fn start_program(
    supervisor: &InvocationSupervisor,
    runc: &Path,
    runtime_root: &Path,
    prepared: &PreparedProgram,
    input: &ProgramInput,
    timeouts: OperationTimeouts,
    cancellation: &CancellationToken,
    execution_start: Option<Instant>,
    execution_limit: Option<Duration>,
    other_runs: &mut BTreeMap<ProgramId, ProgramRun>,
) -> ProgramRun {
    if cancellation.is_cancelled() || execution_expired(execution_start, execution_limit) {
        return ProgramRun::unattempted_with(supervisor.clone());
    }
    let mut pidfd_receiver = match PidfdReceiver::bind(&prepared.pidfd_path) {
        Ok(receiver) => receiver,
        Err(error) => {
            let mut run = ProgramRun::unattempted_with(supervisor.clone());
            run.create = OperationReport::failed(
                operation_error(
                    OperationStage::Create,
                    format!("failed to create runc pidfd socket: {error}"),
                    error.raw_os_error().map(i64::from),
                ),
                [],
            );
            return run;
        }
    };
    let _ = fs::remove_file(&prepared.runc_log_path);
    let mut command = runc_command(runc, runtime_root);
    command
        .arg("--log")
        .arg(&prepared.runc_log_path)
        .arg("--log-format")
        .arg("json")
        .arg("create")
        .arg("--bundle")
        .arg(&prepared.bundle)
        .arg("--pidfd-socket")
        .arg(&prepared.pidfd_path)
        .arg(&prepared.runtime_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let child = match supervisor.spawn(&mut command) {
        Ok(child) => child,
        Err(SupervisorSpawnError::NotSpawned(error)) => {
            let operation = operation_error(
                OperationStage::Create,
                format!("failed to spawn runc create: {error}"),
                None,
            );
            let mut run = ProgramRun::unattempted_with(supervisor.clone());
            run.create = OperationReport::failed(operation, []);
            return run;
        }
        Err(SupervisorSpawnError::SpawnedRegistered { token, error }) => {
            let operation = operation_error(
                OperationStage::Create,
                format!(
                    "runc create was spawned, but its supervisor attachment failed after registration: {error:#}"
                ),
                None,
            );
            let mut run = ProgramRun::unattempted_with(supervisor.clone());
            run.child = Some(token);
            run.runtime_attempted = true;
            run.writer_stopped = false;
            run.create = OperationReport::unknown(
                "runc create outcome is unknown because supervisor attachment failed after the process was registered",
                [operation],
            )
            .expect("literal reason");
            return run;
        }
    };
    let (stdin, stdout, stderr) = supervisor
        .with_child(child, |child| {
            Ok((
                child.stdin.take().expect("piped stdin"),
                child.stdout.take().expect("piped stdout"),
                child.stderr.take().expect("piped stderr"),
            ))
        })
        .expect("newly registered create child");
    let mut create_stdout = StreamDrain::from_stdout(stdout);
    let mut create_stderr = StreamDrain::from_stderr(stderr);
    let create_deadline =
        checked_deadline(Instant::now(), timeouts.create(), "runc create deadline")
            .expect("validated OperationTimeouts fit Instant");
    let mut run = ProgramRun::unattempted_with(supervisor.clone());
    run.child = Some(child);
    run.runtime_attempted = true;
    let mut create_status = None;
    loop {
        for other in other_runs.values_mut() {
            other.pump_io();
        }
        create_stdout.pump();
        create_stderr.pump();
        if create_stdout.bytes.len() > HELPER_OUTPUT_LIMIT
            || create_stderr.bytes.len() > HELPER_OUTPUT_LIMIT
            || fs::metadata(&prepared.runc_log_path)
                .is_ok_and(|metadata| metadata.len() > HELPER_OUTPUT_LIMIT as u64)
        {
            run.errors.push(operation_error(
                OperationStage::Create,
                "runc create diagnostics exceeded the bounded 1 MiB limit",
                None,
            ));
            break;
        }
        if run.pidfd.is_none() {
            match pidfd_receiver.try_receive() {
                Ok(pidfd) => {
                    run.pidfd = pidfd;
                }
                Err(error) => {
                    run.create = OperationReport::unknown(
                        "runc pidfd receipt failed after runc create was spawned",
                        [operation_error(
                            OperationStage::Create,
                            format!("failed to receive container pidfd from runc: {error}"),
                            error.raw_os_error().map(i64::from),
                        )],
                    )
                    .expect("literal reason");
                    break;
                }
            }
        }
        let execution_deadline = execution_start.zip(execution_limit).map(|(start, limit)| {
            checked_deadline(start, limit, "execution timeout")
                .expect("validated execution timeout fits Instant")
        });
        let effective_deadline =
            execution_deadline.map_or(create_deadline, |limit| limit.min(create_deadline));
        if cancellation.is_cancelled() || Instant::now() >= effective_deadline {
            break;
        }
        match supervisor.try_wait(run.child.expect("create supervisor")) {
            Ok(Some(status)) => {
                create_status = Some(status);
                break;
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                run.poll_failed = true;
                run.errors.push(operation_error(
                    OperationStage::Create,
                    format!("failed to poll runc create: {error}"),
                    error.raw_os_error().map(i64::from),
                ));
                break;
            }
        }
    }
    create_stdout.pump();
    create_stderr.pump();
    if create_status.is_none() {
        terminate_child(supervisor, &mut run.child, SUPERVISOR_REAP_LIMIT).unwrap_or_else(
            |error| {
                run.supervisor_unreaped = true;
                run.errors.push(operation_error(
                    OperationStage::Create,
                    format!("failed to terminate runc create supervisor: {error:#}"),
                    None,
                ));
            },
        );
    } else {
        supervisor
            .release_reaped(run.child.expect("completed create supervisor"))
            .expect("completed create supervisor is reaped");
        run.child = None;
    }
    run.create = match create_status {
        Some(status) if status.success() && run.pidfd.is_some() => {
            OperationReport::succeeded(CreateFacts::new(now()))
        }
        Some(status) if status.success() => OperationReport::unknown(
            "runc create succeeded but did not deliver the required container pidfd",
            [],
        )
        .expect("literal reason"),
        Some(status) => OperationReport::failed(
            operation_error(
                OperationStage::Create,
                create_failure_message(
                    status,
                    &create_stdout.bytes,
                    &create_stderr.bytes,
                    &prepared.runc_log_path,
                ),
                status.code().map(i64::from),
            ),
            [],
        ),
        None => OperationReport::unknown(
            "runc create did not complete before cancellation or its deadline",
            [operation_error(
                OperationStage::Create,
                if cancellation.is_cancelled() {
                    "runc create interrupted by cancellation"
                } else if execution_expired(execution_start, execution_limit) {
                    "execution timeout reached while runc create was in progress"
                } else {
                    "runc create deadline exceeded"
                },
                None,
            )],
        )
        .expect("literal reason"),
    };
    if run.create.status() != OperationStatus::Succeeded
        || cancellation.is_cancelled()
        || execution_expired(execution_start, execution_limit)
    {
        return run;
    }
    if !create_stdout.bytes.is_empty()
        || !create_stderr.bytes.is_empty()
        || create_stdout.error.is_some()
        || create_stderr.error.is_some()
        || create_stdout.pipe.is_none()
        || create_stderr.pipe.is_none()
    {
        run.errors.push(operation_error(
            OperationStage::Create,
            format!(
                "runc create succeeded but pre-start Program pipes were not provably empty; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&create_stdout.bytes),
                String::from_utf8_lossy(&create_stderr.bytes)
            ),
            None,
        ));
        return run;
    }
    run.stdin_transfer = Some(InputTransfer::new(stdin, input.stdin().to_vec()));
    run.stdout_drain = Some(create_stdout);
    run.stderr_drain = Some(create_stderr);
    run.writer_stopped = false;

    let init_pid = match run
        .pidfd
        .as_ref()
        .context("successful create has no pidfd")
        .and_then(pidfd_process_id)
    {
        Ok(pid) => pid,
        Err(error) => {
            run.errors.push(operation_error(
                OperationStage::Create,
                format!("could not identify the created container process: {error:#}"),
                None,
            ));
            return run;
        }
    };
    let exit_monitor = ProcExitMonitor::subscribe(init_pid);
    match exit_monitor {
        Ok(monitor) => run.exit_monitor = Some(monitor),
        Err(error) => {
            run.exit_monitor_diagnostic = Some(format!(
                "raw process-result monitoring is unavailable; termination will remain Unknown: {error:#}"
            ));
        }
    }
    match observe_owned_cgroup(init_pid, &prepared.expected_cgroup_path) {
        Ok(path) => run.cgroup_path = Some(path),
        Err(error) => {
            run.errors.push(operation_error(
                OperationStage::Create,
                format!("could not prove ownership of runc's default cgroup: {error:#}"),
                None,
            ));
            return run;
        }
    }

    let start_wall = now();
    let start_monotonic = Instant::now();
    run.execution_entry = Some((start_wall, start_monotonic));
    let execution_deadline = execution_start
        .unwrap_or(start_monotonic)
        .checked_add(execution_limit.unwrap_or(MAX_EXECUTION_TIMEOUT));
    let own_start_deadline =
        checked_deadline(start_monotonic, timeouts.start(), "runc start deadline")
            .expect("validated OperationTimeouts fit Instant");
    let (start_deadline, deadline_message) = execution_deadline.map_or(
        (own_start_deadline, "runc start deadline exceeded"),
        |deadline| {
            if deadline <= own_start_deadline {
                (
                    deadline,
                    "execution timeout reached while runc start was in progress",
                )
            } else {
                (own_start_deadline, "runc start deadline exceeded")
            }
        },
    );
    let mut command = runc_command(runc, runtime_root);
    command.arg("start").arg(&prepared.runtime_id);
    let start_result = supervise_start(
        &mut command,
        start_deadline,
        deadline_message,
        cancellation,
        &mut run,
        other_runs,
    );
    run.start = match start_result {
        Ok(output) if output.status.success() => OperationReport::succeeded(StartFacts::new(now())),
        Ok(output) => OperationReport::failed(
            operation_error(
                OperationStage::Start,
                helper_message("runc start", &output),
                output.status.code().map(i64::from),
            ),
            [],
        ),
        Err(error) => OperationReport::unknown(
            "runc start outcome is unknown because its supervisor did not complete",
            [operation_error(
                OperationStage::Start,
                format!("runc start: {error:#}"),
                None,
            )],
        )
        .expect("literal reason"),
    };
    if run.start.status() != OperationStatus::Failed
        && let Some(message) = run.exit_monitor_diagnostic.take()
    {
        run.errors
            .push(operation_error(OperationStage::Wait, message, None));
    }
    if run.start.status() == OperationStatus::Failed {
        run.exit_monitor = None;
        return run;
    }
    run.runtime_coordinates = Some((
        runc.to_path_buf(),
        runtime_root.to_path_buf(),
        prepared.runtime_id.clone(),
        timeouts.wait(),
    ));
    run
}

fn supervise_start(
    command: &mut Command,
    deadline: Instant,
    deadline_message: &'static str,
    cancellation: &CancellationToken,
    run: &mut ProgramRun,
    other_runs: &mut BTreeMap<ProgramId, ProgramRun>,
) -> AnyResult<HelperOutput> {
    let mut helper = RunningHelper::spawn(&run.supervisor, command)?;
    loop {
        run.pump_io();
        for other in other_runs.values_mut() {
            other.pump_io();
        }
        match helper.try_finish() {
            Ok(Some(output)) => return Ok(output),
            Ok(None) => {}
            Err(error) => {
                if let Err(cleanup) = helper.terminate() {
                    run.supervisor_unreaped = true;
                    return Err(error).context(format!(
                        "failed to terminate runc start after supervision error: {cleanup:#}"
                    ));
                }
                return Err(error);
            }
        }
        if cancellation.is_cancelled() {
            if let Err(error) = helper.terminate() {
                run.supervisor_unreaped = true;
                return Err(error).context("failed to terminate cancelled runc start");
            }
            bail!("runc start interrupted by cancellation");
        }
        if Instant::now() >= deadline {
            if let Err(error) = helper.terminate() {
                run.supervisor_unreaped = true;
                return Err(error).context("failed to terminate timed-out runc start");
            }
            bail!(deadline_message);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Deserialize)]
struct RuncState {
    status: String,
}

struct PidfdReceiver {
    listener: UnixListener,
    connection: Option<UnixStream>,
}

impl PidfdReceiver {
    fn bind(path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            connection: None,
        })
    }

    fn try_receive(&mut self) -> std::io::Result<Option<OwnedFd>> {
        if self.connection.is_none() {
            match self.listener.accept() {
                Ok((connection, _)) => {
                    connection.set_nonblocking(true)?;
                    self.connection = Some(connection);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let connection = self.connection.as_ref().expect("accepted connection");
        let mut payload = [0_u8; 32];
        let mut iov = [IoSliceMut::new(&mut payload)];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let message = match recvmsg(
            connection,
            &mut iov,
            &mut ancillary,
            RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
        ) {
            Ok(message) => message,
            Err(error) if error == rustix::io::Errno::AGAIN => return Ok(None),
            Err(error) => return Err(std::io::Error::from_raw_os_error(error.raw_os_error())),
        };
        if message
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated runc pidfd message",
            ));
        }
        if &payload[..message.bytes] != b"standard" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected runc pidfd message payload",
            ));
        }
        let mut descriptors = Vec::new();
        for item in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = item {
                descriptors.extend(rights);
            }
        }
        if descriptors.len() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "runc pidfd message contained {} descriptors instead of one",
                    descriptors.len()
                ),
            ));
        }
        Ok(descriptors.pop())
    }
}

const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_CN_MCAST_IGNORE: u32 = 2;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;
const NETLINK_HEADER_LEN: usize = 16;
const CONNECTOR_HEADER_LEN: usize = 20;
const PROC_EVENT_EXIT_LEN: usize = 40;
const PROC_EVENT_BUFFER: usize = 64 * 1024;

struct ProcExitMonitor {
    socket: OwnedFd,
    port_id: u32,
    target_pid: u32,
    sequences: BTreeMap<u32, u32>,
    subscribed: bool,
}

impl ProcExitMonitor {
    fn subscribe(target_pid: u32) -> AnyResult<Self> {
        let socket = socket_with(
            AddressFamily::NETLINK,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            Some(NETLINK_CONNECTOR),
        )?;
        bind(&socket, &SocketAddrNetlink::new(0, CN_IDX_PROC))?;
        let address = SocketAddrNetlink::try_from(getsockname(&socket)?)?;
        let mut monitor = Self {
            socket,
            port_id: address.pid(),
            target_pid,
            sequences: BTreeMap::new(),
            subscribed: false,
        };
        monitor.send_control(PROC_CN_MCAST_LISTEN)?;
        monitor.subscribed = true;
        monitor.drain_stale()?;
        Ok(monitor)
    }

    fn try_result(&mut self) -> AnyResult<Option<ProcessResult>> {
        loop {
            let mut buffer = vec![0_u8; PROC_EVENT_BUFFER].into_boxed_slice();
            let (received, full_length, address) =
                match recvfrom(&self.socket, &mut buffer[..], RecvFlags::DONTWAIT) {
                    Ok(value) => value,
                    Err(error) if error == rustix::io::Errno::AGAIN => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
            if full_length > received {
                bail!("proc connector datagram was truncated");
            }
            if address
                .map(SocketAddrNetlink::try_from)
                .transpose()?
                .is_some_and(|address| address.pid() != 0)
            {
                bail!("proc connector event did not originate from the kernel");
            }
            if let Some(status) = self.parse_datagram(&buffer[..received])? {
                return Ok(Some(process_from_raw_wait_status(status)));
            }
        }
    }

    fn drain_stale(&mut self) -> AnyResult<()> {
        if self.try_result()?.is_some() {
            bail!("target process exited before its OCI start attempt");
        }
        Ok(())
    }

    fn parse_datagram(&mut self, bytes: &[u8]) -> AnyResult<Option<u32>> {
        let mut offset = 0;
        while offset < bytes.len() {
            if bytes.len() - offset < NETLINK_HEADER_LEN {
                bail!("truncated proc connector netlink header");
            }
            let message_len = usize::try_from(read_u32(bytes, offset)?)
                .context("netlink message length cannot be represented")?;
            if message_len < NETLINK_HEADER_LEN || message_len > bytes.len() - offset {
                bail!("invalid proc connector netlink message length");
            }
            let message_type = read_u16(bytes, offset + 4)?;
            if message_type == 2 {
                bail!("proc connector returned NLMSG_ERROR");
            }
            if message_type == 3
                && let Some(status) = self.parse_connector_message(
                    &bytes[offset + NETLINK_HEADER_LEN..offset + message_len],
                )?
            {
                return Ok(Some(status));
            }
            offset = offset
                .checked_add((message_len + 3) & !3)
                .context("netlink alignment overflow")?;
        }
        Ok(None)
    }

    fn parse_connector_message(&mut self, bytes: &[u8]) -> AnyResult<Option<u32>> {
        if bytes.len() < CONNECTOR_HEADER_LEN {
            bail!("truncated proc connector header");
        }
        if read_u32(bytes, 0)? != CN_IDX_PROC || read_u32(bytes, 4)? != CN_VAL_PROC {
            return Ok(None);
        }
        let sequence = read_u32(bytes, 8)?;
        let data_len = usize::from(read_u16(bytes, 16)?);
        if data_len > bytes.len() - CONNECTOR_HEADER_LEN {
            bail!("invalid proc connector payload length");
        }
        let data = &bytes[CONNECTOR_HEADER_LEN..CONNECTOR_HEADER_LEN + data_len];
        if data.len() < 16 {
            bail!("truncated proc event header");
        }
        let cpu = read_u32(data, 4)?;
        if let Some(previous) = self.sequences.insert(cpu, sequence)
            && sequence != previous.wrapping_add(1)
        {
            bail!("proc connector sequence gap on CPU {cpu}");
        }
        if read_u32(data, 0)? != PROC_EVENT_EXIT {
            return Ok(None);
        }
        if data.len() < PROC_EVENT_EXIT_LEN {
            bail!("truncated proc exit event");
        }
        let process_pid = read_u32(data, 16)?;
        let process_tgid = read_u32(data, 20)?;
        if process_pid == self.target_pid && process_tgid == self.target_pid {
            return Ok(Some(read_u32(data, 24)?));
        }
        Ok(None)
    }

    fn send_control(&self, operation: u32) -> AnyResult<()> {
        let message = proc_connector_control_message(self.port_id, operation);
        let sent = sendto(
            &self.socket,
            &message,
            SendFlags::empty(),
            &SocketAddrNetlink::new(0, 0),
        )?;
        if sent != message.len() {
            bail!("short proc connector control send");
        }
        Ok(())
    }

    fn unsubscribe(&mut self) -> AnyResult<()> {
        if self.subscribed {
            self.send_control(PROC_CN_MCAST_IGNORE)?;
            self.subscribed = false;
        }
        Ok(())
    }
}

impl Drop for ProcExitMonitor {
    fn drop(&mut self) {
        if self.subscribed {
            let _ = self.unsubscribe();
        }
    }
}

fn proc_connector_control_message(port_id: u32, operation: u32) -> [u8; 40] {
    let mut message = [0_u8; 40];
    write_u32(&mut message, 0, 40);
    write_u16(&mut message, 4, 3);
    write_u32(&mut message, 8, 1);
    write_u32(&mut message, 12, port_id);
    write_u32(&mut message, 16, CN_IDX_PROC);
    write_u32(&mut message, 20, CN_VAL_PROC);
    write_u32(&mut message, 24, 1);
    write_u16(&mut message, 32, 4);
    write_u32(&mut message, 36, operation);
    message
}

fn read_u16(bytes: &[u8], offset: usize) -> AnyResult<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("truncated native-endian u16")?;
    Ok(u16::from_ne_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> AnyResult<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("truncated native-endian u32")?;
    Ok(u32::from_ne_bytes([value[0], value[1], value[2], value[3]]))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn pidfd_process_id(pidfd: &OwnedFd) -> AnyResult<u32> {
    let path = PathBuf::from(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()));
    let mut bytes = Vec::with_capacity(4097);
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("pidfd fdinfo exceeds 4096 bytes");
    }
    let text = std::str::from_utf8(&bytes).context("pidfd fdinfo is not UTF-8")?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("Pid:\t"))
        .context("pidfd fdinfo has no Pid field")?;
    let pid = value.parse::<u32>().context("invalid pidfd Pid field")?;
    if pid == 0 {
        bail!("pidfd Pid field is zero");
    }
    Ok(pid)
}

fn current_cgroup_base() -> AnyResult<PathBuf> {
    let current = cgroup_path_from_proc(Path::new("/proc/self/cgroup"))?;
    if current == Path::new("/sys/fs/cgroup") {
        return Ok(current);
    }
    current
        .parent()
        .map(Path::to_path_buf)
        .context("current cgroup path has no parent")
}

fn cgroup_path_from_proc(path: &Path) -> AnyResult<PathBuf> {
    let mut bytes = Vec::with_capacity(4097);
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("{} exceeds 4096 bytes", path.display());
    }
    let text = std::str::from_utf8(&bytes).context("process cgroup data is not UTF-8")?;
    let relative = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("host does not expose a unified cgroup-v2 path")?;
    let relative = Path::new(relative);
    if !relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        bail!(
            "invalid unified cgroup path {}",
            relative.as_os_str().display()
        );
    }
    Ok(Path::new("/sys/fs/cgroup").join(
        relative
            .strip_prefix("/")
            .expect("validated absolute cgroup path"),
    ))
}

fn observe_owned_cgroup(pid: u32, expected: &Path) -> AnyResult<PathBuf> {
    let path = PathBuf::from(format!("/proc/{pid}/cgroup"));
    let actual = cgroup_path_from_proc(&path)?;
    if actual != expected {
        bail!(
            "runc selected cgroup {}, expected the private default {}",
            actual.display(),
            expected.display()
        );
    }
    Ok(actual)
}

fn process_from_raw_wait_status(status: u32) -> ProcessResult {
    let signal = status & 0x7f;
    if signal == 0 {
        return ProcessResult::Exited {
            code: i32::try_from((status >> 8) & 0xff).expect("exit code fits i32"),
            ended_at: now(),
        };
    }
    if signal != 0x7f
        && let Some(signal) = NonZeroU32::new(signal)
    {
        return ProcessResult::Signaled {
            signal,
            ended_at: now(),
        };
    }
    ProcessResult::unknown(
        format!("proc connector reported unsupported raw wait status 0x{status:x}"),
        Availability::available(now()),
    )
    .expect("non-empty reason")
}

fn preflight_pidfd_socket(path: &Path) -> AnyResult<()> {
    if path.as_os_str().as_bytes().len() >= 108 {
        bail!("Unix socket path is too long: {}", path.display());
    }
    let listener = UnixListener::bind(path)?;
    drop(listener);
    fs::remove_file(path)?;
    Ok(())
}

fn poll_children(runs: &mut BTreeMap<ProgramId, ProgramRun>) -> bool {
    let mut failed = false;
    for run in runs.values_mut() {
        if run.poll_failed {
            failed = true;
            continue;
        }
        if let Err(error) = poll_one(run) {
            run.poll_failed = true;
            failed = true;
            run.errors.push(operation_error(
                OperationStage::Wait,
                format!("failed to poll runc supervisor: {error}"),
                error.raw_os_error().map(i64::from),
            ));
        }
    }
    failed
}

#[allow(
    clippy::too_many_lines,
    reason = "one ordered poll preserves raw-exit-before-stopped-fallback evidence precedence"
)]
fn poll_one(run: &mut ProgramRun) -> std::io::Result<bool> {
    run.pump_io();
    if run.process.is_some() {
        return Ok(false);
    }
    if let Some(monitor) = &mut run.exit_monitor {
        match monitor.try_result() {
            Ok(Some(result)) => {
                let unsubscribe = monitor.unsubscribe();
                run.observe_raw_process_result(result);
                run.exit_monitor = None;
                let state_probe_stop = run.state_probe.as_mut().map(RunningHelper::terminate);
                if state_probe_stop.as_ref().is_none_or(Result::is_ok) {
                    run.state_probe = None;
                    run.state_probe_deadline = None;
                }
                if let Err(error) = unsubscribe {
                    run.errors.push(operation_error(
                        OperationStage::Wait,
                        format!("failed to unsubscribe proc connector after exit: {error:#}"),
                        None,
                    ));
                }
                if let Some(Err(error)) = state_probe_stop {
                    run.supervisor_unreaped = true;
                    run.errors.push(operation_error(
                        OperationStage::Wait,
                        format!(
                            "raw exit was proved but the concurrent runc state probe was not reaped: {error:#}"
                        ),
                        None,
                    ));
                }
                return Ok(true);
            }
            Ok(None) => {}
            Err(error) => {
                let unsubscribe = monitor.unsubscribe();
                run.errors.push(operation_error(
                    OperationStage::Wait,
                    format!(
                        "raw process-result monitoring failed; termination will remain Unknown: {error:#}"
                    ),
                    None,
                ));
                if let Err(error) = unsubscribe {
                    run.errors.push(operation_error(
                        OperationStage::Wait,
                        format!("failed to unsubscribe invalid proc connector monitor: {error:#}"),
                        None,
                    ));
                }
                run.exit_monitor = None;
                if run.stopped_observation.is_some() {
                    run.fallback_after_stopped(
                        "runc state proved process termination, but raw exit monitoring failed",
                    );
                    return Ok(true);
                }
            }
        }
    }
    if run
        .stopped_observation
        .is_some_and(|(_, deadline)| Instant::now() >= deadline)
    {
        let unsubscribe = run.exit_monitor.as_mut().map(ProcExitMonitor::unsubscribe);
        run.exit_monitor = None;
        run.errors.push(operation_error(
            OperationStage::Wait,
            "raw proc exit event did not arrive before the process wait deadline",
            None,
        ));
        if let Some(Err(error)) = unsubscribe {
            run.errors.push(operation_error(
                OperationStage::Wait,
                format!("failed to unsubscribe proc connector after wait deadline: {error:#}"),
                None,
            ));
        }
        run.fallback_after_stopped(
            "runc state proved process termination, but no raw exit event arrived before the wait deadline",
        );
        return Ok(true);
    }
    if run.stopped_observation.is_some() {
        return Ok(false);
    }
    if run.state_probe_failed {
        return Ok(false);
    }
    let Some((runc, root, runtime_id, wait_timeout)) = run.runtime_coordinates.as_ref() else {
        return Ok(false);
    };
    if run.state_probe.is_none() {
        let mut command = runc_command(runc, root);
        command.arg("state").arg(runtime_id);
        run.state_probe = Some(
            RunningHelper::spawn(&run.supervisor, &mut command).map_err(std::io::Error::other)?,
        );
        run.state_probe_deadline = Some(
            checked_deadline(Instant::now(), *wait_timeout, "runc state probe deadline")
                .map_err(std::io::Error::other)?,
        );
        return Ok(false);
    }
    if run
        .state_probe_deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        let termination = run.state_probe.as_mut().expect("matched some").terminate();
        if termination.is_ok() {
            run.state_probe = None;
        } else {
            run.supervisor_unreaped = true;
        }
        run.state_probe_deadline = None;
        run.state_probe_failed = true;
        termination.map_err(std::io::Error::other)?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "runc state probe deadline exceeded",
        ));
    }
    let output = match run.state_probe.as_mut().expect("matched some").try_finish() {
        Ok(output) => output,
        Err(error) => {
            let termination = run.state_probe.as_mut().expect("matched some").terminate();
            if termination.is_ok() {
                run.state_probe = None;
            } else {
                run.supervisor_unreaped = true;
            }
            run.state_probe_deadline = None;
            run.state_probe_failed = true;
            termination.map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(format!(
                "runc state probe supervision failed: {error:#}"
            )));
        }
    };
    if let Some(output) = output {
        run.state_probe = None;
        run.state_probe_deadline = None;
        if !output.status.success() {
            run.state_probe_failed = true;
            return Err(std::io::Error::other(helper_message("runc state", &output)));
        }
        let state: RuncState = match serde_json::from_slice(&output.stdout) {
            Ok(state) => state,
            Err(error) => {
                run.state_probe_failed = true;
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        };
        if state.status == "stopped" {
            run.observe_runtime_stopped(*wait_timeout);
            return Ok(run.process.is_some());
        }
    }
    Ok(false)
}

fn stop_all(
    runc: &Path,
    root: &Path,
    programs: &BTreeMap<ProgramId, PreparedProgram>,
    outcomes: &mut BTreeMap<ProgramId, ProgramRun>,
    timeouts: OperationTimeouts,
) {
    let ids = outcomes
        .iter()
        .filter(|(_, run)| {
            matches!(
                run.start.status(),
                OperationStatus::Succeeded | OperationStatus::Unknown
            ) && run.process.is_none()
                && !run.writer_stopped
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }
    let first_term = Instant::now();
    let grace = checked_deadline(first_term, STOP_GRACE_PERIOD, "shared stop grace")
        .expect("fixed grace fits Instant");
    let term_helper_cap = grace
        .checked_sub(Duration::from_secs(2))
        .expect("shared grace exceeds bounded helper reap reserve");
    signal_all(
        runc,
        root,
        programs,
        outcomes,
        &ids,
        StopSignal::Term,
        timeouts.term_signal(),
        Some(term_helper_cap),
    );
    while Instant::now() < grace {
        poll_children(outcomes);
        if ids.iter().all(|id| outcomes[id].process.is_some()) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let remaining = ids
        .into_iter()
        .filter(|id| outcomes[id].process.is_none())
        .collect::<Vec<_>>();
    signal_all(
        runc,
        root,
        programs,
        outcomes,
        &remaining,
        StopSignal::Kill,
        timeouts.kill_signal(),
        None,
    );
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "signal phase supervision keeps concurrent launch, one absolute deadline, bounded reap, and per-Program evidence in one ordering boundary"
)]
fn signal_all(
    runc: &Path,
    root: &Path,
    programs: &BTreeMap<ProgramId, PreparedProgram>,
    outcomes: &mut BTreeMap<ProgramId, ProgramRun>,
    ids: &[ProgramId],
    signal: StopSignal,
    timeout: Duration,
    absolute_cap: Option<Instant>,
) {
    let signal_text = match signal {
        StopSignal::Term => "TERM",
        StopSignal::Kill => "KILL",
    };
    let started = Instant::now();
    let phase_deadline = checked_deadline(started, timeout, "runc signal deadline")
        .expect("validated OperationTimeouts fit Instant");
    let phase_deadline = absolute_cap.map_or(phase_deadline, |cap| cap.min(phase_deadline));
    let mut attempts = Vec::with_capacity(ids.len());
    for id in ids {
        let attempted_at = now();
        let mut command = runc_command(runc, root);
        command
            .arg("kill")
            .arg("--all")
            .arg(&programs[id].runtime_id)
            .arg(signal_text);
        let (helper, spawn_error) =
            match RunningHelper::spawn(&outcomes[id].supervisor, &mut command) {
                Ok(helper) => (Some(helper), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
        attempts.push(TermAttempt {
            id: id.clone(),
            attempted_at,
            helper,
            spawn_error,
            output: None,
        });
    }
    while Instant::now() < phase_deadline
        && attempts
            .iter()
            .any(|attempt| attempt.helper.is_some() && attempt.output.is_none())
    {
        for attempt in &mut attempts {
            if let Some(helper) = &mut attempt.helper {
                match helper.try_finish() {
                    Ok(Some(output)) => attempt.output = Some(Ok(output)),
                    Ok(None) => {}
                    Err(error) => attempt.output = Some(Err(error)),
                }
            }
        }
        poll_children(outcomes);
        thread::sleep(POLL_INTERVAL);
    }
    for attempt in &mut attempts {
        if let Some(helper) = &mut attempt.helper
            && !helper.is_reaped()
            && let Err(error) = helper.request_terminate()
        {
            attempt.output = Some(Err(error));
        }
    }
    let reap_deadline = checked_deadline(
        Instant::now(),
        SUPERVISOR_REAP_LIMIT,
        "signal helper reap deadline",
    )
    .expect("fixed helper reap limit fits Instant");
    while Instant::now() < reap_deadline
        && attempts.iter().any(|attempt| {
            attempt
                .helper
                .as_ref()
                .is_some_and(|helper| !helper.is_reaped())
        })
    {
        for attempt in &mut attempts {
            if let Some(helper) = &mut attempt.helper
                && !helper.is_reaped()
                && let Err(error) = helper.poll_reaped()
            {
                attempt.output = Some(Err(error));
            }
        }
        poll_children(outcomes);
        thread::sleep(POLL_INTERVAL);
    }
    for mut attempt in attempts {
        let output = attempt.output.take();
        let unreaped = attempt
            .helper
            .as_ref()
            .is_some_and(|helper| !helper.is_reaped());
        let mut result = match output {
            Some(Ok(output)) if output.status.success() => StopActionResult::Accepted,
            Some(Ok(output)) => StopActionResult::Rejected(operation_error(
                OperationStage::Signal,
                helper_message(&format!("runc kill {signal_text}"), &output),
                output.status.code().map(i64::from),
            )),
            Some(Err(error)) => {
                StopActionResult::unknown(format!("runc kill {signal_text}: {error:#}"), [])
                    .expect("non-empty reason")
            }
            None if attempt.spawn_error.is_some() => StopActionResult::unknown(
                format!(
                    "failed to spawn runc kill {signal_text}: {}",
                    attempt.spawn_error.as_deref().expect("matched some")
                ),
                [],
            )
            .expect("non-empty reason"),
            None => StopActionResult::unknown(
                format!("runc kill {signal_text} did not report before its deadline"),
                [],
            )
            .expect("literal reason"),
        };
        if unreaped {
            result = StopActionResult::unknown(
                format!("runc kill {signal_text} helper was not reaped before its bounded confirmation deadline"),
                [],
            )
            .expect("non-empty reason");
        }
        let outcome = outcomes.get_mut(&attempt.id).expect("output slot");
        outcome.supervisor_unreaped |= unreaped;
        outcome
            .stop_actions
            .push(StopAction::new(signal, attempt.attempted_at, result));
    }
}

struct TermAttempt {
    id: ProgramId,
    attempted_at: DateTime<FixedOffset>,
    helper: Option<RunningHelper>,
    spawn_error: Option<String>,
    output: Option<AnyResult<HelperOutput>>,
}

struct InputTransfer {
    pipe: Option<ChildStdin>,
    bytes: Vec<u8>,
    written: usize,
    error: Option<String>,
}

impl InputTransfer {
    fn new(pipe: ChildStdin, bytes: Vec<u8>) -> Self {
        let error = set_nonblocking(&pipe).err().map(|error| error.to_string());
        Self {
            pipe: error.is_none().then_some(pipe),
            bytes,
            written: 0,
            error,
        }
    }

    fn pump(&mut self) {
        let Some(pipe) = &mut self.pipe else {
            return;
        };
        let mut pumped = 0;
        while self.written < self.bytes.len() && pumped < PIPE_PUMP_BYTE_BUDGET {
            match pipe.write(&self.bytes[self.written..]) {
                Ok(0) => {
                    self.error = Some("stdin pipe accepted zero bytes".to_owned());
                    self.pipe = None;
                    return;
                }
                Ok(count) => {
                    self.written += count;
                    pumped += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.pipe = None;
                    return;
                }
            }
        }
        if self.written == self.bytes.len() {
            self.pipe = None;
        }
    }

    fn freeze(&mut self) {
        if self.pipe.is_some() && self.written < self.bytes.len() && self.error.is_none() {
            self.error =
                Some("stdin transfer stopped when execution entered termination".to_owned());
        }
        self.pipe = None;
    }

    fn finish(mut self) -> StdinOutput {
        self.pump();
        let facts = StdinWriteFacts::new(u64::try_from(self.written).unwrap_or(u64::MAX));
        let write = if let Some(error) = self.error {
            OperationReport::<StdinWriteFacts>::failed_with_facts(
                facts,
                operation_error(OperationStage::StdinWrite, error, None),
                [],
            )
        } else if self.pipe.is_some() {
            OperationReport::<StdinWriteFacts>::failed_with_facts(
                facts,
                operation_error(
                    OperationStage::StdinWrite,
                    "stdin transfer did not complete before stream drain deadline",
                    None,
                ),
                [],
            )
        } else {
            OperationReport::succeeded(facts)
        };
        self.pipe = None;
        StdinOutput::new(write, OperationReport::succeeded(()))
    }
}

struct StreamDrain {
    pipe: Option<Box<dyn Read>>,
    bytes: Vec<u8>,
    omitted: bool,
    eof: bool,
    error: Option<String>,
}

impl StreamDrain {
    fn from_stdout(pipe: ChildStdout) -> Self {
        Self::new(pipe)
    }

    fn from_stderr(pipe: ChildStderr) -> Self {
        Self::new(pipe)
    }

    fn new<R: Read + AsFd + 'static>(pipe: R) -> Self {
        let error = set_nonblocking(&pipe).err().map(|error| error.to_string());
        Self {
            pipe: error.is_none().then_some(Box::new(pipe)),
            bytes: Vec::new(),
            omitted: false,
            eof: false,
            error,
        }
    }

    fn pump(&mut self) {
        let Some(pipe) = &mut self.pipe else {
            return;
        };
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        let mut pumped = 0;
        while pumped < PIPE_PUMP_BYTE_BUDGET {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    self.pipe = None;
                    return;
                }
                Ok(count) => {
                    pumped += count;
                    let available = MAX_CAPTURED_STREAM_BYTES.saturating_sub(self.bytes.len());
                    let keep = available.min(count);
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.omitted |= keep < count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.pipe = None;
                    return;
                }
            }
        }
    }

    fn finish(mut self, stage: OperationStage) -> OperationReport<StreamFacts> {
        self.pump();
        if self.pipe.is_some() && self.error.is_none() {
            self.error = Some("stream did not reach EOF before stream drain deadline".to_owned());
        }
        self.pipe = None;
        let facts = StreamFacts::new(self.bytes, self.omitted, self.eof)
            .expect("nonblocking drainer preserves stream shape");
        match self.error {
            Some(message) => OperationReport::<StreamFacts>::failed_with_facts(
                facts,
                operation_error(stage, message, None),
                [],
            ),
            None => OperationReport::succeeded(facts),
        }
    }
}

fn set_nonblocking(fd: &impl AsFd) -> std::io::Result<()> {
    let flags = fcntl_getfl(fd)?;
    Ok(fcntl_setfl(fd, flags | OFlags::NONBLOCK)?)
}

#[allow(
    clippy::too_many_lines,
    reason = "one ordered finalizer preserves wait, forced confirmation, supervisor reap, and stream closure boundaries"
)]
fn finalize_children(runs: &mut BTreeMap<ProgramId, ProgramRun>, timeouts: OperationTimeouts) {
    let wait_deadline = checked_deadline(Instant::now(), timeouts.wait(), "process wait deadline")
        .expect("validated OperationTimeouts fit Instant");
    loop {
        poll_children(runs);
        if runs
            .values()
            .all(|run| run.runtime_coordinates.is_none() || run.process.is_some())
            || Instant::now() >= wait_deadline
        {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let mut confirmation_needed = false;
    for run in runs.values_mut() {
        if run.runtime_coordinates.is_some() && run.process.is_none() {
            confirmation_needed = true;
            run.errors.push(operation_error(
                OperationStage::Wait,
                "process wait deadline exceeded; entering forced-stop confirmation",
                None,
            ));
        }
    }
    if confirmation_needed {
        let confirmation_deadline = checked_deadline(
            Instant::now(),
            timeouts.forced_stop_confirmation(),
            "forced-stop confirmation deadline",
        )
        .expect("validated OperationTimeouts fit Instant");
        loop {
            poll_children(runs);
            if runs
                .values()
                .all(|run| run.runtime_coordinates.is_none() || run.process.is_some())
                || Instant::now() >= confirmation_deadline
            {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
    for run in runs.values_mut() {
        if let Some(mut monitor) = run.exit_monitor.take()
            && let Err(error) = monitor.unsubscribe()
        {
            run.errors.push(operation_error(
                OperationStage::Wait,
                format!("failed to unsubscribe proc connector during finalization: {error:#}"),
                None,
            ));
        }
        if run.child.is_some()
            && let Err(error) = terminate_child(
                &run.supervisor,
                &mut run.child,
                timeouts.forced_stop_confirmation(),
            )
        {
            run.supervisor_unreaped = true;
            run.errors.push(operation_error(
                OperationStage::Wait,
                format!("failed to terminate and reap runc create supervisor: {error:#}"),
                None,
            ));
        }
        if let Some(helper) = &mut run.state_probe {
            match helper.terminate() {
                Ok(()) => {
                    run.state_probe = None;
                    run.state_probe_deadline = None;
                }
                Err(error) => run.errors.push(operation_error(
                    OperationStage::Wait,
                    format!("failed to terminate and reap runc state probe: {error:#}"),
                    None,
                )),
            }
        }
        run.supervisor_unreaped |= run
            .state_probe
            .as_ref()
            .is_some_and(|helper| !helper.is_reaped());
        if run.runtime_coordinates.is_some() && run.process.is_none() {
            let termination = run.state_probe.as_mut().map(RunningHelper::terminate);
            if termination.as_ref().is_none_or(Result::is_ok) {
                run.state_probe = None;
            }
            run.process = Some(
                ProcessResult::unknown(
                    "the container did not reach a directly observed stopped state before the forced-stop confirmation deadline",
                    Availability::unavailable("no process-end observation was obtained")
                        .expect("literal reason"),
                )
                .expect("literal reason"),
            );
            run.writer_stopped = false;
            run.errors.push(operation_error(
                OperationStage::Wait,
                termination.and_then(Result::err).map_or_else(
                    || "forced-stop confirmation deadline exceeded".to_owned(),
                    |error| format!("forced-stop confirmation failed: {error:#}"),
                ),
                None,
            ));
        }
    }
    let drain_deadline = checked_deadline(
        Instant::now(),
        timeouts.stream_drain(),
        "stream drain deadline",
    )
    .expect("validated OperationTimeouts fit Instant");
    loop {
        for run in runs.values_mut() {
            run.pump_output();
        }
        if runs.values().all(ProgramRun::io_complete) || Instant::now() >= drain_deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    for run in runs.values_mut() {
        run.stdin = Some(
            run.stdin_transfer
                .take()
                .map_or_else(unattempted_stdin, InputTransfer::finish),
        );
        run.stdout = Some(
            run.stdout_drain
                .take()
                .map_or_else(unattempted_stream, |drain| {
                    drain.finish(OperationStage::StdoutRead)
                }),
        );
        run.stderr = Some(
            run.stderr_drain
                .take()
                .map_or_else(unattempted_stream, |drain| {
                    drain.finish(OperationStage::StderrRead)
                }),
        );
    }
}

fn unattempted_stdin() -> StdinOutput {
    StdinOutput::new(
        OperationReport::not_attempted("stdin was not connected").expect("literal reason"),
        OperationReport::not_attempted("stdin was not connected").expect("literal reason"),
    )
}

fn unattempted_stream() -> OperationReport<StreamFacts> {
    OperationReport::not_attempted("the Program was not started").expect("literal reason")
}

#[allow(
    clippy::too_many_lines,
    reason = "one absolute cleanup deadline must visibly cover runtime deletion, cgroup removal, mount verification, and owned artifact removal"
)]
fn cleanup_runtime(
    runc: &Path,
    runtime_root: &Path,
    program: &PreparedProgram,
    run: &mut ProgramRun,
    timeouts: OperationTimeouts,
    supervisor_deadline: Instant,
) {
    let deadline = checked_deadline(
        Instant::now(),
        timeouts.runtime_filesystem_removal(),
        "runtime filesystem removal deadline",
    )
    .expect("validated OperationTimeouts fit Instant")
    .min(supervisor_deadline);
    if Instant::now() >= deadline {
        record_cleanup_deadline(run, "before runc deletion");
        return;
    }
    if run.runtime_attempted {
        match run_helper_until(
            &run.supervisor,
            runc_command(runc, runtime_root)
                .arg("delete")
                .arg("--force")
                .arg(&program.runtime_id),
            deadline,
            None,
        ) {
            Ok(output) if output.status.success() => {
                run.writer_stopped |= run.child.is_none() && run.state_probe.is_none();
                if run.process.is_none() && run.start.status() != OperationStatus::NotAttempted {
                    run.process = Some(
                        ProcessResult::unknown(
                            "runc delete --force proved the runtime object and possible process were removed without an unflattened process result",
                            Availability::available(now()),
                        )
                        .expect("literal reason"),
                    );
                }
            }
            Ok(output) => run.errors.push(operation_error(
                OperationStage::RuntimeFilesystemRemoval,
                helper_message("runc delete --force", &output),
                output.status.code().map(i64::from),
            )),
            Err(error) => {
                run.supervisor_unreaped |= !error.supervisor_reaped;
                run.errors.push(operation_error(
                    OperationStage::RuntimeFilesystemRemoval,
                    format!("runc delete --force: {error:#}"),
                    None,
                ));
            }
        }
    }
    if Instant::now() >= deadline {
        record_cleanup_deadline(run, "after runc deletion");
        return;
    }
    let mut cgroups = BTreeSet::from([program.expected_cgroup_path.clone()]);
    if let Some(path) = &run.cgroup_path {
        cgroups.insert(path.clone());
    }
    for cgroup in cgroups {
        if let Err(error) = remove_owned_cgroup(&cgroup, &program.runtime_id, deadline) {
            run.errors.push(operation_error(
                OperationStage::RuntimeFilesystemRemoval,
                format!(
                    "failed to remove Engine-owned cgroup {}: {error:#}",
                    cgroup.display()
                ),
                None,
            ));
        }
    }
    if Instant::now() >= deadline {
        record_cleanup_deadline(run, "after cgroup removal");
        return;
    }
    if let Err(error) = program.rootfs.ensure_no_mounts() {
        run.errors.push(operation_error(
            OperationStage::RuntimeFilesystemRemoval,
            format!("mounts remain after runc deletion: {error:#}"),
            None,
        ));
        return;
    }
    if Instant::now() >= deadline {
        record_cleanup_deadline(run, "after mount verification");
        return;
    }
    for relative in program.artifacts.iter().rev() {
        if Instant::now() >= deadline {
            record_cleanup_deadline(run, "while removing runtime-created mount artifacts");
            return;
        }
        let path = program.rootfs.path().join(relative);
        let result = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir(&path),
            Ok(_) => {
                run.errors.push(operation_error(
                    OperationStage::RuntimeFilesystemRemoval,
                    format!(
                        "runtime-created mount artifact {} changed from an expected directory; it was preserved",
                        path.display()
                    ),
                    None,
                ));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::DirectoryNotEmpty
        {
            run.errors.push(operation_error(
                OperationStage::RuntimeFilesystemRemoval,
                format!(
                    "failed to remove runtime-created mount artifact {}: {error}",
                    path.display()
                ),
                error.raw_os_error().map(i64::from),
            ));
        }
    }
}

fn record_cleanup_deadline(run: &mut ProgramRun, phase: &str) {
    run.writer_stopped = false;
    run.errors.push(operation_error(
        OperationStage::RuntimeFilesystemRemoval,
        format!("runtime filesystem removal deadline exceeded {phase}"),
        None,
    ));
}

fn remove_owned_cgroup(path: &Path, runtime_id: &str, deadline: Instant) -> AnyResult<()> {
    if Instant::now() >= deadline {
        bail!("runtime filesystem removal deadline exceeded before cgroup cleanup");
    }
    if !path.starts_with("/sys/fs/cgroup")
        || path.file_name().and_then(|name| name.to_str()) != Some(runtime_id)
    {
        bail!("refusing to remove a cgroup not owned by runtime id {runtime_id}");
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("owned cgroup path is not a directory");
    }
    let processes = fs::read_to_string(path.join("cgroup.procs"))?;
    if !processes.trim().is_empty() {
        bail!("owned cgroup still contains processes after runc deletion");
    }
    for entry in fs::read_dir(path)? {
        if Instant::now() >= deadline {
            bail!("runtime filesystem removal deadline exceeded during cgroup inspection");
        }
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            bail!(
                "owned cgroup still contains child cgroup {} after runc deletion",
                entry.path().display()
            );
        }
    }
    if Instant::now() >= deadline {
        bail!("runtime filesystem removal deadline exceeded before cgroup removal");
    }
    fs::remove_dir(path)?;
    Ok(())
}

fn capture_final(
    engine: &NativeEngine,
    program: &PreparedProgram,
    run: &mut ProgramRun,
) -> Availability<ImageDescriptor> {
    if !run.writer_stopped {
        return Availability::unavailable(
            "a process that could still write the rootfs was not proved stopped",
        )
        .expect("literal reason");
    }
    if run
        .errors
        .iter()
        .any(|error| error.stage() == OperationStage::RuntimeFilesystemRemoval)
    {
        return Availability::unavailable(
            "runtime filesystems or their owned mount artifacts were not proved removed",
        )
        .expect("literal reason");
    }
    let budget = match OperationBudget::new(
        engine.timeouts.final_environment_capture(),
        "final environment capture",
    ) {
        Ok(budget) => budget,
        Err(error) => {
            run.errors.push(operation_error(
                OperationStage::FinalEnvironmentCapture,
                format!("failed to establish final environment capture deadline: {error:#}"),
                None,
            ));
            return Availability::unavailable(
                "the final environment capture deadline could not be established",
            )
            .expect("literal reason");
        }
    };
    let store = BudgetedStore::new(Arc::clone(&engine.store), budget);
    let result = (|| -> AnyResult<ImageDescriptor> {
        budget.check()?;
        program.rootfs.ensure_no_mounts()?;
        budget.check()?;
        let captured = program.rootfs.capture()?;
        budget.check()?;
        let image =
            publish_capture(&store, &program.parent, &captured).map_err(anyhow::Error::from)?;
        budget.check()?;
        Ok(image)
    })();
    match result {
        Ok(image) => Availability::available(image),
        Err(error) => {
            run.errors.push(operation_error(
                OperationStage::FinalEnvironmentCapture,
                format!("failed to capture final environment: {error:#}"),
                None,
            ));
            Availability::unavailable(format!("failed to capture final environment: {error:#}"))
                .expect("non-empty reason")
        }
    }
}

fn publish_capture(
    store: &dyn OciContentStore,
    parent: &ImageDescriptor,
    captured: &CapturedLayer,
) -> Result<ImageDescriptor, crate::oci::OciError> {
    let descriptor = Descriptor::new(
        captured.media_type.clone(),
        captured.size,
        captured.diff_id.clone(),
    );
    let mut reader = captured.open().map_err(|error| crate::oci::OciError::Io {
        path: "final.layer".to_owned(),
        source: std::io::Error::other(error.to_string()),
    })?;
    publish_expected(
        store,
        &descriptor,
        &mut reader,
        &[MediaType::ImageLayer],
        "final.layer",
    )?;
    publish_final_image(store, parent, Some((descriptor, captured.diff_id.clone())))
}

fn cleanup_invocation(
    prepared: &mut PreparedInvocation,
    all_writers_stopped: bool,
    budget: OperationBudget,
) -> Option<OperationError> {
    if let Err(error) = budget.check() {
        if let Some(workspace) = prepared.workspace.take() {
            let _preserved = workspace.keep();
        }
        return Some(operation_error(
            OperationStage::Cleanup,
            format!("cleanup deadline exceeded before workspace removal: {error:#}"),
            None,
        ));
    }
    if !all_writers_stopped
        || prepared
            .programs
            .values()
            .any(|program| program.rootfs.ensure_no_mounts().is_err())
    {
        if let Some(workspace) = prepared.workspace.take() {
            let preserved = workspace.keep();
            return Some(operation_error(
                OperationStage::Cleanup,
                format!(
                    "preserved workspace {} because a writer may still be active or a residual mount could not be excluded",
                    preserved.display()
                ),
                None,
            ));
        }
        return None;
    }
    prepared.workspace.take().and_then(|workspace| {
        let result = workspace.close();
        if let Err(error) = budget.check() {
            return Some(operation_error(
                OperationStage::Cleanup,
                format!("cleanup deadline exceeded while removing workspace: {error:#}"),
                None,
            ));
        }
        result.err().map(|error| {
            operation_error(
                OperationStage::Cleanup,
                format!("failed to remove invocation workspace: {error}"),
                error.raw_os_error().map(i64::from),
            )
        })
    })
}

fn validate_runtime(id: &ProgramId, program: &ProgramInput) -> Result<(), EngineError> {
    let base = program_path(id).child("runtime_config");
    let value = program.runtime_config().as_json();
    if value
        .pointer("/root/path")
        .and_then(serde_json::Value::as_str)
        != Some("rootfs")
    {
        return Err(EngineError::unsupported(
            base.clone().child("root").child("path"),
            "NativeEngine materializes each private rootfs at the exact bundle path rootfs",
        ));
    }
    if value
        .pointer("/process/terminal")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(EngineError::unsupported(
            base.clone().child("process").child("terminal"),
            "NativeEngine implements independent byte streams and does not allocate a terminal",
        ));
    }
    if value.pointer("/linux/cgroupsPath").is_some() {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("cgroupsPath"),
            "NativeEngine requires its unique runtime id to select an Engine-owned cgroup; caller-selected cgroupsPath has external or concurrent ownership",
        ));
    }
    validate_isolated_host_boundaries(&base, value)?;
    let namespaces = value
        .pointer("/linux/namespaces")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            EngineError::unsupported(
                base.clone().child("linux").child("namespaces"),
                "isolated execution requires an explicit new network namespace",
            )
        })?;
    let required_namespaces = ["cgroup", "ipc", "mount", "network", "pid", "uts"];
    let mut observed_namespaces = BTreeSet::new();
    for (index, namespace) in namespaces.iter().enumerate() {
        if namespace.get("path").is_some_and(|path| !path.is_null()) {
            return Err(EngineError::unsupported(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("path"),
                "isolated NativeEngine execution requires newly created namespaces, not existing host namespaces",
            ));
        }
        let namespace_type = namespace
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !required_namespaces.contains(&namespace_type) {
            return Err(EngineError::unsupported(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("type"),
                "isolated NativeEngine execution supports only new pid, network, ipc, uts, mount, and cgroup namespaces",
            ));
        }
        if !observed_namespaces.insert(namespace_type) {
            return Err(EngineError::invalid(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("type"),
                "namespace type is duplicated",
            ));
        }
    }
    if !required_namespaces
        .iter()
        .all(|namespace| observed_namespaces.contains(namespace))
    {
        return Err(EngineError::unsupported(
            base.child("linux").child("namespaces"),
            "isolated execution requires new pid, network, ipc, uts, mount, and cgroup namespaces",
        ));
    }
    Ok(())
}

fn validate_isolated_host_boundaries(
    base: &InputPath,
    value: &serde_json::Value,
) -> Result<(), EngineError> {
    if value.pointer("/process/noNewPrivileges") != Some(&serde_json::Value::Bool(true)) {
        return Err(EngineError::unsupported(
            base.clone().child("process").child("noNewPrivileges"),
            "isolated rootful execution requires noNewPrivileges=true",
        ));
    }
    if value
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .any(|hooks| hooks.as_array().is_none_or(|hooks| !hooks.is_empty()))
        })
    {
        return Err(EngineError::unsupported(
            base.clone().child("hooks"),
            "isolated NativeEngine execution does not permit caller-controlled host hooks",
        ));
    }
    validate_isolated_mounts(base, value)?;
    let capabilities = value
        .pointer("/process/capabilities")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            EngineError::unsupported(
                base.clone().child("process").child("capabilities"),
                "isolated rootful execution requires all five capability sets to be explicitly empty",
            )
        })?;
    for set in [
        "bounding",
        "effective",
        "inheritable",
        "permitted",
        "ambient",
    ] {
        if capabilities
            .get(set)
            .and_then(serde_json::Value::as_array)
            .is_none_or(|entries| !entries.is_empty())
        {
            return Err(EngineError::unsupported(
                base.clone()
                    .child("process")
                    .child("capabilities")
                    .child(set),
                "isolated rootful execution requires this capability set to be explicitly empty",
            ));
        }
    }
    if value
        .pointer("/linux/devices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|devices| !devices.is_empty())
    {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("devices"),
            "isolated NativeEngine execution does not permit explicit host devices",
        ));
    }
    if value.pointer("/linux/seccomp/listenerPath").is_some() {
        return Err(EngineError::unsupported(
            base.clone()
                .child("linux")
                .child("seccomp")
                .child("listenerPath"),
            "isolated NativeEngine execution does not permit a host seccomp listener path",
        ));
    }
    if value.pointer("/linux/rootfsPropagation").is_some() {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("rootfsPropagation"),
            "isolated NativeEngine execution does not permit caller-selected rootfs propagation",
        ));
    }
    for field in ["uidMappings", "gidMappings", "sysctl", "intelRdt"] {
        if value.pointer(&format!("/linux/{field}")).is_some() {
            return Err(EngineError::unsupported(
                base.clone().child("linux").child(field),
                "isolated NativeEngine does not implement this additional host-kernel boundary",
            ));
        }
    }
    Ok(())
}

fn validate_isolated_mounts(
    base: &InputPath,
    value: &serde_json::Value,
) -> Result<(), EngineError> {
    for (index, mount) in value
        .get("mounts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mount_type = mount
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let bind = mount_type == "bind"
            || mount
                .get("options")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|options| {
                    options
                        .iter()
                        .any(|option| matches!(option.as_str(), Some("bind" | "rbind")))
                });
        if bind {
            return Err(EngineError::unsupported(
                base.clone().child("mounts").index(index),
                "isolated NativeEngine execution does not permit bind mounts across the host boundary",
            ));
        }
        if !matches!(mount_type, "proc" | "tmpfs" | "sysfs") {
            return Err(EngineError::unsupported(
                base.clone().child("mounts").index(index).child("type"),
                "isolated NativeEngine execution supports only proc, tmpfs, and sysfs mounts",
            ));
        }
        if mount_type == "sysfs"
            && !mount
                .get("options")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|options| options.iter().any(|option| option.as_str() == Some("ro")))
        {
            return Err(EngineError::unsupported(
                base.clone().child("mounts").index(index).child("options"),
                "isolated NativeEngine sysfs mounts must be read-only",
            ));
        }
    }
    Ok(())
}

fn validate_host_resources(id: &ProgramId, program: &ProgramInput) -> Result<(), EngineError> {
    let base = program_path(id).child("runtime_config");
    let value = program.runtime_config().as_json();
    if let Some(mounts) = value.get("mounts").and_then(serde_json::Value::as_array) {
        for (index, mount) in mounts.iter().enumerate() {
            let destination_path = base
                .clone()
                .child("mounts")
                .index(index)
                .child("destination");
            let destination = mount
                .get("destination")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    EngineError::invalid(destination_path.clone(), "mount destination is required")
                })?;
            safe_container_path(destination).map_err(|error| {
                EngineError::invalid(
                    destination_path,
                    format!("invalid mount destination: {error:#}"),
                )
            })?;
            let bind = mount.get("type").and_then(serde_json::Value::as_str) == Some("bind")
                || mount
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|options| {
                        options
                            .iter()
                            .any(|option| matches!(option.as_str(), Some("bind" | "rbind")))
                    });
            if bind {
                let path = base.clone().child("mounts").index(index).child("source");
                let source = mount
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        EngineError::invalid(path.clone(), "bind mount source is required")
                    })?;
                validate_host_path(source, path)?;
            }
        }
    }
    if let Some(namespaces) = value
        .pointer("/linux/namespaces")
        .and_then(serde_json::Value::as_array)
    {
        for (index, namespace) in namespaces.iter().enumerate() {
            if let Some(path) = namespace.get("path").and_then(serde_json::Value::as_str) {
                validate_host_path(
                    path,
                    base.clone()
                        .child("linux")
                        .child("namespaces")
                        .index(index)
                        .child("path"),
                )?;
            }
        }
    }
    for phase in [
        "prestart",
        "createRuntime",
        "createContainer",
        "startContainer",
        "poststart",
        "poststop",
    ] {
        if let Some(hooks) = value
            .pointer(&format!("/hooks/{phase}"))
            .and_then(serde_json::Value::as_array)
        {
            for (index, hook) in hooks.iter().enumerate() {
                let path = base
                    .clone()
                    .child("hooks")
                    .child(phase)
                    .index(index)
                    .child("path");
                let executable = hook
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| EngineError::invalid(path.clone(), "hook path is required"))?;
                validate_hook_path(executable, path)?;
            }
        }
    }
    Ok(())
}

fn validate_host_path(raw: &str, path: InputPath) -> Result<(), EngineError> {
    let value = Path::new(raw);
    if !value.is_absolute() {
        return Err(EngineError::invalid(
            path,
            "explicit host resource path must be absolute",
        ));
    }
    fs::symlink_metadata(value).map_err(|error| {
        EngineError::input_unavailable(path, format!("cannot inspect host resource {raw}: {error}"))
    })?;
    Ok(())
}

fn validate_hook_path(raw: &str, path: InputPath) -> Result<(), EngineError> {
    validate_host_path(raw, path.clone())?;
    let metadata = fs::metadata(raw).map_err(|error| {
        EngineError::input_unavailable(
            path.clone(),
            format!("cannot inspect hook executable {raw}: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(EngineError::input_unavailable(
            path,
            format!("hook path {raw} is not an executable regular file"),
        ));
    }
    Ok(())
}

fn validate_platform(id: &ProgramId, image: &VerifiedImage) -> Result<(), EngineError> {
    let platform_path = program_path(id)
        .child("initial_environment")
        .child("platform");
    let os = image.platform().os().to_string();
    if os != "linux" {
        return Err(EngineError::unsupported(
            platform_path.clone().child("os"),
            format!("image operating system {os} cannot execute on the Linux NativeEngine"),
        ));
    }
    let actual = image.platform().architecture().to_string();
    let expected = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    if actual != expected {
        return Err(EngineError::unsupported(
            platform_path.clone().child("architecture"),
            format!("image architecture {actual} cannot execute on host architecture {expected}"),
        ));
    }
    if let Some(variant) = image.platform().variant()
        && !(expected == "arm64" && variant == "v8")
    {
        return Err(EngineError::unsupported(
            platform_path.clone().child("variant"),
            format!(
                "NativeEngine cannot prove image CPU variant {variant}; the aarch64 build target proves only the arm64/v8 baseline"
            ),
        ));
    }
    let config: serde_json::Value = serde_json::from_slice(image.config().bytes())
        .expect("VerifiedImage retains already validated config JSON");
    for (field, reason) in [
        (
            "os.version",
            "NativeEngine has not proved an image OS-version contract against the host kernel",
        ),
        (
            "os.features",
            "NativeEngine has not proved image-required OS features against the host",
        ),
        (
            "features",
            "NativeEngine does not implement reserved OCI platform features",
        ),
    ] {
        if config.get(field).is_some() {
            return Err(EngineError::unsupported(
                platform_path.clone().child(field),
                reason,
            ));
        }
    }
    Ok(())
}

fn map_oci_error(id: &ProgramId, error: &crate::oci::OciError) -> EngineError {
    let path = program_path(id).child("initial_environment");
    let reason = error.to_string();
    if error.kind() == OciErrorKind::Content {
        EngineError::input_unavailable(path, reason)
    } else {
        EngineError::invalid(path, reason)
    }
}

fn program_path(id: &ProgramId) -> InputPath {
    InputPath::field("programs").key(id.as_str())
}

fn validate_private_directory(path: &Path, label: &str) -> Result<PathBuf, EngineError> {
    if !path.is_absolute() {
        return Err(EngineError::internal(format!(
            "{label} must be absolute: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        EngineError::internal(format!(
            "failed to resolve {label} {}: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|error| EngineError::internal(format!("failed to inspect {label}: {error}")))?;
    if !metadata.is_dir() || metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(EngineError::internal(format!(
            "{label} must be an owned private directory with mode 0700 or stricter"
        )));
    }
    Ok(canonical)
}

fn validate_runc(
    path: &Path,
    budget: OperationBudget,
    supervisor: &InvocationSupervisor,
) -> Result<PathBuf, EngineError> {
    if !path.is_absolute() {
        return Err(EngineError::internal(format!(
            "runc executable must be absolute: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        EngineError::internal(format!("failed to resolve runc executable: {error}"))
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        EngineError::internal(format!("failed to inspect runc executable: {error}"))
    })?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(EngineError::internal(
            "runc executable is not an executable regular file",
        ));
    }
    let timeout = budget
        .remaining()
        .map_err(|error| EngineError::internal(format!("{error:#}")))?;
    let output = run_helper(
        supervisor,
        Command::new(&canonical).arg("--version"),
        timeout,
    )
    .map_err(|error| EngineError::internal(format!("runc --version failed: {error:#}")))?;
    if !output.status.success() {
        return Err(EngineError::internal(helper_message(
            "runc --version",
            &output,
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.lines().any(|line| line.trim() == "spec: 1.3.0") {
        return Err(EngineError::internal(
            "runc does not report exact OCI Runtime Specification 1.3.0 support",
        ));
    }
    let timeout = budget
        .remaining()
        .map_err(|error| EngineError::internal(format!("{error:#}")))?;
    let output = run_helper(
        supervisor,
        Command::new(&canonical).args(["create", "--help"]),
        timeout,
    )
    .map_err(|error| EngineError::internal(format!("runc create --help failed: {error:#}")))?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout).contains("--pidfd-socket")
    {
        return Err(EngineError::internal(
            "runc does not expose the required create --pidfd-socket capability",
        ));
    }
    Ok(canonical)
}

fn create_private_directory(path: &Path) -> AnyResult<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_exact_config(bundle: &Path, bytes: &[u8]) -> AnyResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(bundle.join("config.json"))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn mount_artifacts(rootfs: &Path, config: &serde_json::Value) -> AnyResult<Vec<PathBuf>> {
    let mut artifacts = BTreeSet::new();
    for mount in config
        .get("mounts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let destination = mount
            .get("destination")
            .and_then(serde_json::Value::as_str)
            .context("OCI mount destination is absent")?;
        let relative = safe_container_path(destination)?;
        reject_symlink_ancestor(rootfs, &relative)?;
        let mut current = PathBuf::new();
        for component in relative.components() {
            current.push(component.as_os_str());
            if fs::symlink_metadata(rootfs.join(&current))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                artifacts.insert(current.clone());
            }
        }
    }
    let mut artifacts = artifacts.into_iter().collect::<Vec<_>>();
    artifacts.sort_by_key(|path| path.components().count());
    Ok(artifacts)
}

fn safe_container_path(path: &str) -> AnyResult<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        bail!("mount destination must be absolute");
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => result.push(value),
            _ => bail!("mount destination is not normalized"),
        }
    }
    if result.as_os_str().is_empty() {
        bail!("mounting over / is unsupported");
    }
    Ok(result)
}

fn reject_symlink_ancestor(rootfs: &Path, relative: &Path) -> AnyResult<()> {
    let mut current = rootfs.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("mount destination traverses symlink {}", current.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Debug)]
struct HelperOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct HelperRunError {
    message: String,
    supervisor_reaped: bool,
}

impl std::fmt::Display for HelperRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HelperRunError {}

fn helper_run_error(error: impl std::fmt::Display, supervisor_reaped: bool) -> HelperRunError {
    HelperRunError {
        message: error.to_string(),
        supervisor_reaped,
    }
}

fn run_helper(
    supervisor: &InvocationSupervisor,
    command: &mut Command,
    timeout: Duration,
) -> Result<HelperOutput, HelperRunError> {
    let deadline = checked_deadline(Instant::now(), timeout, "helper deadline")
        .map_err(|error| helper_run_error(format!("{error:#}"), true))?;
    run_helper_until(supervisor, command, deadline, None)
}

fn run_helper_until(
    supervisor: &InvocationSupervisor,
    command: &mut Command,
    deadline: Instant,
    cancellation: Option<&CancellationToken>,
) -> Result<HelperOutput, HelperRunError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(helper_run_error(
            "helper deadline exceeded before spawn",
            supervisor.lifecycle() == SupervisorLifecycle::Reaped,
        ));
    }
    let remaining = deadline.saturating_duration_since(now);
    if remaining < MIN_HELPER_START_REMAINING {
        return Err(helper_run_error(
            "helper deadline has insufficient time to reserve one work poll and one reap poll before spawn",
            supervisor.lifecycle() == SupervisorLifecycle::Reaped,
        ));
    }
    let reap_reserve = SUPERVISOR_REAP_LIMIT.min(remaining / 2);
    let work_deadline = deadline.checked_sub(reap_reserve).unwrap_or(now);
    if now >= work_deadline {
        return Err(helper_run_error(
            "helper deadline has no remaining bounded work interval before its reap reserve",
            supervisor.lifecycle() == SupervisorLifecycle::Reaped,
        ));
    }
    let mut helper = match RunningHelper::spawn(supervisor, command) {
        Ok(helper) => helper,
        Err(error) => {
            return Err(helper_run_error(
                format!("{error:#}"),
                supervisor.lifecycle() == SupervisorLifecycle::Reaped,
            ));
        }
    };
    loop {
        match helper.try_finish_until(deadline) {
            Ok(Some(output)) => return Ok(output),
            Ok(None) => {}
            Err(error) => {
                return match helper.terminate_until(deadline) {
                    Ok(()) => Err(helper_run_error(format!("{error:#}"), true)),
                    Err(cleanup) => Err(helper_run_error(
                        format!(
                            "helper supervision failed: {error:#}; helper termination failed: {cleanup:#}"
                        ),
                        false,
                    )),
                };
            }
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return match helper.terminate_until(deadline) {
                Ok(()) => Err(helper_run_error("helper interrupted by cancellation", true)),
                Err(error) => Err(helper_run_error(
                    format!("failed to terminate cancelled helper: {error:#}"),
                    false,
                )),
            };
        }
        if Instant::now() >= work_deadline {
            return match helper.terminate_until(deadline) {
                Ok(()) => Err(helper_run_error("helper deadline exceeded", true)),
                Err(error) => Err(helper_run_error(
                    format!("failed to terminate timed-out helper: {error:#}"),
                    false,
                )),
            };
        }
        thread::sleep(POLL_INTERVAL.min(work_deadline.saturating_duration_since(Instant::now())));
    }
}

struct RunningHelper {
    supervisor: InvocationSupervisor,
    token: SupervisorToken,
    stdout: File,
    stderr: File,
    reaped: bool,
}

impl RunningHelper {
    fn spawn(supervisor: &InvocationSupervisor, command: &mut Command) -> AnyResult<Self> {
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .process_group(0);
        Ok(Self {
            token: supervisor.spawn(command).map_err(anyhow::Error::from)?,
            supervisor: supervisor.clone(),
            stdout,
            stderr,
            reaped: false,
        })
    }

    fn try_finish(&mut self) -> AnyResult<Option<HelperOutput>> {
        let deadline = checked_deadline(
            Instant::now(),
            SUPERVISOR_REAP_LIMIT,
            "helper supervision deadline",
        )?;
        self.try_finish_until(deadline)
    }

    fn try_finish_until(&mut self, deadline: Instant) -> AnyResult<Option<HelperOutput>> {
        if Instant::now() >= deadline {
            bail!("helper supervision deadline exceeded");
        }
        let stdout_size = match self.stdout.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.terminate_until(deadline)
                    .context("failed to terminate helper after stdout metadata error")?;
                return Err(error.into());
            }
        };
        let stderr_size = match self.stderr.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.terminate_until(deadline)
                    .context("failed to terminate helper after stderr metadata error")?;
                return Err(error.into());
            }
        };
        let oversized = if stdout_size > HELPER_OUTPUT_LIMIT as u64 {
            Some("stdout")
        } else if stderr_size > HELPER_OUTPUT_LIMIT as u64 {
            Some("stderr")
        } else {
            None
        };
        if let Some(stream) = oversized {
            self.terminate_until(deadline)
                .context("failed to terminate oversized-output helper")?;
            bail!("helper {stream} output exceeds {HELPER_OUTPUT_LIMIT} bytes");
        }
        let status = match self.supervisor.try_wait(self.token) {
            Ok(Some(status)) => {
                self.reaped = true;
                self.supervisor.release_reaped(self.token)?;
                status
            }
            Ok(None) => return Ok(None),
            Err(error) => {
                self.terminate_until(deadline)
                    .context("failed to terminate helper after wait error")?;
                return Err(error.into());
            }
        };
        Ok(Some(HelperOutput {
            status,
            stdout: read_helper_output(&mut self.stdout, "stdout")?,
            stderr: read_helper_output(&mut self.stderr, "stderr")?,
        }))
    }

    fn terminate(&mut self) -> AnyResult<()> {
        let deadline = checked_deadline(
            Instant::now(),
            SUPERVISOR_REAP_LIMIT,
            "helper reap deadline",
        )?;
        self.terminate_until(deadline)
    }

    fn terminate_until(&mut self, deadline: Instant) -> AnyResult<()> {
        loop {
            if Instant::now() >= deadline {
                bail!("helper reap deadline exceeded");
            }
            if self.supervisor.try_wait(self.token)?.is_some() {
                self.reaped = true;
                self.supervisor.release_reaped(self.token)?;
                return Ok(());
            }
            self.supervisor.progress_kill(self.token)?;
            if Instant::now() >= deadline {
                bail!(
                    "helper process group was killed but its leader was not reaped before the confirmation deadline"
                );
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn request_terminate(&mut self) -> AnyResult<()> {
        if self.supervisor.try_wait(self.token)?.is_some() {
            self.reaped = true;
            self.supervisor.release_reaped(self.token)?;
            return Ok(());
        }
        self.supervisor.progress_kill(self.token).map(|_| ())
    }

    fn poll_reaped(&mut self) -> AnyResult<()> {
        if !self.reaped && self.supervisor.try_wait(self.token)?.is_some() {
            self.reaped = true;
            self.supervisor.release_reaped(self.token)?;
        } else if !self.reaped {
            self.supervisor.progress_kill(self.token)?;
        }
        Ok(())
    }

    const fn is_reaped(&self) -> bool {
        self.reaped
    }
}

impl Drop for RunningHelper {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.request_terminate();
        }
    }
}

fn terminate_child(
    supervisor: &InvocationSupervisor,
    child: &mut Option<SupervisorToken>,
    timeout: Duration,
) -> AnyResult<()> {
    let Some(token) = *child else {
        return Ok(());
    };
    if let Ok(Some(_)) = supervisor.try_wait(token) {
        supervisor.release_reaped(token)?;
        *child = None;
        return Ok(());
    }
    let deadline = checked_deadline(Instant::now(), timeout, "child reap deadline")?;
    loop {
        if supervisor.try_wait(token)?.is_some() {
            supervisor.release_reaped(token)?;
            *child = None;
            return Ok(());
        }
        supervisor.progress_kill(token)?;
        if Instant::now() >= deadline {
            bail!(
                "child process group was killed but its leader was not reaped before the confirmation deadline"
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn read_helper_output(file: &mut File, stream: &str) -> AnyResult<Vec<u8>> {
    let size = file.metadata()?.len();
    if size > HELPER_OUTPUT_LIMIT as u64 {
        bail!("helper {stream} output exceeds {HELPER_OUTPUT_LIMIT} bytes");
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(HELPER_OUTPUT_LIMIT));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn runc_command(runc: &Path, root: &Path) -> Command {
    let mut command = Command::new(runc);
    command.arg("--root").arg(root);
    command
}

fn helper_message(operation: &str, output: &HelperOutput) -> String {
    format!(
        "{operation} failed with {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn create_failure_message(
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
    log_path: &Path,
) -> String {
    let log = read_bounded_diagnostic(log_path)
        .unwrap_or_else(|error| format!("<unavailable bounded runc log: {error}>").into_bytes());
    format!(
        "runc create failed with {status}; diagnostic stdout: {}; diagnostic stderr: {}; runc log: {}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr),
        String::from_utf8_lossy(&log)
    )
}

fn read_bounded_diagnostic(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(HELPER_OUTPUT_LIMIT.min(4096));
    match File::open(path) {
        Ok(file) => file
            .take(u64::try_from(HELPER_OUTPUT_LIMIT + 1).expect("diagnostic limit fits u64"))
            .read_to_end(&mut bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(bytes),
        Err(error) => return Err(error),
    };
    if bytes.len() > HELPER_OUTPUT_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runc diagnostic log exceeds 1 MiB",
        ));
    }
    Ok(bytes)
}

fn operation_error(
    stage: OperationStage,
    message: impl Into<String>,
    code: Option<i64>,
) -> OperationError {
    OperationError::new(now(), stage, message, code).expect("operation messages are non-empty")
}

fn output_internal(error: impl std::fmt::Display) -> EngineError {
    EngineError::internal(format!(
        "failed to construct trustworthy RunOutput: {error}"
    ))
}

fn checked_deadline(start: Instant, duration: Duration, operation: &str) -> AnyResult<Instant> {
    start
        .checked_add(duration)
        .with_context(|| format!("{operation} exceeds the monotonic clock range"))
}

fn execution_expired(start: Option<Instant>, limit: Option<Duration>) -> bool {
    start
        .zip(limit)
        .is_some_and(|(start, limit)| start.elapsed() >= limit)
}

fn now() -> DateTime<FixedOffset> {
    Local::now().fixed_offset()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::fs::File;
    use std::io::Read;
    use std::num::NonZeroU64;
    use std::sync::Mutex;

    use oci_spec::image::{Descriptor, Digest, MediaType};
    use run_protocol::{ImageDescriptor, RuntimeConfig};
    use serde_json::json;
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::{ContentError, ContentErrorKind, OciContent};

    struct UnavailableStore;

    impl OciContentStore for UnavailableStore {
        fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
            Err(ContentError::new(
                ContentErrorKind::Unavailable,
                "test content is absent",
            ))
        }

        fn publish(
            &self,
            _descriptor: &Descriptor,
            _content: &mut dyn Read,
        ) -> Result<(), ContentError> {
            Err(ContentError::new(
                ContentErrorKind::Rejected,
                "test store is read-only",
            ))
        }
    }

    #[derive(Default)]
    struct PublishCountingStore {
        publishes: std::sync::atomic::AtomicUsize,
    }

    impl OciContentStore for PublishCountingStore {
        fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
            Err(ContentError::new(
                ContentErrorKind::Unavailable,
                "test content is absent",
            ))
        }

        fn publish(
            &self,
            _descriptor: &Descriptor,
            _content: &mut dyn Read,
        ) -> Result<(), ContentError> {
            self.publishes.fetch_add(1, Ordering::Relaxed);
            Err(ContentError::new(
                ContentErrorKind::Rejected,
                "test publish must not be reached",
            ))
        }
    }

    fn fake_runc_with_create_markers(
        create_delay: Duration,
    ) -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let workspace = tempfile::tempdir().expect("fake runc workspace");
        let created = workspace.path().join("created");
        let deleted = workspace.path().join("deleted");
        let runc = workspace.path().join("runc");
        fs::write(
            &runc,
            format!(
                "#!/bin/sh\noperation=\nfor argument in \"$@\"; do\n  case \"$argument\" in create|delete) operation=\"$argument\";; esac\ndone\ncase \"$operation\" in\n  create) sleep {}; printf created > '{}'; sleep 30;;\n  delete) printf deleted > '{}'; rm -f '{}';;\nesac\n",
                create_delay.as_secs_f64(),
                created.display(),
                deleted.display(),
                created.display(),
            ),
        )
        .expect("write fake runc");
        fs::set_permissions(&runc, fs::Permissions::from_mode(0o700))
            .expect("executable fake runc");
        (workspace, runc, created, deleted)
    }

    fn empty_prepared_invocation(
        workspace: TempDir,
        runc: PathBuf,
        supervisor: InvocationSupervisor,
        runtime_id: &str,
    ) -> PreparedInvocation {
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("private invocation workspace");
        let runtime_root = workspace.path().join("runtime");
        create_private_directory(&runtime_root).expect("runtime root");
        let bundle = workspace.path().join("bundle");
        create_private_directory(&bundle).expect("bundle");
        let rootfs = Rootfs::materialize_in(
            &bundle,
            &[],
            RootfsLimits::default(),
            |_descriptor| -> AnyResult<std::io::Cursor<Vec<u8>>> {
                unreachable!("empty layer set does not open content")
            },
        )
        .expect("empty rootfs");
        let program = PreparedProgram {
            bundle: bundle.clone(),
            runtime_id: runtime_id.to_owned(),
            pidfd_path: bundle.join("pidfd.sock"),
            runc_log_path: bundle.join("runc.log"),
            expected_cgroup_path: Path::new("/sys/fs/cgroup").join(runtime_id),
            rootfs,
            parent: test_image(),
            artifacts: Vec::new(),
        };
        PreparedInvocation {
            workspace: Some(workspace),
            runtime_root,
            runc,
            programs: BTreeMap::from([(ProgramId::primary(), program)]),
            supervisor,
        }
    }

    #[test]
    fn capability_limits_fail_before_host_or_content_probe() {
        let engine = test_engine();
        let programs = (0..=MAX_PROGRAMS)
            .map(|index| (ProgramId::new(format!("p{index}")), test_program()))
            .chain(std::iter::once((ProgramId::primary(), test_program())))
            .collect();
        let input = RunInput::new(programs, None, Network::Isolated).expect("input");
        let error = engine
            .run(input, CancellationToken::new())
            .expect_err("Program cap");
        assert_eq!(
            error.path().map(ToString::to_string).as_deref(),
            Some("programs")
        );

        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), test_program())]),
            NonZeroU64::new(
                u64::try_from(MAX_EXECUTION_TIMEOUT.as_millis()).expect("milliseconds") + 1,
            ),
            Network::Isolated,
        )
        .expect("input");
        let error = engine
            .run(input, CancellationToken::new())
            .expect_err("timeout cap");
        assert_eq!(
            error.path().map(ToString::to_string).as_deref(),
            Some("execution_timeout_ms")
        );
    }

    #[test]
    fn isolated_profile_requires_one_new_network_namespace() {
        let id = ProgramId::primary();
        validate_runtime(&id, &test_program()).expect("new private network namespace");
        let runtime = RuntimeConfig::parse(
            br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"terminal":false,"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0},"noNewPrivileges":true,"capabilities":{"bounding":[],"effective":[],"inheritable":[],"permitted":[],"ambient":[]}},"linux":{"namespaces":[{"type":"pid"},{"type":"network","path":"/proc/1/ns/net"},{"type":"ipc"},{"type":"uts"},{"type":"mount"},{"type":"cgroup"}]}}"#.to_vec(),
        )
        .expect("runtime");
        let program = ProgramInput::new(test_image(), runtime, Vec::new()).expect("program");
        let error = validate_runtime(&id, &program).expect_err("existing namespace");
        assert!(
            error
                .path()
                .expect("path")
                .to_string()
                .ends_with("linux.namespaces[1].path"),
            "{error:?}"
        );
    }

    #[test]
    fn isolated_profile_rejects_cross_boundary_runtime_features_at_exact_paths() {
        let cases = [
            (
                "hooks",
                json!({"hooks": {"prestart": [{"path": "/bin/true"}]}}),
                "hooks",
            ),
            (
                "bind",
                json!({"mounts": [{"destination": "/host", "type": "bind", "source": "/"}]}),
                "mounts[0]",
            ),
            (
                "namespace",
                json!({"linux": {"namespaces": [{"type": "network"}, {"type": "pid", "path": "/proc/1/ns/pid"}]}}),
                "linux.namespaces[1].path",
            ),
            (
                "capability",
                json!({"process": {"capabilities": {
                    "bounding": [], "effective": ["CAP_NET_ADMIN"], "inheritable": [],
                    "permitted": [], "ambient": []
                }}}),
                "process.capabilities.effective",
            ),
            (
                "privilege gain",
                json!({"process": {"noNewPrivileges": false}}),
                "process.noNewPrivileges",
            ),
            (
                "device",
                json!({"linux": {"devices": [{"path": "/dev/kmsg", "type": "c", "major": 1, "minor": 11}], "namespaces": [{"type": "network"}]}}),
                "linux.devices",
            ),
            (
                "seccomp listener",
                json!({"linux": {"seccomp": {"defaultAction": "SCMP_ACT_ALLOW", "listenerPath": "/run/notify.sock"}, "namespaces": [{"type": "network"}]}}),
                "linux.seccomp.listenerPath",
            ),
            (
                "rootfs propagation",
                json!({"linux": {"rootfsPropagation": "shared", "namespaces": [{"type": "network"}]}}),
                "linux.rootfsPropagation",
            ),
            (
                "caller cgroup",
                json!({"linux": {"cgroupsPath": "/shared", "namespaces": [{"type": "network"}]}}),
                "linux.cgroupsPath",
            ),
        ];
        for (label, addition, suffix) in cases {
            let mut value = json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "terminal": false,
                    "args": ["/bin/true"],
                    "cwd": "/",
                    "user": {"uid": 0, "gid": 0},
                    "noNewPrivileges": true,
                    "capabilities": {
                        "bounding": [], "effective": [], "inheritable": [],
                        "permitted": [], "ambient": []
                    }
                },
                "linux": {"namespaces": [
                    {"type": "pid"}, {"type": "network"}, {"type": "ipc"},
                    {"type": "uts"}, {"type": "mount"}, {"type": "cgroup"}
                ]}
            });
            merge_json(&mut value, &addition);
            let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime JSON"))
                .expect("runtime config");
            let program = ProgramInput::new(test_image(), runtime, Vec::new()).expect("program");
            let error = validate_runtime(&ProgramId::primary(), &program)
                .expect_err("cross-boundary feature unexpectedly accepted");
            assert!(
                error
                    .path()
                    .expect("unsupported path")
                    .to_string()
                    .ends_with(suffix),
                "{label}: {error}"
            );
        }
    }

    fn merge_json(target: &mut serde_json::Value, addition: &serde_json::Value) {
        let target = target.as_object_mut().expect("target object");
        for (key, value) in addition.as_object().expect("addition object") {
            if let (Some(existing), Some(fields)) = (target.get_mut(key), value.as_object())
                && let Some(existing) = existing.as_object_mut()
            {
                for (field, value) in fields {
                    existing.insert(field.clone(), value.clone());
                }
                continue;
            }
            target.insert(key.clone(), value.clone());
        }
    }

    #[test]
    fn helper_output_and_container_paths_are_bounded() {
        let pid_file = tempfile::NamedTempFile::new().expect("pid file");
        let supervisor = InvocationSupervisor::new();
        let error = run_helper(
            &supervisor,
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "printf %s \"$$\" > \"$1\"; head -c {} /dev/zero; sleep 30",
                    HELPER_OUTPUT_LIMIT + 1
                ))
                .arg("runlab-helper-test")
                .arg(pid_file.path()),
            Duration::from_secs(5),
        )
        .expect_err("helper output cap");
        assert!(error.to_string().contains("output exceeds"), "{error:#}");
        let pid = fs::read_to_string(pid_file.path())
            .expect("helper pid")
            .parse::<i32>()
            .expect("numeric helper pid");
        let pid = Pid::from_raw(pid).expect("positive helper pid");
        assert_eq!(
            rustix::process::test_kill_process(pid).expect_err("helper must be gone"),
            rustix::io::Errno::SRCH
        );
        assert!(safe_container_path("/a/b").is_ok());
        assert!(safe_container_path("/a/../b").is_err());
        assert!(safe_container_path("relative").is_err());
    }

    #[test]
    fn operation_budget_and_helper_deadline_are_explicit() {
        let budget = OperationBudget::new(Duration::ZERO, "test preparation").expect("deadline");
        let store = BudgetedStore::new(Arc::new(UnavailableStore), budget);
        let Err(error) = store.open(test_image().as_oci()) else {
            panic!("expired budget reached the underlying store");
        };
        assert_eq!(error.kind(), ContentErrorKind::Internal);
        assert!(error.reason().contains("deadline exceeded"));

        let began = Instant::now();
        let supervisor = InvocationSupervisor::new();
        let error = run_helper(
            &supervisor,
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_millis(20),
        )
        .expect_err("helper timeout");
        assert!(error.to_string().contains("deadline"));
        assert!(began.elapsed() < Duration::from_secs(2));

        let marker = tempfile::NamedTempFile::new().expect("marker path");
        let marker_path = marker.path().to_path_buf();
        drop(marker);
        let error = run_helper_until(
            &supervisor,
            Command::new("/bin/sh")
                .arg("-c")
                .arg("printf spawned > \"$1\"")
                .arg("tiny-deadline")
                .arg(&marker_path),
            Instant::now() + POLL_INTERVAL,
            None,
        )
        .expect_err("tiny remaining interval must reject before spawn");
        assert!(error.to_string().contains("insufficient time"));
        assert!(!marker_path.exists(), "tiny-deadline helper was spawned");
        assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);

        assert!(matches!(
            process_from_raw_wait_status(7 << 8),
            ProcessResult::Exited { code: 7, .. }
        ));
        assert!(matches!(
            process_from_raw_wait_status(9),
            ProcessResult::Signaled { signal, .. } if signal.get() == 9
        ));
        assert!(matches!(
            process_from_raw_wait_status(0x7f),
            ProcessResult::Unknown { .. }
        ));
        let control = proc_connector_control_message(42, PROC_CN_MCAST_LISTEN);
        assert_eq!(read_u32(&control, 0).expect("netlink length"), 40);
        assert_eq!(read_u32(&control, 12).expect("netlink port"), 42);
        assert_eq!(
            read_u32(&control, 16).expect("connector index"),
            CN_IDX_PROC
        );
        assert_eq!(read_u32(&control, 36).expect("listen operation"), 1);
    }

    #[test]
    fn invocation_supervisor_retries_transient_kill_and_wait_failures() {
        let supervisor = InvocationSupervisor::new();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor.spawn(&mut command).expect("registered child");
        supervisor.inject_faults(1, 0, 1, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_secs(2))
            .expect("final guard retries transient failures");
        assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
    }

    #[test]
    fn invocation_supervisor_distinguishes_kill_delivery_from_unproved_termination() {
        let marker = tempfile::NamedTempFile::new().expect("marker path");
        let marker_path = marker.path().to_path_buf();
        drop(marker);
        let supervisor = InvocationSupervisor::new();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep .2; printf late > \"$1\"")
            .arg("supervisor-child")
            .arg(&marker_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor.spawn(&mut command).expect("registered child");
        supervisor.inject_faults(usize::MAX, 0, 0, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_millis(100))
            .expect_err("wait evidence remains unavailable");
        assert!(matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::KillDelivered { children: 1 }
        ));
        thread::sleep(Duration::from_millis(300));
        assert!(!marker_path.exists(), "proved KILL allowed a late marker");
        supervisor.inject_faults(0, 0, 0, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_secs(2))
            .expect("reap after fault removal");

        let supervisor = InvocationSupervisor::new();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor.spawn(&mut command).expect("registered child");
        supervisor.inject_faults(usize::MAX, 0, usize::MAX, 0);
        let failure = supervisor
            .finalize(Instant::now() + Duration::from_millis(30))
            .expect_err("termination cannot be proved");
        assert!(matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::TerminationUnproven {
                kill_delivered: 0,
                unproved: 1
            }
        ));
        let workspace = tempfile::tempdir().expect("workspace");
        let preserved = workspace.path().to_path_buf();
        let error = preserve_workspace_after_supervisor_failure(workspace, &failure);
        assert!(error.to_string().contains("preserved workspace"));
        assert!(preserved.is_dir());
        supervisor.inject_faults(0, 0, 0, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_secs(2))
            .expect("supervisor still owns and reaps child after fault removal");
        fs::remove_dir(&preserved).expect("remove empty preserved test workspace");
    }

    #[test]
    fn pidfd_open_failure_is_registered_and_bounded_by_run_owner() {
        let supervisor = InvocationSupervisor::new();
        supervisor.inject_faults(0, 1, 0, 0);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let began = Instant::now();
        let spawn_error = supervisor
            .spawn(&mut command)
            .expect_err("pidfd_open injection");
        assert!(spawn_error.to_string().contains("pidfd_open"));
        assert!(supervisor.only_child_termination_started());
        assert!(matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::TerminationUnproven {
                kill_delivered: 0,
                unproved: 1
            }
        ));
        supervisor
            .finalize(Instant::now() + Duration::from_millis(100))
            .expect("unreaped pid permits bounded process-group cleanup");
        assert!(began.elapsed() < Duration::from_secs(1));
        assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
    }

    #[test]
    fn preflight_pidfd_open_failure_uses_preparation_deadline_not_drop() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("private workspace");
        let runc = workspace.path().join("runc");
        fs::write(&runc, "#!/bin/sh\nsleep 30\n").expect("fake runc");
        fs::set_permissions(&runc, fs::Permissions::from_mode(0o700)).expect("executable runc");
        let engine = NativeEngine::new(
            Arc::new(UnavailableStore),
            workspace.path(),
            runc,
            OperationTimeouts::default()
                .with_preparation(Duration::from_millis(100))
                .expect("preparation timeout"),
        );
        let supervisor = InvocationSupervisor::new();
        supervisor.inject_faults(0, 1, 0, 0);
        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), test_program())]),
            None,
            Network::Isolated,
        )
        .expect("input");
        let began = Instant::now();
        let error = engine
            .run_supervised(&input, &CancellationToken::new(), &supervisor)
            .expect_err("preflight pidfd failure");
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("pidfd_open"));
        assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
        assert_eq!(
            fs::read_dir(workspace.path())
                .expect("workspace entries")
                .count(),
            1,
            "preflight must not create an invocation workspace"
        );
    }

    #[test]
    fn group_kill_failure_keeps_zombie_identity_until_descendants_are_killed() {
        let marker = tempfile::NamedTempFile::new().expect("marker path");
        let marker_path = marker.path().to_path_buf();
        drop(marker);
        let supervisor = InvocationSupervisor::new();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("(sleep .4; printf late > \"$1\") & wait")
            .arg("supervisor-descendant")
            .arg(&marker_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor
            .spawn(&mut command)
            .expect("registered process group");
        supervisor.inject_faults(0, 0, 0, usize::MAX);
        supervisor
            .finalize(Instant::now() + Duration::from_millis(100))
            .expect_err("group KILL remains unproved");
        let (exit_observed, leader_kill, group_kill, leader_reaped) = supervisor.only_child_facts();
        assert!(exit_observed && leader_kill);
        assert!(!group_kill && !leader_reaped);
        assert!(matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::TerminationUnproven {
                kill_delivered: 0,
                unproved: 1
            }
        ));
        supervisor.inject_faults(0, 0, 0, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_secs(2))
            .expect("retry proves group KILL and reaps leader");
        thread::sleep(Duration::from_millis(500));
        assert!(
            !marker_path.exists(),
            "descendant wrote after proved group KILL"
        );
    }

    #[test]
    fn pidfd_open_failure_keeps_fast_exit_unreaped_until_group_cleanup() {
        let marker = tempfile::NamedTempFile::new().expect("marker path");
        let marker_path = marker.path().to_path_buf();
        drop(marker);
        let supervisor = InvocationSupervisor::new();
        supervisor.inject_faults(0, 1, 0, usize::MAX);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("(sleep .4; printf late > \"$1\") & exit 0")
            .arg("supervisor-no-pidfd-descendant")
            .arg(&marker_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor
            .spawn(&mut command)
            .expect_err("production-path pidfd_open fault");
        assert!(supervisor.only_child_termination_started());
        supervisor
            .finalize(Instant::now() + Duration::from_millis(100))
            .expect_err("persistent group failure must remain unproved");
        let (exit_observed, leader_kill, group_kill, leader_reaped) = supervisor.only_child_facts();
        assert!(
            exit_observed,
            "fast leader exit was not observed with WNOWAIT"
        );
        assert!(!leader_kill && !group_kill && !leader_reaped);
        assert!(matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::TerminationUnproven {
                kill_delivered: 0,
                unproved: 1
            }
        ));
        supervisor.inject_faults(0, 0, 0, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_secs(2))
            .expect("retained zombie identity permits a later group cleanup and reap");
        thread::sleep(Duration::from_millis(500));
        assert!(
            !marker_path.exists(),
            "descendant wrote after no-pidfd process-group cleanup"
        );
    }

    #[test]
    fn create_attach_failure_is_attempted_and_deleted_after_supervisor_is_safe() {
        let (_runc_workspace, runc, created, deleted) =
            fake_runc_with_create_markers(Duration::ZERO);
        let supervisor = InvocationSupervisor::new();
        let workspace = tempfile::tempdir().expect("invocation workspace");
        let mut prepared = empty_prepared_invocation(
            workspace,
            runc.clone(),
            supervisor.clone(),
            "run-engine-attach-fault-delete",
        );
        supervisor.inject_faults(0, 1, 0, 0);
        let mut run = start_program(
            &supervisor,
            &runc,
            &prepared.runtime_root,
            &prepared.programs[&ProgramId::primary()],
            &test_program(),
            OperationTimeouts::default(),
            &CancellationToken::new(),
            None,
            None,
            &mut BTreeMap::new(),
        );
        assert_eq!(run.create.status(), OperationStatus::Unknown);
        assert_eq!(run.start.status(), OperationStatus::NotAttempted);
        assert!(run.runtime_attempted && !run.writer_stopped);
        assert!(run.runtime_coordinates.is_none());
        assert!(run.process.is_none());
        assert!(run.create.errors().any(|error| {
            error
                .message()
                .contains("attachment failed after registration")
        }));
        let marker_deadline = Instant::now() + Duration::from_secs(1);
        while !created.exists() && Instant::now() < marker_deadline {
            thread::sleep(POLL_INTERVAL);
        }
        assert!(created.exists(), "fake create side effect did not occur");

        let mut outcomes = BTreeMap::from([(ProgramId::primary(), run)]);
        finalize_children(&mut outcomes, OperationTimeouts::default());
        supervisor
            .finalize(Instant::now() + Duration::from_secs(1))
            .expect("create supervisor safe before runtime delete");
        run = outcomes.remove(&ProgramId::primary()).expect("run");
        cleanup_runtime(
            &runc,
            &prepared.runtime_root,
            &prepared.programs[&ProgramId::primary()],
            &mut run,
            OperationTimeouts::default(),
            Instant::now() + Duration::from_secs(2),
        );
        supervisor
            .finalize(Instant::now() + Duration::from_secs(1))
            .expect("delete helper reaped");
        assert!(deleted.exists(), "safe runtime delete was not attempted");
        assert!(!created.exists(), "fake runtime object survived delete");
        assert!(
            run.process.is_none(),
            "create failure must remain NeverStarted"
        );
        assert!(run.writer_stopped);
        if let Some(error) = cleanup_invocation(
            &mut prepared,
            true,
            OperationBudget::new(Duration::from_secs(2), "test cleanup").expect("budget"),
        ) {
            panic!("invocation cleanup failed: {error:?}");
        }
    }

    #[test]
    fn unproved_late_create_cannot_race_runtime_cleanup_or_capture() {
        let (_runc_workspace, runc, created, deleted) =
            fake_runc_with_create_markers(Duration::from_millis(50));
        let supervisor = InvocationSupervisor::new();
        supervisor.inject_faults(0, 1, 0, usize::MAX);
        let workspace = tempfile::tempdir().expect("invocation workspace");
        let workspace_path = workspace.path().to_path_buf();
        let mut prepared = empty_prepared_invocation(
            workspace,
            runc.clone(),
            supervisor.clone(),
            "run-engine-late-create-gate",
        );
        let store = Arc::new(PublishCountingStore::default());
        let timeouts = OperationTimeouts::default()
            .with_cleanup(Duration::from_millis(100))
            .expect("cleanup timeout")
            .with_wait(Duration::from_millis(10))
            .expect("wait timeout")
            .with_forced_stop_confirmation(Duration::from_millis(10))
            .expect("confirmation timeout")
            .with_stream_drain(Duration::from_millis(10))
            .expect("drain timeout");
        let engine = NativeEngine::new(store.clone(), &workspace_path, runc, timeouts);
        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), test_program())]),
            None,
            Network::Isolated,
        )
        .expect("input");
        let error = execute(&engine, &input, &CancellationToken::new(), &mut prepared)
            .expect_err("unproved create supervisor must withhold RunOutput");
        assert!(error.to_string().contains("preserved workspace"), "{error}");
        assert!(
            created.exists(),
            "late fake create side effect was not observed"
        );
        assert!(
            !deleted.exists(),
            "runtime delete crossed an unsafe boundary"
        );
        assert_eq!(
            store.publishes.load(Ordering::Relaxed),
            0,
            "capture published"
        );
        assert!(
            workspace_path.exists(),
            "unsafe workspace was not preserved"
        );

        supervisor.inject_faults(0, 0, 0, 0);
        supervisor
            .finalize(Instant::now() + Duration::from_secs(2))
            .expect("test teardown closes retained supervisor");
        drop(prepared);
        fs::remove_dir_all(&workspace_path).expect("remove preserved test workspace");
    }

    #[test]
    fn kill_delivered_at_cleanup_deadline_preserves_without_drop_tail() {
        let (_runc_workspace, runc, _created, deleted) =
            fake_runc_with_create_markers(Duration::ZERO);
        let supervisor = InvocationSupervisor::new();
        supervisor.inject_faults(usize::MAX, 1, 0, 0);
        let workspace = tempfile::tempdir().expect("invocation workspace");
        let workspace_path = workspace.path().to_path_buf();
        let mut prepared = empty_prepared_invocation(
            workspace,
            runc.clone(),
            supervisor.clone(),
            "run-engine-expired-safe-gate",
        );
        let store = Arc::new(PublishCountingStore::default());
        let timeouts = OperationTimeouts::default()
            .with_cleanup(Duration::from_millis(100))
            .expect("cleanup timeout")
            .with_wait(Duration::from_millis(10))
            .expect("wait timeout")
            .with_forced_stop_confirmation(Duration::from_millis(10))
            .expect("confirmation timeout")
            .with_stream_drain(Duration::from_millis(10))
            .expect("drain timeout");
        let engine = NativeEngine::new(store.clone(), &workspace_path, runc, timeouts);
        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), test_program())]),
            None,
            Network::Isolated,
        )
        .expect("input");
        let began = Instant::now();
        let error = execute(&engine, &input, &CancellationToken::new(), &mut prepared)
            .expect_err("KillDelivered without reap must withhold cleanup and capture");
        assert!(error.to_string().contains("preserved workspace"), "{error}");
        assert!(matches!(
            supervisor.lifecycle(),
            SupervisorLifecycle::KillDelivered { children: 1 }
        ));
        assert!(
            !deleted.exists(),
            "delete spawned after the absolute deadline"
        );
        assert_eq!(
            store.publishes.load(Ordering::Relaxed),
            0,
            "capture published"
        );
        assert!(
            workspace_path.exists(),
            "unsafe workspace was not preserved"
        );
        let raw_pid = supervisor.only_child_pid();
        drop(prepared);
        drop(supervisor);
        assert!(
            began.elapsed() < Duration::from_millis(300),
            "Drop added a hidden bounded-wait tail: {:?}",
            began.elapsed()
        );
        rustix::process::waitpid(Some(raw_pid), rustix::process::WaitOptions::empty())
            .expect("reap already-KILLed test child")
            .expect("already-KILLed child wait status");
        fs::remove_dir_all(&workspace_path).expect("remove preserved test workspace");
    }

    #[test]
    fn runc_state_failures_are_wait_evidence_and_do_not_respawn() {
        for (label, body) in [
            ("nonzero", "printf diagnostic >&2; exit 23"),
            ("invalid-json", "printf '{not-json'; exit 0"),
        ] {
            let workspace = tempfile::tempdir().expect("state wrapper workspace");
            let count = workspace.path().join("count");
            let wrapper = workspace.path().join("runc");
            fs::write(
                &wrapper,
                format!("#!/bin/sh\nprintf x >> '{}'\n{body}\n", count.display()),
            )
            .expect("write state wrapper");
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
                .expect("executable state wrapper");
            let mut run = ProgramRun::unattempted();
            run.runtime_coordinates = Some((
                wrapper,
                workspace.path().to_path_buf(),
                "state-test".to_owned(),
                Duration::from_secs(1),
            ));
            let error = loop {
                match poll_one(&mut run) {
                    Ok(_) => thread::sleep(POLL_INTERVAL),
                    Err(error) => break error,
                }
            };
            assert!(
                error.to_string().contains("runc state")
                    || error.kind() == std::io::ErrorKind::InvalidData,
                "{label}: {error}"
            );
            assert!(run.state_probe_failed, "{label}");
            assert!(run.state_probe.is_none(), "{label}");
            assert!(!poll_one(&mut run).expect("failed probe stays disabled"));
            assert_eq!(fs::read(&count).expect("one invocation"), b"x", "{label}");
        }
    }

    #[test]
    fn stopped_state_waits_for_already_queued_raw_exit_evidence() {
        let (socket, _peer) = UnixStream::pair().expect("socket pair");
        let mut run = ProgramRun::unattempted();
        run.exit_monitor = Some(ProcExitMonitor {
            socket: socket.into(),
            port_id: 0,
            target_pid: 42,
            sequences: BTreeMap::new(),
            subscribed: false,
        });

        run.observe_runtime_stopped(Duration::from_secs(1));
        assert!(run.writer_stopped);
        assert!(run.process.is_none(), "state must not preempt raw evidence");
        run.observe_raw_process_result(process_from_raw_wait_status(7 << 8));
        assert!(matches!(
            run.process,
            Some(ProcessResult::Exited { code: 7, .. })
        ));
        assert!(run.stopped_observation.is_none());
    }

    #[test]
    fn unattempted_output_is_structurally_valid_and_timeouts_are_stable() {
        let engine = test_engine();
        assert_eq!(engine.operation_timeouts(), OperationTimeouts::default());
        let mut run = ProgramRun::unattempted();
        run.output(Availability::unavailable("not captured").expect("reason"))
            .expect("valid output");
    }

    #[test]
    fn image_platform_requirements_are_rejected_at_exact_paths() {
        for (field, value, suffix) in [
            ("variant", json!("v9"), "platform.variant"),
            ("os.version", json!("test-kernel"), "platform.os.version"),
            (
                "os.features",
                json!(["test-feature"]),
                "platform.os.features",
            ),
        ] {
            let store = MemoryStore::default();
            let descriptor = image_with_platform_field(&store, field, value);
            let image = inspect_image(&store, &descriptor).expect("verified image");
            let Err(error) = validate_platform(&ProgramId::primary(), &image) else {
                panic!("unproved platform requirement {field}");
            };
            assert!(
                error
                    .path()
                    .expect("platform path")
                    .to_string()
                    .ends_with(suffix),
                "{field}: {error}"
            );
        }
        if std::env::consts::ARCH == "aarch64" {
            let store = MemoryStore::default();
            let descriptor = image_with_platform_field(&store, "variant", json!("v8"));
            let image = inspect_image(&store, &descriptor).expect("verified arm64/v8 image");
            validate_platform(&ProgramId::primary(), &image)
                .expect("aarch64 execution proves the OCI arm64/v8 baseline");
        }
    }

    #[test]
    #[ignore = "set RUNLAB_NATIVE_E2E_IMAGE and run as root on the runlab Linux VM"]
    #[allow(
        clippy::too_many_lines,
        reason = "one opt-in real-runtime lifecycle shares one imported image while asserting cross-phase evidence"
    )]
    fn real_runc_exercises_native_engine_contract() {
        if std::env::var_os("RUNLAB_NATIVE_E2E").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return;
        }
        assert_eq!(geteuid().as_raw(), 0, "real NativeEngine E2E requires root");
        let archive = PathBuf::from(
            std::env::var_os("RUNLAB_NATIVE_E2E_IMAGE")
                .expect("RUNLAB_NATIVE_E2E_IMAGE must name a docker-save archive"),
        );
        let runc = PathBuf::from(
            std::env::var_os("RUNLAB_NATIVE_E2E_RUNC")
                .unwrap_or_else(|| "/usr/local/bin/runc".into()),
        );
        let store = Arc::new(MemoryStore::default());
        let initial = import_docker_archive(store.as_ref(), &archive);
        let workspace = tempfile::tempdir().expect("workspace");
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("private workspace");
        let engine = Arc::new(NativeEngine::new(
            store.clone(),
            workspace.path(),
            runc,
            OperationTimeouts::default(),
        ));

        let cgroups_before = engine_cgroups();
        let first = engine
            .run(
                e2e_input(
                    &initial,
                    "stdio-delta",
                    "IFS= read -r line; mkdir -p /result /runtime-created; printf 'out:%s' \"$line\"; printf err >&2; printf delta >/result/value; printf persistent >/runtime-created/persistent; cat /proc/self/cgroup >/result/cgroup; test \"$(wc -l </proc/net/route)\" -eq 1; printf ignored >/runtime-created/nested/ephemeral; sleep .2; exit 7",
                    b"hello\n",
                    None,
                ),
                CancellationToken::new(),
            )
            .expect("nonzero workload is a complete RunOutput");
        let program = &first.programs()[&ProgramId::primary()];
        assert_eq!(program.create().status(), OperationStatus::Succeeded);
        assert_eq!(program.start().status(), OperationStatus::Succeeded);
        assert!(matches!(
            program.process(),
            ProcessResult::Exited { code: 7, .. }
        ));
        let interval_started = match first.execution().interval() {
            ExecutionInterval::Entered { started_at, .. } => *started_at,
            ExecutionInterval::NotEntered { .. } => panic!("execution interval was not entered"),
        };
        assert!(
            program
                .create()
                .facts()
                .expect("create facts")
                .completed_at()
                <= interval_started
        );
        assert!(interval_started <= program.start().facts().expect("start facts").started_at());
        assert_eq!(
            program.stdout().facts().expect("stdout facts").bytes(),
            b"out:hello"
        );
        assert_eq!(
            program.stderr().facts().expect("stderr facts").bytes(),
            b"err"
        );
        assert_eq!(
            program
                .stdin()
                .write()
                .facts()
                .expect("stdin facts")
                .bytes_written(),
            6
        );
        let final_image = program.final_environment().value().unwrap_or_else(|| {
            panic!(
                "final image unavailable: {:?}; errors: {:?}",
                program.final_environment().unavailable_reason(),
                program.errors().collect::<Vec<_>>()
            )
        });
        assert_final_delta(store.as_ref(), final_image);
        assert_workspace_empty(workspace.path());
        assert_eq!(engine_cgroups(), cgroups_before, "owned cgroup leaked");

        let exit_zero = engine
            .run(
                e2e_input(&initial, "exit-zero", "sleep .2; exit 0", b"", None),
                CancellationToken::new(),
            )
            .expect("exit zero output");
        assert!(matches!(
            exit_zero.programs()[&ProgramId::primary()].process(),
            ProcessResult::Exited { code: 0, .. }
        ));

        let signaled = engine
            .run(
                e2e_input(
                    &initial,
                    "forced-signal",
                    "trap '' TERM; while :; do sleep 1; done",
                    b"",
                    NonZeroU64::new(100),
                ),
                CancellationToken::new(),
            )
            .expect("signal output");
        assert!(signaled.execution().timed_out());
        assert!(
            matches!(
                signaled.programs()[&ProgramId::primary()].process(),
                ProcessResult::Signaled { signal, .. } if signal.get() == 9
            ),
            "{:?}",
            signaled.programs()[&ProgramId::primary()].process()
        );

        let blocked_stdin = vec![b'x'; 10 * 1024 * 1024];
        let timed_out = engine
            .run(
                e2e_input(
                    &initial,
                    "timeout",
                    "sleep 30",
                    &blocked_stdin,
                    NonZeroU64::new(100),
                ),
                CancellationToken::new(),
            )
            .expect("timeout output");
        assert!(timed_out.execution().timed_out());
        assert!(
            !timed_out.programs()[&ProgramId::primary()]
                .stop_actions()
                .is_empty()
        );
        let timed_out_stdin = timed_out.programs()[&ProgramId::primary()].stdin();
        assert_eq!(timed_out_stdin.write().status(), OperationStatus::Failed);
        assert!(
            timed_out_stdin
                .write()
                .facts()
                .expect("partial stdin facts")
                .bytes_written()
                < u64::try_from(blocked_stdin.len()).expect("stdin length")
        );
        assert_eq!(timed_out_stdin.close().status(), OperationStatus::Succeeded);

        let cancellation = CancellationToken::new();
        let request = cancellation.clone();
        let cancellation_worker = thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            request.cancel();
        });
        let cancellation_output = engine
            .run(
                e2e_input(&initial, "cancel", "sleep 30", &blocked_stdin, None),
                cancellation,
            )
            .expect("cancelled output");
        cancellation_worker.join().expect("cancellation worker");
        assert!(cancellation_output.execution().cancelled());
        assert!(
            !cancellation_output.programs()[&ProgramId::primary()]
                .stop_actions()
                .is_empty()
        );
        assert_eq!(
            cancellation_output.programs()[&ProgramId::primary()]
                .stdin()
                .write()
                .status(),
            OperationStatus::Failed
        );

        let shared_grace = RunInput::new(
            BTreeMap::from([
                (
                    ProgramId::new("dependency"),
                    e2e_program(
                        &initial,
                        "shared-grace-dependency",
                        "trap '' TERM; while :; do sleep 1; done",
                        b"",
                        true,
                    ),
                ),
                (
                    ProgramId::primary(),
                    e2e_program(
                        &initial,
                        "shared-grace-primary",
                        "trap '' TERM; while :; do sleep 1; done",
                        b"",
                        true,
                    ),
                ),
            ]),
            NonZeroU64::new(100),
            Network::Isolated,
        )
        .expect("multi-Program input");
        let shared_grace = engine
            .run(shared_grace, CancellationToken::new())
            .expect("multi-Program timeout output");
        let term_times = shared_grace
            .programs()
            .values()
            .map(|program| {
                program
                    .stop_actions()
                    .iter()
                    .find(|action| action.signal() == StopSignal::Term)
                    .expect("TERM attempt")
                    .attempted_at()
            })
            .collect::<Vec<_>>();
        assert_eq!(term_times.len(), 2);
        assert!(
            (term_times[1] - term_times[0])
                .num_milliseconds()
                .unsigned_abs()
                < 500,
            "TERM attempts did not share one concurrent grace boundary: {term_times:?}"
        );
        assert!(shared_grace.programs().values().all(|program| {
            program
                .stop_actions()
                .iter()
                .any(|action| action.signal() == StopSignal::Kill)
        }));

        let large_output = RunInput::new(
            BTreeMap::from([
                (
                    ProgramId::new("dependency"),
                    e2e_program(
                        &initial,
                        "large-output-dependency",
                        "dd if=/dev/zero bs=1048576 count=2 2>/dev/null; trap '' TERM; while :; do sleep 1; done",
                        b"",
                        true,
                    ),
                ),
                (
                    ProgramId::primary(),
                    e2e_program(
                        &initial,
                        "large-output-primary",
                        "trap '' TERM; while :; do sleep 1; done",
                        b"",
                        true,
                    ),
                ),
            ]),
            NonZeroU64::new(100),
            Network::Isolated,
        )
        .expect("large-output multi-Program input");
        let large_output = engine
            .run(large_output, CancellationToken::new())
            .expect("large-output multi-Program timeout");
        assert_eq!(
            large_output.programs()[&ProgramId::new("dependency")]
                .stdout()
                .facts()
                .expect("dependency stdout")
                .bytes()
                .len(),
            2 * 1024 * 1024
        );

        let identical = e2e_input_uncgrouped(
            &initial,
            "identical-concurrent",
            "sleep .2; exit 0",
            b"",
            None,
        );
        let second_input = identical.clone();
        let first_engine = Arc::clone(&engine);
        let first_worker =
            thread::spawn(move || first_engine.run(identical, CancellationToken::new()));
        let second_engine = Arc::clone(&engine);
        let second_worker =
            thread::spawn(move || second_engine.run(second_input, CancellationToken::new()));
        first_worker
            .join()
            .expect("first worker")
            .expect("first concurrent Run");
        second_worker
            .join()
            .expect("second worker")
            .expect("second concurrent Run");

        let (_create_wrapper_workspace, create_wrapper) =
            delayed_runc_wrapper(&engine.runc_executable, "create");
        let create_deadline_engine = NativeEngine::new(
            store.clone(),
            workspace.path(),
            create_wrapper,
            OperationTimeouts::default()
                .with_create(Duration::from_millis(10))
                .expect("minimum create timeout"),
        );
        let create_deadline = create_deadline_engine
            .run(
                e2e_input(&initial, "create-deadline", "exit 0", b"", None),
                CancellationToken::new(),
            )
            .expect("create timeout is structured output");
        let create_deadline = &create_deadline.programs()[&ProgramId::primary()];
        assert_eq!(create_deadline.create().status(), OperationStatus::Unknown);
        assert_eq!(
            create_deadline.start().status(),
            OperationStatus::NotAttempted
        );
        assert!(
            create_deadline
                .create()
                .errors()
                .any(|error| error.message().contains("create deadline exceeded"))
        );

        let (_start_wrapper_workspace, start_wrapper) =
            delayed_runc_wrapper(&engine.runc_executable, "start");
        let start_deadline_engine = NativeEngine::new(
            store.clone(),
            workspace.path(),
            start_wrapper,
            OperationTimeouts::default()
                .with_start(Duration::from_millis(10))
                .expect("minimum start timeout"),
        );
        let start_deadline = start_deadline_engine
            .run(
                e2e_input(&initial, "start-deadline", "sleep 1", b"", None),
                CancellationToken::new(),
            )
            .expect("start timeout is structured output");
        let start_program = &start_deadline.programs()[&ProgramId::primary()];
        assert_eq!(start_program.create().status(), OperationStatus::Succeeded);
        assert_eq!(start_program.start().status(), OperationStatus::Unknown);
        assert!(start_deadline.execution().interval().was_entered());
        assert!(
            start_program
                .start()
                .errors()
                .any(|error| error.message().contains("start deadline exceeded"))
        );

        let create_failure = engine
            .run(
                e2e_input_with_invalid_rlimit(&initial, "create-failure", "exit 0"),
                CancellationToken::new(),
            )
            .expect("create failure is structured output");
        let create_failure = &create_failure.programs()[&ProgramId::primary()];
        assert_eq!(create_failure.create().status(), OperationStatus::Failed);
        assert_eq!(
            create_failure.start().status(),
            OperationStatus::NotAttempted
        );
        assert_eq!(
            create_failure.stdout().status(),
            OperationStatus::NotAttempted
        );
        assert_eq!(
            create_failure.stderr().status(),
            OperationStatus::NotAttempted
        );
        assert!(
            create_failure
                .create()
                .errors()
                .any(|error| error.message().contains("runc log:"))
        );
        assert!(
            create_failure.create().errors().any(|error| {
                let message = error.message().to_ascii_lowercase();
                message.contains("rlimit")
            }),
            "create failure did not retain the target rlimit diagnostic: {:?}",
            create_failure.create().errors().collect::<Vec<_>>()
        );
        assert_workspace_empty(workspace.path());
        assert_eq!(engine_cgroups(), cgroups_before, "owned cgroup leaked");
    }

    #[derive(Default)]
    struct MemoryStore {
        blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl OciContentStore for MemoryStore {
        fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
            self.blobs
                .lock()
                .expect("store lock")
                .get(&descriptor.digest().to_string())
                .cloned()
                .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn OciContent>)
                .ok_or_else(|| {
                    ContentError::new(ContentErrorKind::Unavailable, "test content is absent")
                })
        }

        fn publish(
            &self,
            descriptor: &Descriptor,
            content: &mut dyn Read,
        ) -> Result<(), ContentError> {
            let mut bytes = Vec::new();
            content.read_to_end(&mut bytes).map_err(|error| {
                ContentError::new(ContentErrorKind::Internal, error.to_string())
            })?;
            let actual = descriptor_for_test_bytes(descriptor.media_type().clone(), &bytes);
            if actual.size() != descriptor.size() || actual.digest() != descriptor.digest() {
                return Err(ContentError::new(
                    ContentErrorKind::Rejected,
                    "published bytes do not match descriptor",
                ));
            }
            self.blobs
                .lock()
                .expect("store lock")
                .insert(descriptor.digest().to_string(), bytes);
            Ok(())
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DockerArchiveManifest {
        config: String,
        layers: Vec<String>,
    }

    fn import_docker_archive(store: &MemoryStore, archive: &Path) -> ImageDescriptor {
        let mut files = BTreeMap::new();
        for item in tar::Archive::new(File::open(archive).expect("docker archive"))
            .entries()
            .expect("docker archive entries")
        {
            let mut item = item.expect("docker archive entry");
            if item.header().entry_type().is_file() {
                let path = item.path().expect("archive path").into_owned();
                let mut bytes = Vec::new();
                item.read_to_end(&mut bytes).expect("archive bytes");
                files.insert(path, bytes);
            }
        }
        let manifests: Vec<DockerArchiveManifest> = serde_json::from_slice(
            files
                .get(Path::new("manifest.json"))
                .expect("docker manifest.json"),
        )
        .expect("docker manifest.json shape");
        let manifest = manifests.first().expect("one docker image");
        let config_bytes = files
            .get(Path::new(&manifest.config))
            .expect("docker config")
            .clone();
        let config = descriptor_for_test_bytes(MediaType::ImageConfig, &config_bytes);
        let layers = manifest
            .layers
            .iter()
            .map(|path| {
                let bytes = files.get(Path::new(path)).expect("docker layer").clone();
                let media_type = if bytes.starts_with(&[0x1f, 0x8b]) {
                    MediaType::ImageLayerGzip
                } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                    MediaType::ImageLayerZstd
                } else {
                    MediaType::ImageLayer
                };
                let descriptor = descriptor_for_test_bytes(media_type, &bytes);
                (descriptor, bytes)
            })
            .collect::<Vec<_>>();
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": &config,
            "layers": layers.iter().map(|(descriptor, _)| descriptor).collect::<Vec<_>>()
        }))
        .expect("OCI manifest");
        let manifest_descriptor =
            descriptor_for_test_bytes(MediaType::ImageManifest, &manifest_bytes);
        let mut blobs = store.blobs.lock().expect("store lock");
        blobs.insert(config.digest().to_string(), config_bytes);
        for (descriptor, bytes) in layers {
            blobs.insert(descriptor.digest().to_string(), bytes);
        }
        blobs.insert(manifest_descriptor.digest().to_string(), manifest_bytes);
        ImageDescriptor::new(manifest_descriptor).expect("image descriptor")
    }

    fn descriptor_for_test_bytes(media_type: MediaType, bytes: &[u8]) -> Descriptor {
        let digest = Sha256::digest(bytes);
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            write!(encoded, "{byte:02x}").expect("hex write");
        }
        Descriptor::new(
            media_type,
            u64::try_from(bytes.len()).expect("blob size"),
            Digest::try_from(format!("sha256:{encoded}")).expect("digest"),
        )
    }

    fn image_with_platform_field(
        store: &MemoryStore,
        field: &str,
        value: serde_json::Value,
    ) -> ImageDescriptor {
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            other => other,
        };
        let mut config_value = json!({
            "architecture": architecture,
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": []},
            "config": {}
        });
        config_value
            .as_object_mut()
            .expect("config object")
            .insert(field.to_owned(), value);
        let config_bytes = serde_json::to_vec(&config_value).expect("config bytes");
        let config = descriptor_for_test_bytes(MediaType::ImageConfig, &config_bytes);
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": &config,
            "layers": []
        }))
        .expect("manifest bytes");
        let manifest = descriptor_for_test_bytes(MediaType::ImageManifest, &manifest_bytes);
        let mut blobs = store.blobs.lock().expect("store lock");
        blobs.insert(config.digest().to_string(), config_bytes);
        blobs.insert(manifest.digest().to_string(), manifest_bytes);
        ImageDescriptor::new(manifest).expect("image descriptor")
    }

    fn e2e_input(
        image: &ImageDescriptor,
        name: &str,
        script: &str,
        stdin: &[u8],
        timeout: Option<NonZeroU64>,
    ) -> RunInput {
        e2e_input_with_cwd(image, name, script, stdin, timeout, "/")
    }

    fn delayed_runc_wrapper(real_runc: &Path, delayed_operation: &str) -> (TempDir, PathBuf) {
        let workspace = tempfile::tempdir().expect("wrapper workspace");
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("private wrapper workspace");
        let wrapper = workspace.path().join("runc");
        let quoted_runc = format!("'{}'", real_runc.to_string_lossy().replace('\'', "'\\''"));
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\noperation=\nhelp=0\nfor argument in \"$@\"; do\n  case \"$argument\" in create|start) operation=\"$argument\";; --help) help=1;; esac\ndone\nif [ \"$operation\" = {delayed_operation} ] && [ \"$help\" -eq 0 ]; then sleep 1; fi\nexec {quoted_runc} \"$@\"\n"
            ),
        )
        .expect("write runc wrapper");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
            .expect("executable runc wrapper");
        (workspace, wrapper)
    }

    fn e2e_input_uncgrouped(
        image: &ImageDescriptor,
        name: &str,
        script: &str,
        stdin: &[u8],
        timeout: Option<NonZeroU64>,
    ) -> RunInput {
        e2e_input_with_cwd(image, name, script, stdin, timeout, "/")
    }

    fn e2e_input_with_cwd(
        image: &ImageDescriptor,
        name: &str,
        script: &str,
        stdin: &[u8],
        timeout: Option<NonZeroU64>,
        cwd: &str,
    ) -> RunInput {
        let program = e2e_program_with_options(image, name, script, stdin, cwd, false);
        RunInput::new(
            BTreeMap::from([(ProgramId::primary(), program)]),
            timeout,
            Network::Isolated,
        )
        .expect("RunInput")
    }

    fn e2e_program(
        image: &ImageDescriptor,
        name: &str,
        script: &str,
        stdin: &[u8],
        _resources: bool,
    ) -> ProgramInput {
        e2e_program_with_options(image, name, script, stdin, "/", false)
    }

    fn e2e_input_with_invalid_rlimit(
        image: &ImageDescriptor,
        name: &str,
        script: &str,
    ) -> RunInput {
        let program = e2e_program_with_options(image, name, script, b"", "/", true);
        RunInput::new(
            BTreeMap::from([(ProgramId::primary(), program)]),
            None,
            Network::Isolated,
        )
        .expect("RunInput")
    }

    fn e2e_program_with_options(
        image: &ImageDescriptor,
        _name: &str,
        script: &str,
        stdin: &[u8],
        cwd: &str,
        invalid_rlimit: bool,
    ) -> ProgramInput {
        let linux = json!({
            "namespaces": [
                {"type": "pid"}, {"type": "network"}, {"type": "ipc"},
                {"type": "uts"}, {"type": "mount"}, {"type": "cgroup"}
            ],
            "resources": {"memory": {"limit": 134_217_728}, "pids": {"limit": 64}}
        });
        let mut value = json!({
            "ociVersion": "1.3.0",
            "root": {"path": "rootfs", "readonly": false},
            "process": {
                "terminal": false,
                "user": {"uid": 0, "gid": 0},
                "args": ["/bin/sh", "-c", script],
                "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                "cwd": cwd,
                "noNewPrivileges": true,
                "capabilities": {
                    "bounding": [], "effective": [], "inheritable": [],
                    "permitted": [], "ambient": []
                }
            },
            "hostname": "runlab-e2e",
            "mounts": [
                {"destination": "/proc", "type": "proc", "source": "proc"},
                {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]},
                {"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "noexec", "nodev", "ro"]},
                {"destination": "/runtime-created/nested", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=755", "size=1m"]}
            ],
            "linux": linux
        });
        if invalid_rlimit {
            value["process"]["rlimits"] = json!([{
                "type": "RLIMIT_NOFILE", "soft": 2, "hard": 1
            }]);
        }
        let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime JSON"))
            .expect("runtime config");
        ProgramInput::new(image.clone(), runtime, stdin.to_vec()).expect("program")
    }

    fn assert_final_delta(store: &MemoryStore, image: &ImageDescriptor) {
        let verified = inspect_image(store, image).expect("verify final image");
        let layers = verified
            .layers()
            .iter()
            .map(|layer| VerifiedLayer {
                descriptor: layer.descriptor(),
                expected_diff_id: layer.diff_id(),
            })
            .collect::<Vec<_>>();
        let workspace = tempfile::tempdir().expect("materialize workspace");
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("private materialize workspace");
        let rootfs = Rootfs::materialize_in(
            workspace.path(),
            &layers,
            RootfsLimits::default(),
            |descriptor| store.open(descriptor).map_err(anyhow::Error::from),
        )
        .expect("materialize final image");
        assert_eq!(
            fs::read(rootfs.path().join("result/value")).expect("delta"),
            b"delta"
        );
        assert!(rootfs.path().join("result/cgroup").is_file());
        assert_eq!(
            fs::read(rootfs.path().join("runtime-created/persistent")).expect("persistent delta"),
            b"persistent"
        );
        assert!(!rootfs.path().join("runtime-created/nested").exists());
    }

    fn assert_workspace_empty(path: &Path) {
        assert_eq!(fs::read_dir(path).expect("workspace entries").count(), 0);
    }

    fn engine_cgroups() -> BTreeSet<PathBuf> {
        let root = Path::new("/sys/fs/cgroup");
        let mut pending = vec![root.to_path_buf()];
        let mut matches = BTreeSet::new();
        let mut visited = 0_usize;
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("read cgroup tree") {
                let entry = entry.expect("cgroup entry");
                if !entry.file_type().expect("cgroup entry type").is_dir() {
                    continue;
                }
                visited += 1;
                assert!(visited <= 100_000, "cgroup tree exceeds test bound");
                let path = entry.path();
                if entry.file_name().as_bytes().starts_with(b"run-engine-") {
                    matches.insert(path.clone());
                }
                pending.push(path);
            }
        }
        matches
    }

    fn test_engine() -> NativeEngine {
        NativeEngine::new(
            Arc::new(UnavailableStore),
            PathBuf::from("/intentionally-unprobed"),
            PathBuf::from("/intentionally-unprobed/runc"),
            OperationTimeouts::default(),
        )
    }

    fn test_program() -> ProgramInput {
        let runtime = RuntimeConfig::parse(
            br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"terminal":false,"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0},"noNewPrivileges":true,"capabilities":{"bounding":[],"effective":[],"inheritable":[],"permitted":[],"ambient":[]}},"linux":{"namespaces":[{"type":"pid"},{"type":"network"},{"type":"ipc"},{"type":"uts"},{"type":"mount"},{"type":"cgroup"}]}}"#.to_vec(),
        )
        .expect("runtime");
        ProgramInput::new(test_image(), runtime, Vec::new()).expect("program")
    }

    fn test_image() -> ImageDescriptor {
        ImageDescriptor::new(Descriptor::new(
            MediaType::ImageManifest,
            0,
            Digest::try_from(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .expect("digest"),
        ))
        .expect("image descriptor")
    }
}
