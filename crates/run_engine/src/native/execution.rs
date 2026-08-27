use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::budget::OperationBudget;
use super::capture::capture;
use super::cleanup::{RuntimeCleanup, RuntimeCleanupReport, cleanup_invocation, cleanup_runtime};
use super::prepare::{PreparedInvocation, PreparedProgram};
use super::program::{ProgramRun, RootfsStability};
use super::report::{operation_error, output_internal};
use super::start::{ProgramStarter, StartControl};
use super::stop::stop_all;
use super::subprocess::{InvocationSupervisor, SupervisorLifecycle};
use super::time::{POLL_INTERVAL, execution_expired, wall_clock_now};
use super::wait::{finalize_children, poll_children};
use crate::{CancellationToken, OciContentStore, OperationTimeouts};
use anyhow::{Result as AnyResult, bail};
use run_protocol::{
    Availability, EngineError, ExecutionInterval, ExecutionOutput, ImageDescriptor, OperationStage,
    OperationStatus, ProcessResult, ProgramId, RunInput, RunOutput,
};

pub(super) struct ExecutionContext {
    store: Arc<dyn OciContentStore>,
    timeouts: OperationTimeouts,
}

impl ExecutionContext {
    pub(super) const fn new(store: Arc<dyn OciContentStore>, timeouts: OperationTimeouts) -> Self {
        Self { store, timeouts }
    }
}

pub(super) fn execute(
    context: &ExecutionContext,
    input: &RunInput,
    cancellation: &CancellationToken,
    prepared: &mut PreparedInvocation,
) -> Result<RunOutput, EngineError> {
    let mut lifecycle = ExecutionLifecycle::new(input, &prepared.supervisor);
    lifecycle.start_programs(context, input, cancellation, prepared);
    let secret_cleanup_error = scrub_sensitive_artifacts(prepared).err().map(|error| {
        operation_error(
            OperationStage::Cleanup,
            format!("failed to remove transient Secret material after OCI create: {error:#}"),
            None,
        )
    });
    lifecycle.wait_for_termination(cancellation);
    for run in lifecycle.runs.values_mut() {
        run.freeze_stdin();
    }
    let interval_end = wall_clock_now();
    stop_all(
        &prepared.runc,
        &prepared.runtime_root,
        &prepared.programs,
        &mut lifecycle.runs,
        context.timeouts,
    );
    finalize_children(&mut lifecycle.runs, context.timeouts);

    let cleanup_budget = OperationBudget::new(context.timeouts.cleanup(), "invocation cleanup")
        .map_err(|error| EngineError::internal(format!("{error:#}")))?;
    let supervisor_deadline = cleanup_budget.deadline();
    establish_capture_safety(prepared, &mut lifecycle.runs, supervisor_deadline)?;
    cleanup_program_runtimes(context, prepared, &mut lifecycle.runs, supervisor_deadline);
    finalize_supervisor_for_capture(prepared, &mut lifecycle.runs, supervisor_deadline)?;
    let outputs = capture_outputs(context, prepared, &mut lifecycle.runs)?;

    let all_writers_stopped = lifecycle
        .runs
        .values()
        .all(|run| run.runtime.writer_stopped);
    let cleanup_error =
        cleanup_invocation(prepared, all_writers_stopped, cleanup_budget).map(|issue| {
            let (message, code) = issue.into_parts();
            operation_error(OperationStage::Cleanup, message, code)
        });
    let execution_errors = secret_cleanup_error
        .into_iter()
        .chain(cleanup_error)
        .collect::<Vec<_>>();
    let interval = lifecycle
        .interval_start
        .map_or_else(
            || ExecutionInterval::not_entered("no Program reached an OCI start attempt"),
            |started_at| Ok(ExecutionInterval::entered(started_at, interval_end)),
        )
        .map_err(output_internal)?;
    let execution = ExecutionOutput::new(
        interval,
        lifecycle.termination_reason() == TerminationReason::TimedOut,
        lifecycle.termination_reason() == TerminationReason::Cancelled,
        execution_errors,
    )
    .map_err(output_internal)?;
    if lifecycle.runs.values().any(|run| run.supervision.unreaped) {
        return Err(EngineError::internal(
            "NativeEngine could not prove that every runtime helper was terminated and reaped; full cleanup was attempted and no trustworthy RunOutput is available",
        ));
    }
    RunOutput::new(input, execution, outputs).map_err(output_internal)
}

fn scrub_sensitive_artifacts(prepared: &PreparedInvocation) -> AnyResult<()> {
    // OCI create has consumed config.json and pinned each bind source; unlinking
    // their host paths does not invalidate the files visible to the Program.
    for path in prepared
        .programs
        .values()
        .flat_map(|program| &program.sensitive_artifacts)
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct ExecutionLifecycle {
    runs: BTreeMap<ProgramId, ProgramRun>,
    interval_start: Option<chrono::DateTime<chrono::FixedOffset>>,
    monotonic_start: Option<Instant>,
    execution_limit: Option<Duration>,
    termination_reason: Option<TerminationReason>,
}

impl ExecutionLifecycle {
    fn new(input: &RunInput, supervisor: &InvocationSupervisor) -> Self {
        Self {
            runs: input
                .programs()
                .keys()
                .cloned()
                .map(|id| (id, ProgramRun::unattempted_with(supervisor.clone())))
                .collect(),
            interval_start: None,
            monotonic_start: None,
            execution_limit: input
                .execution_timeout_ms()
                .map(|value| Duration::from_millis(value.get())),
            termination_reason: None,
        }
    }

    fn start_programs(
        &mut self,
        context: &ExecutionContext,
        input: &RunInput,
        cancellation: &CancellationToken,
        prepared: &PreparedInvocation,
    ) {
        let starter = ProgramStarter::new(
            &prepared.supervisor,
            &prepared.runc,
            &prepared.runtime_root,
            context.timeouts,
        );
        for program_id in program_order(input) {
            if self.stop_requested(cancellation) {
                break;
            }
            let run = starter.start(
                &prepared.programs[&program_id],
                &input.programs()[&program_id],
                StartControl::new(cancellation, self.monotonic_start, self.execution_limit),
                &mut self.runs,
            );
            let start_succeeded = run.facts.start.status() == OperationStatus::Succeeded;
            self.observe_execution_entry(&run);
            self.runs.insert(program_id, run);
            if self.stop_requested(cancellation) {
                break;
            }
            if !start_succeeded {
                self.termination_reason = Some(TerminationReason::Lifecycle);
                break;
            }
        }
    }

    fn wait_for_termination(&mut self, cancellation: &CancellationToken) {
        if self.termination_reason.is_some() {
            return;
        }
        loop {
            if poll_children(&mut self.runs) {
                self.termination_reason = Some(TerminationReason::Lifecycle);
                break;
            }
            if self.runs[&ProgramId::primary()].facts.process.is_some() {
                self.termination_reason = Some(TerminationReason::PrimaryEnded);
                break;
            }
            if self.stop_requested(cancellation) {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn observe_execution_entry(&mut self, run: &ProgramRun) {
        if self.monotonic_start.is_none()
            && let Some((wall_clock, monotonic)) = &run.runtime.execution_entry
        {
            self.interval_start = Some(*wall_clock);
            self.monotonic_start = Some(*monotonic);
        }
    }

    fn stop_requested(&mut self, cancellation: &CancellationToken) -> bool {
        if cancellation.is_cancelled() {
            self.termination_reason = Some(TerminationReason::Cancelled);
            return true;
        }
        if execution_expired(self.monotonic_start, self.execution_limit) {
            self.termination_reason = Some(TerminationReason::TimedOut);
            return true;
        }
        false
    }

    fn termination_reason(&self) -> TerminationReason {
        self.termination_reason
            .unwrap_or(TerminationReason::Lifecycle)
    }
}

fn program_order(input: &RunInput) -> Vec<ProgramId> {
    input
        .programs()
        .keys()
        .filter(|id| !id.is_primary())
        .cloned()
        .chain(std::iter::once(ProgramId::primary()))
        .collect()
}

fn establish_capture_safety(
    prepared: &mut PreparedInvocation,
    runs: &mut BTreeMap<ProgramId, ProgramRun>,
    deadline: Instant,
) -> Result<(), EngineError> {
    if let Err(error) = establish_runtime_cleanup_safety(&prepared.supervisor, deadline) {
        mark_writers_unstopped(runs);
        if let Some(workspace) = prepared.workspace.take() {
            return Err(preserve_workspace_after_supervisor_failure(
                &workspace, &error,
            ));
        }
        return Err(EngineError::internal(format!("{error:#}")));
    }
    Ok(())
}

fn cleanup_program_runtimes(
    context: &ExecutionContext,
    prepared: &PreparedInvocation,
    runs: &mut BTreeMap<ProgramId, ProgramRun>,
    deadline: Instant,
) {
    for (program_id, program) in &prepared.programs {
        let run = runs.get_mut(program_id).expect("output slot exists");
        let report = cleanup_runtime(RuntimeCleanup {
            runc: &prepared.runc,
            runtime_root: &prepared.runtime_root,
            program,
            supervisor: &run.supervision.owner,
            runtime_attempted: run.runtime.attempted,
            removal_timeout: context.timeouts.runtime_filesystem_removal(),
            supervisor_deadline: deadline,
            egress: &mut run.runtime.egress,
        });
        apply_runtime_cleanup_report(run, report);
    }
}

fn finalize_supervisor_for_capture(
    prepared: &mut PreparedInvocation,
    runs: &mut BTreeMap<ProgramId, ProgramRun>,
    deadline: Instant,
) -> Result<(), EngineError> {
    if let Err(error) = prepared.supervisor.finalize(deadline) {
        mark_writers_unstopped(runs);
        if let Some(workspace) = prepared.workspace.take() {
            return Err(preserve_workspace_after_supervisor_failure(
                &workspace, &error,
            ));
        }
        return Err(EngineError::internal(format!(
            "invocation supervisor could not prove every child reaped before capture: {error:#}"
        )));
    }
    for run in runs.values_mut() {
        run.supervision.unreaped = false;
    }
    Ok(())
}

fn mark_writers_unstopped(runs: &mut BTreeMap<ProgramId, ProgramRun>) {
    for run in runs.values_mut() {
        run.runtime.writer_stopped = false;
    }
}

fn capture_outputs(
    context: &ExecutionContext,
    prepared: &mut PreparedInvocation,
    runs: &mut BTreeMap<ProgramId, ProgramRun>,
) -> Result<BTreeMap<ProgramId, run_protocol::ProgramOutput>, EngineError> {
    let mut outputs = BTreeMap::new();
    for (program_id, program) in &mut prepared.programs {
        let run = runs.get_mut(program_id).expect("output slot exists");
        let final_environment = capture_final(context, program, run);
        outputs.insert(program_id.clone(), run.output(final_environment)?);
    }
    Ok(outputs)
}

fn apply_runtime_cleanup_report(run: &mut ProgramRun, report: RuntimeCleanupReport) {
    if report.runtime_deleted {
        run.runtime.writer_stopped |=
            run.supervision.create_child.is_none() && run.supervision.state_probe.is_none();
        if run.facts.process.is_none() && run.facts.start.status() != OperationStatus::NotAttempted
        {
            run.facts.process = Some(
                ProcessResult::unknown(
                    "runc delete --force proved the runtime object and possible process were removed without an unflattened process result",
                    Availability::available(wall_clock_now()),
                )
                .expect("literal reason"),
            );
        }
    }
    run.runtime.rootfs_stability = report.rootfs_stability;
    run.supervision.unreaped |= report.supervisor_unreaped;
    run.facts
        .errors
        .extend(report.issues.into_iter().map(|issue| {
            let (message, code) = issue.into_parts();
            operation_error(OperationStage::RuntimeFilesystemRemoval, message, code)
        }));
}

fn establish_runtime_cleanup_safety(
    supervisor: &InvocationSupervisor,
    deadline: Instant,
) -> AnyResult<()> {
    supervisor.finalize(deadline)?;
    if supervisor.lifecycle() != SupervisorLifecycle::Reaped {
        bail!("invocation supervisor still owns an active helper");
    }
    if Instant::now() >= deadline {
        bail!(
            "invocation cleanup deadline expired after supervisor safety was established and before runtime cleanup"
        );
    }
    Ok(())
}

fn preserve_workspace_after_supervisor_failure(
    workspace: &std::path::Path,
    error: &anyhow::Error,
) -> EngineError {
    EngineError::internal(format!(
        "invocation supervisor could not prove every child reaped before capture; preserved workspace {}: {error:#}",
        workspace.display()
    ))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminationReason {
    PrimaryEnded,
    TimedOut,
    Cancelled,
    Lifecycle,
}

fn capture_final(
    context: &ExecutionContext,
    program: &PreparedProgram,
    run: &mut ProgramRun,
) -> Availability<ImageDescriptor> {
    if !run.runtime.writer_stopped {
        return Availability::unavailable(
            "a process that could still write the rootfs was not proved stopped",
        )
        .expect("literal reason");
    }
    if run.runtime.rootfs_stability != RootfsStability::Stable {
        return Availability::unavailable(
            "the underlying rootfs was not proved stable after mount and artifact cleanup",
        )
        .expect("literal reason");
    }
    match capture(
        Arc::clone(&context.store),
        context.timeouts.final_environment_capture(),
        program,
    ) {
        Ok(image) => Availability::available(image),
        Err(error) => {
            run.facts.errors.push(operation_error(
                OperationStage::FinalEnvironmentCapture,
                error.operation_message(),
                None,
            ));
            Availability::unavailable(error.unavailable_reason()).expect("non-empty reason")
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
