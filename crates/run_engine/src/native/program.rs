use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset};
use run_protocol::{
    Availability, CreateFacts, EngineError, ImageDescriptor, OperationError, OperationReport,
    ProcessResult, ProgramOutput, StartFacts, StdinOutput, StopAction, StreamFacts,
};

use super::linux_evidence::ProcExitMonitor;
use super::network::EgressNetwork;
use super::report::output_internal;
use super::stdio::{InputTransfer, StreamDrain};
use super::subprocess::{InvocationSupervisor, RunningHelper, SupervisorToken};
use super::time::{checked_deadline, wall_clock_now};
use crate::{EngineObserver, ProgramStream};

pub(super) struct ProgramRun {
    pub(super) supervision: SupervisionState,
    pub(super) runtime: RuntimeState,
    pub(super) io: ProgramIo,
    pub(super) facts: ProgramFacts,
}

pub(super) struct SupervisionState {
    pub(super) owner: InvocationSupervisor,
    pub(super) create_child: Option<SupervisorToken>,
    pub(super) state_probe: Option<RunningHelper>,
    pub(super) state_probe_deadline: Option<Instant>,
    pub(super) state_probe_failed: bool,
    pub(super) unreaped: bool,
}

pub(super) struct RuntimeState {
    pub(super) coordinates: Option<(PathBuf, PathBuf, String, Duration)>,
    pub(super) exit_monitor: Option<ProcExitMonitor>,
    pub(super) exit_monitor_diagnostic: Option<String>,
    pub(super) stopped_observation: Option<(DateTime<FixedOffset>, Instant)>,
    pub(super) pidfd: Option<OwnedFd>,
    pub(super) cgroup_path: Option<PathBuf>,
    pub(super) attempted: bool,
    pub(super) execution_entry: Option<(DateTime<FixedOffset>, Instant)>,
    pub(super) poll_failed: bool,
    pub(super) writer_stopped: bool,
    pub(super) rootfs_stability: RootfsStability,
    pub(super) egress: Option<EgressNetwork>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum RootfsStability {
    #[default]
    Unproved,
    Stable,
}

pub(super) struct ProgramIo {
    pub(super) stdin_transfer: Option<InputTransfer>,
    pub(super) stdout_drain: Option<StreamDrain>,
    pub(super) stderr_drain: Option<StreamDrain>,
}

pub(super) struct ProgramFacts {
    pub(super) create: OperationReport<CreateFacts>,
    pub(super) start: OperationReport<StartFacts>,
    pub(super) process: Option<ProcessResult>,
    pub(super) stdin: Option<StdinOutput>,
    pub(super) stdout: Option<OperationReport<StreamFacts>>,
    pub(super) stderr: Option<OperationReport<StreamFacts>>,
    pub(super) stop_actions: Vec<StopAction>,
    pub(super) errors: Vec<OperationError>,
}

impl ProgramRun {
    #[cfg(test)]
    pub(super) fn unattempted() -> Self {
        Self::unattempted_with(InvocationSupervisor::new())
    }

    pub(super) fn unattempted_with(supervisor: InvocationSupervisor) -> Self {
        Self {
            supervision: SupervisionState {
                owner: supervisor,
                create_child: None,
                state_probe: None,
                state_probe_deadline: None,
                state_probe_failed: false,
                unreaped: false,
            },
            runtime: RuntimeState {
                coordinates: None,
                exit_monitor: None,
                exit_monitor_diagnostic: None,
                stopped_observation: None,
                pidfd: None,
                cgroup_path: None,
                attempted: false,
                execution_entry: None,
                poll_failed: false,
                writer_stopped: true,
                rootfs_stability: RootfsStability::Unproved,
                egress: None,
            },
            io: ProgramIo {
                stdin_transfer: None,
                stdout_drain: None,
                stderr_drain: None,
            },
            facts: ProgramFacts {
                create: OperationReport::not_attempted("runc create was not attempted")
                    .expect("literal reason"),
                start: OperationReport::not_attempted("runc start was not attempted")
                    .expect("literal reason"),
                process: None,
                stdin: None,
                stdout: None,
                stderr: None,
                stop_actions: Vec::new(),
                errors: Vec::new(),
            },
        }
    }

    pub(super) fn observe_output(
        &mut self,
        program_id: run_protocol::ProgramId,
        observer: Arc<dyn EngineObserver>,
    ) {
        if let Some(stdout) = &mut self.io.stdout_drain {
            stdout.observe(
                program_id.clone(),
                ProgramStream::Stdout,
                Arc::clone(&observer),
            );
        }
        if let Some(stderr) = &mut self.io.stderr_drain {
            stderr.observe(program_id, ProgramStream::Stderr, observer);
        }
    }

    pub(super) fn output(
        &mut self,
        final_environment: Availability<ImageDescriptor>,
    ) -> Result<ProgramOutput, EngineError> {
        let process = self.facts.process.take().unwrap_or_else(|| {
            ProcessResult::never_started("the Program did not reach a proved start")
                .expect("literal reason")
        });
        let stdin = self.facts.stdin.take().unwrap_or_else(unattempted_stdin);
        let stdout = self.facts.stdout.take().unwrap_or_else(unattempted_stream);
        let stderr = self.facts.stderr.take().unwrap_or_else(unattempted_stream);
        ProgramOutput::new(
            self.facts.create.clone(),
            self.facts.start.clone(),
            process,
            stdin,
            stdout,
            stderr,
            std::mem::take(&mut self.facts.stop_actions),
            final_environment,
            std::mem::take(&mut self.facts.errors),
        )
        .map_err(output_internal)
    }

    pub(super) fn pump_io(&mut self) {
        if let Some(transfer) = &mut self.io.stdin_transfer {
            transfer.pump();
        }
        self.pump_output();
    }

    pub(super) fn pump_output(&mut self) {
        if let Some(drain) = &mut self.io.stdout_drain {
            drain.pump();
        }
        if let Some(drain) = &mut self.io.stderr_drain {
            drain.pump();
        }
    }

    pub(super) fn freeze_stdin(&mut self) {
        if let Some(transfer) = &mut self.io.stdin_transfer {
            transfer.freeze();
        }
    }

    pub(super) fn observe_runtime_stopped(&mut self, wait_timeout: Duration) {
        self.runtime.writer_stopped = true;
        let ended_at = wall_clock_now();
        if self.runtime.exit_monitor.is_some() {
            let deadline =
                checked_deadline(Instant::now(), wait_timeout, "raw exit evidence deadline")
                    .expect("validated OperationTimeouts fit Instant");
            self.runtime.stopped_observation = Some((ended_at, deadline));
        } else {
            self.facts.process = Some(
                ProcessResult::unknown(
                    "runc state directly proved process termination but does not expose an unflattened process result",
                    Availability::available(ended_at),
                )
                .expect("literal reason"),
            );
        }
    }

    pub(super) fn fallback_after_stopped(&mut self, reason: impl Into<String>) {
        let ended_at = self
            .runtime
            .stopped_observation
            .take()
            .map_or_else(wall_clock_now, |(ended_at, _)| ended_at);
        self.facts.process = Some(
            ProcessResult::unknown(reason, Availability::available(ended_at))
                .expect("non-empty reason"),
        );
    }

    pub(super) fn observe_raw_process_result(&mut self, result: ProcessResult) {
        self.runtime.writer_stopped = true;
        self.runtime.stopped_observation = None;
        self.facts.process = Some(result);
    }

    pub(super) fn io_complete(&self) -> bool {
        self.io
            .stdin_transfer
            .as_ref()
            .is_none_or(InputTransfer::is_closed)
            && self
                .io
                .stdout_drain
                .as_ref()
                .is_none_or(StreamDrain::is_closed)
            && self
                .io
                .stderr_drain
                .as_ref()
                .is_none_or(StreamDrain::is_closed)
    }
}

pub(super) fn unattempted_stdin() -> StdinOutput {
    StdinOutput::new(
        OperationReport::not_attempted("stdin was not connected").expect("literal reason"),
        OperationReport::not_attempted("stdin was not connected").expect("literal reason"),
    )
}

pub(super) fn unattempted_stream() -> OperationReport<StreamFacts> {
    OperationReport::not_attempted("the Program was not started").expect("literal reason")
}
