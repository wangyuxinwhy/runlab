use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult, bail};
use run_protocol::{
    CreateFacts, OperationError, OperationReport, OperationStage, OperationStatus, ProgramId,
    ProgramInput, StartFacts,
};

use super::cgroup::observe_owned_cgroup;
use super::linux_evidence::{PidfdReceiver, ProcExitMonitor, pidfd_process_id};
use super::network::EgressNetwork;
use super::prepare::{MAX_EXECUTION_TIMEOUT, PreparedProgram};
use super::program::ProgramRun;
use super::report::operation_error;
use super::runc::{create_failure_message, helper_message, runc_command};
use super::stdio::{InputTransfer, StreamDrain};
use super::subprocess::{
    HELPER_OUTPUT_LIMIT, HelperOutput, InvocationSupervisor, RunningHelper, SUPERVISOR_REAP_LIMIT,
    terminate_child,
};
use super::time::{POLL_INTERVAL, checked_deadline, execution_expired, wall_clock_now};
use crate::{CancellationToken, EngineEventSink, OperationTimeouts};

pub(super) struct ProgramStarter<'a> {
    supervisor: &'a InvocationSupervisor,
    runc: &'a Path,
    runtime_root: &'a Path,
    timeouts: OperationTimeouts,
    event_sink: Arc<dyn EngineEventSink>,
    execution_event_sent: Cell<bool>,
}

#[derive(Clone, Copy)]
pub(super) struct StartControl<'a> {
    cancellation: &'a CancellationToken,
    execution_start: Option<Instant>,
    execution_limit: Option<Duration>,
}

impl<'a> StartControl<'a> {
    pub(super) const fn new(
        cancellation: &'a CancellationToken,
        execution_start: Option<Instant>,
        execution_limit: Option<Duration>,
    ) -> Self {
        Self {
            cancellation,
            execution_start,
            execution_limit,
        }
    }
}

impl<'a> ProgramStarter<'a> {
    pub(super) const fn new(
        supervisor: &'a InvocationSupervisor,
        runc: &'a Path,
        runtime_root: &'a Path,
        timeouts: OperationTimeouts,
        event_sink: Arc<dyn EngineEventSink>,
    ) -> Self {
        Self {
            supervisor,
            runc,
            runtime_root,
            timeouts,
            event_sink,
            execution_event_sent: Cell::new(false),
        }
    }
}

impl ProgramStarter<'_> {
    pub(super) fn start(
        &self,
        program_id: &ProgramId,
        prepared: &PreparedProgram,
        input: &ProgramInput,
        control: StartControl<'_>,
        other_runs: &mut BTreeMap<ProgramId, ProgramRun>,
    ) -> ProgramRun {
        if control.cancellation.is_cancelled()
            || execution_expired(control.execution_start, control.execution_limit)
        {
            return ProgramRun::unattempted_with(self.supervisor.clone());
        }
        match self.create(prepared, input, control, other_runs) {
            CreateOutcome::Ready(mut run) => {
                run.forward_output_events(program_id.clone(), Arc::clone(&self.event_sink));
                self.start_created(prepared, control, other_runs, run)
            }
            CreateOutcome::Finished(run) => run,
        }
    }

    fn create(
        &self,
        prepared: &PreparedProgram,
        input: &ProgramInput,
        control: StartControl<'_>,
        other_runs: &mut BTreeMap<ProgramId, ProgramRun>,
    ) -> CreateOutcome {
        let Self {
            supervisor,
            runc,
            runtime_root,
            timeouts,
            ..
        } = self;
        let StartControl {
            cancellation,
            execution_start,
            execution_limit,
        } = control;
        CreateSession {
            supervisor,
            runc,
            runtime_root,
            timeouts: *timeouts,
            prepared,
            input,
            cancellation,
            execution_start,
            execution_limit,
            other_runs,
        }
        .run()
    }

    fn start_created(
        &self,
        prepared: &PreparedProgram,
        control: StartControl<'_>,
        other_runs: &mut BTreeMap<ProgramId, ProgramRun>,
        mut run: ProgramRun,
    ) -> ProgramRun {
        let Self {
            runc,
            runtime_root,
            timeouts,
            ..
        } = *self;
        let StartControl {
            cancellation,
            execution_start,
            execution_limit,
        } = control;
        let Some(init_pid) = establish_created_process_evidence(prepared, &mut run) else {
            return run;
        };

        if let Some(plan) = prepared.egress.clone() {
            let network_deadline = checked_deadline(
                Instant::now(),
                timeouts.start(),
                "egress network setup deadline",
            )
            .expect("validated OperationTimeouts fit Instant");
            let mut network = EgressNetwork::new(plan);
            let setup = network.setup(
                init_pid,
                &run.supervision.owner,
                network_deadline,
                cancellation,
            );
            run.runtime.egress = Some(network);
            if let Err(error) = setup {
                run.supervision.unreaped |= !error.supervisor_reaped;
                run.facts.errors.push(operation_error(
                    OperationStage::Preparation,
                    format!("failed to establish outbound-only egress: {error}"),
                    None,
                ));
                return run;
            }
        }

        if !establish_process_result_monitor(init_pid, &mut run) {
            return run;
        }

        let start_wall = wall_clock_now();
        let start_monotonic = Instant::now();
        run.runtime.execution_entry = Some((start_wall, start_monotonic));
        if !self.execution_event_sent.replace(true) {
            self.event_sink.stage(crate::EngineStage::Executing);
        }
        let (start_deadline, deadline_message) =
            start_deadline(start_monotonic, execution_start, execution_limit, timeouts);
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
        run.facts.start = start_report(start_result);
        if run.facts.start.status() == OperationStatus::Failed {
            run.runtime.exit_monitor = None;
            return run;
        }
        run.runtime.coordinates = Some((
            runc.to_path_buf(),
            runtime_root.to_path_buf(),
            prepared.runtime_id.clone(),
            timeouts.wait(),
        ));
        run
    }
}

struct CreateSession<'a> {
    supervisor: &'a InvocationSupervisor,
    runc: &'a Path,
    runtime_root: &'a Path,
    timeouts: OperationTimeouts,
    prepared: &'a PreparedProgram,
    input: &'a ProgramInput,
    cancellation: &'a CancellationToken,
    execution_start: Option<Instant>,
    execution_limit: Option<Duration>,
    other_runs: &'a mut BTreeMap<ProgramId, ProgramRun>,
}

impl CreateSession<'_> {
    fn run(mut self) -> CreateOutcome {
        let mut helper = match self.launch() {
            CreateLaunch::Running(helper) => helper,
            CreateLaunch::Finished(run) => return CreateOutcome::Finished(run),
        };
        self.supervise(&mut helper);
        helper.stdout.pump();
        helper.stderr.pump();
        let termination_error = self.finish_supervisor(&mut helper);
        helper.run.facts.create = self.create_report(&helper, termination_error);
        if helper.run.facts.create.status() != OperationStatus::Succeeded
            || self.cancellation.is_cancelled()
            || execution_expired(self.execution_start, self.execution_limit)
        {
            return CreateOutcome::Finished(helper.run);
        }
        if !helper.pipes_are_empty() {
            helper.run.facts.errors.push(operation_error(
                OperationStage::ProcessSupervision,
                format!(
                    "runc create succeeded but pre-start Program pipes were not provably empty; stdout: {}; stderr: {}",
                    String::from_utf8_lossy(helper.stdout.bytes()),
                    String::from_utf8_lossy(helper.stderr.bytes())
                ),
                None,
            ));
            return CreateOutcome::Finished(helper.run);
        }
        helper.run.io.stdin_transfer = Some(InputTransfer::new(
            helper.stdin,
            self.input.stdin().to_vec(),
        ));
        helper.run.io.stdout_drain = Some(helper.stdout);
        helper.run.io.stderr_drain = Some(helper.stderr);
        helper.run.runtime.writer_stopped = false;
        CreateOutcome::Ready(helper.run)
    }

    fn launch(&self) -> CreateLaunch {
        let pidfd_receiver = match PidfdReceiver::bind(&self.prepared.pidfd_path) {
            Ok(receiver) => receiver,
            Err(error) => {
                let mut run = ProgramRun::unattempted_with(self.supervisor.clone());
                run.facts.create = OperationReport::failed(
                    operation_error(
                        OperationStage::Create,
                        format!("failed to create runc pidfd socket: {error}"),
                        error.raw_os_error().map(i64::from),
                    ),
                    [],
                );
                return CreateLaunch::Finished(run);
            }
        };
        let _ = fs::remove_file(&self.prepared.runc_log_path);
        let mut command = runc_command(self.runc, self.runtime_root);
        command
            .arg("--log")
            .arg(&self.prepared.runc_log_path)
            .arg("--log-format")
            .arg("json")
            .arg("create")
            .arg("--bundle")
            .arg(&self.prepared.bundle)
            .arg("--pidfd-socket")
            .arg(&self.prepared.pidfd_path)
            .arg(&self.prepared.runtime_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = match self.supervisor.spawn(&mut command) {
            Ok(child) => child,
            Err(error) => {
                let operation = operation_error(
                    OperationStage::Create,
                    format!("failed to spawn runc create: {error:#}"),
                    None,
                );
                let mut run = ProgramRun::unattempted_with(self.supervisor.clone());
                run.facts.create = OperationReport::failed(operation, []);
                return CreateLaunch::Finished(run);
            }
        };
        let (stdin, stdout, stderr) = self
            .supervisor
            .with_child(child, |child| {
                Ok((
                    child.stdin.take().expect("piped stdin"),
                    child.stdout.take().expect("piped stdout"),
                    child.stderr.take().expect("piped stderr"),
                ))
            })
            .expect("newly registered create child");
        let stdout = StreamDrain::from_stdout(stdout);
        let stderr = StreamDrain::from_stderr(stderr);
        let create_deadline = checked_deadline(
            Instant::now(),
            self.timeouts.create(),
            "runc create deadline",
        )
        .expect("validated OperationTimeouts fit Instant");
        let mut run = ProgramRun::unattempted_with(self.supervisor.clone());
        run.supervision.create_child = Some(child);
        run.runtime.attempted = true;
        CreateLaunch::Running(CreateHelper {
            pidfd_receiver,
            stdin,
            stdout,
            stderr,
            deadline: create_deadline,
            status: None,
            run,
        })
    }

    fn supervise(&mut self, helper: &mut CreateHelper) {
        loop {
            for other in self.other_runs.values_mut() {
                other.pump_io();
            }
            helper.stdout.pump();
            helper.stderr.pump();
            if helper.stdout.bytes().len() > HELPER_OUTPUT_LIMIT
                || helper.stderr.bytes().len() > HELPER_OUTPUT_LIMIT
                || fs::metadata(&self.prepared.runc_log_path)
                    .is_ok_and(|metadata| metadata.len() > HELPER_OUTPUT_LIMIT as u64)
            {
                helper.record_unknown(
                    "runc create diagnostics exceeded the bounded limit",
                    operation_error(
                        OperationStage::Create,
                        "runc create diagnostics exceeded the bounded 1 MiB limit",
                        None,
                    ),
                );
                break;
            }
            if helper.run.runtime.pidfd.is_none() {
                match helper.pidfd_receiver.try_receive() {
                    Ok(pidfd) => {
                        helper.run.runtime.pidfd = pidfd;
                    }
                    Err(error) => {
                        helper.record_unknown(
                            "runc pidfd receipt failed after runc create was spawned",
                            operation_error(
                                OperationStage::Create,
                                format!("failed to receive container pidfd from runc: {error}"),
                                error.raw_os_error().map(i64::from),
                            ),
                        );
                        break;
                    }
                }
            }
            let execution_deadline =
                self.execution_start
                    .zip(self.execution_limit)
                    .map(|(start, limit)| {
                        checked_deadline(start, limit, "execution timeout")
                            .expect("validated execution timeout fits Instant")
                    });
            let effective_deadline =
                execution_deadline.map_or(helper.deadline, |limit| limit.min(helper.deadline));
            if self.cancellation.is_cancelled() || Instant::now() >= effective_deadline {
                break;
            }
            match self.supervisor.try_wait(
                helper
                    .run
                    .supervision
                    .create_child
                    .expect("create supervisor"),
            ) {
                Ok(Some(status)) => {
                    helper.status = Some(status);
                    break;
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    helper.run.runtime.poll_failed = true;
                    helper.record_unknown(
                        "runc create outcome is unknown because its supervisor could not be polled",
                        operation_error(
                            OperationStage::Create,
                            format!("failed to poll runc create: {error}"),
                            error.raw_os_error().map(i64::from),
                        ),
                    );
                    break;
                }
            }
        }
    }

    fn finish_supervisor(&self, helper: &mut CreateHelper) -> Option<OperationError> {
        if helper.status.is_none() {
            match terminate_child(
                self.supervisor,
                &mut helper.run.supervision.create_child,
                SUPERVISOR_REAP_LIMIT,
            ) {
                Ok(()) => None,
                Err(error) => {
                    helper.run.supervision.unreaped = true;
                    Some(operation_error(
                        OperationStage::Create,
                        format!("failed to terminate runc create supervisor: {error:#}"),
                        None,
                    ))
                }
            }
        } else {
            self.supervisor
                .release_reaped(
                    helper
                        .run
                        .supervision
                        .create_child
                        .expect("completed create supervisor"),
                )
                .expect("completed create supervisor is reaped");
            helper.run.supervision.create_child = None;
            None
        }
    }

    fn create_report(
        &self,
        helper: &CreateHelper,
        termination_error: Option<OperationError>,
    ) -> OperationReport<CreateFacts> {
        if helper.status.is_none() && helper.run.facts.create.status() == OperationStatus::Unknown {
            let reason = helper
                .run
                .facts
                .create
                .reason()
                .expect("Unknown create report has a reason")
                .to_owned();
            let mut errors = helper
                .run
                .facts
                .create
                .errors()
                .cloned()
                .collect::<Vec<_>>();
            errors.extend(termination_error);
            return OperationReport::unknown(reason, errors).expect("existing reason is valid");
        }
        match helper.status {
            Some(status) if status.success() && helper.run.runtime.pidfd.is_some() => {
                OperationReport::succeeded(CreateFacts::new(wall_clock_now()))
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
                        helper.stdout.bytes(),
                        helper.stderr.bytes(),
                        &self.prepared.runc_log_path,
                    ),
                    status.code().map(i64::from),
                ),
                [],
            ),
            None => {
                let mut errors = vec![operation_error(
                    OperationStage::Create,
                    if self.cancellation.is_cancelled() {
                        "runc create interrupted by cancellation"
                    } else if execution_expired(self.execution_start, self.execution_limit) {
                        "execution timeout reached while runc create was in progress"
                    } else {
                        "runc create deadline exceeded"
                    },
                    None,
                )];
                errors.extend(termination_error);
                OperationReport::unknown(
                    "runc create did not complete before cancellation or its deadline",
                    errors,
                )
                .expect("literal reason")
            }
        }
    }
}

enum CreateLaunch {
    Running(CreateHelper),
    Finished(ProgramRun),
}

struct CreateHelper {
    pidfd_receiver: PidfdReceiver,
    stdin: ChildStdin,
    stdout: StreamDrain,
    stderr: StreamDrain,
    deadline: Instant,
    status: Option<std::process::ExitStatus>,
    run: ProgramRun,
}

impl CreateHelper {
    fn record_unknown(&mut self, reason: &'static str, error: OperationError) {
        self.run.facts.create = OperationReport::unknown(reason, [error]).expect("literal reason");
    }

    fn pipes_are_empty(&self) -> bool {
        self.stdout.bytes().is_empty()
            && self.stderr.bytes().is_empty()
            && self.stdout.error().is_none()
            && self.stderr.error().is_none()
            && !self.stdout.is_closed()
            && !self.stderr.is_closed()
    }
}

fn establish_created_process_evidence(
    prepared: &PreparedProgram,
    run: &mut ProgramRun,
) -> Option<u32> {
    let init_pid = match run
        .runtime
        .pidfd
        .as_ref()
        .context("successful create has no pidfd")
        .and_then(pidfd_process_id)
    {
        Ok(pid) => pid,
        Err(error) => {
            run.facts.errors.push(operation_error(
                OperationStage::ProcessSupervision,
                format!("could not identify the created container process: {error:#}"),
                None,
            ));
            return None;
        }
    };
    match observe_owned_cgroup(init_pid, &prepared.expected_cgroup_path) {
        Ok(path) => run.runtime.cgroup_path = Some(path),
        Err(error) => {
            run.facts.errors.push(operation_error(
                OperationStage::ProcessSupervision,
                format!("could not prove ownership of runc's default cgroup: {error:#}"),
                None,
            ));
            return None;
        }
    }
    Some(init_pid)
}

fn establish_process_result_monitor(init_pid: u32, run: &mut ProgramRun) -> bool {
    match ProcExitMonitor::subscribe(init_pid) {
        Ok(monitor) => {
            run.runtime.exit_monitor = Some(monitor);
            true
        }
        Err(error) => {
            run.facts.errors.push(operation_error(
                OperationStage::ProcessSupervision,
                format!(
                    "could not establish reliable process-result monitoring before OCI start: {error:#}"
                ),
                None,
            ));
            false
        }
    }
}

fn start_deadline(
    start_monotonic: Instant,
    execution_start: Option<Instant>,
    execution_limit: Option<Duration>,
    timeouts: OperationTimeouts,
) -> (Instant, &'static str) {
    let execution_deadline = execution_start
        .unwrap_or(start_monotonic)
        .checked_add(execution_limit.unwrap_or(MAX_EXECUTION_TIMEOUT));
    let own_start_deadline =
        checked_deadline(start_monotonic, timeouts.start(), "runc start deadline")
            .expect("validated OperationTimeouts fit Instant");
    execution_deadline.map_or(
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
    )
}

fn start_report(result: AnyResult<HelperOutput>) -> OperationReport<StartFacts> {
    match result {
        Ok(output) if output.status.success() => {
            OperationReport::succeeded(StartFacts::new(wall_clock_now()))
        }
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
    }
}

enum CreateOutcome {
    Ready(ProgramRun),
    Finished(ProgramRun),
}

fn supervise_start(
    command: &mut Command,
    deadline: Instant,
    deadline_message: &'static str,
    cancellation: &CancellationToken,
    run: &mut ProgramRun,
    other_runs: &mut BTreeMap<ProgramId, ProgramRun>,
) -> AnyResult<HelperOutput> {
    let mut helper = RunningHelper::spawn(&run.supervision.owner, command)?;
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
                    run.supervision.unreaped = true;
                    return Err(error).context(format!(
                        "failed to terminate runc start after supervision error: {cleanup:#}"
                    ));
                }
                return Err(error);
            }
        }
        if cancellation.is_cancelled() {
            if let Err(error) = helper.terminate() {
                run.supervision.unreaped = true;
                return Err(error).context("failed to terminate cancelled runc start");
            }
            bail!("runc start interrupted by cancellation");
        }
        if Instant::now() >= deadline {
            if let Err(error) = helper.terminate() {
                run.supervision.unreaped = true;
                return Err(error).context("failed to terminate timed-out runc start");
            }
            bail!(deadline_message);
        }
        thread::sleep(POLL_INTERVAL);
    }
}
