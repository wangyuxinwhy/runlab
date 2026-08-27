use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};

use super::{
    Availability, CreateFacts, Explanation, OperationError, OperationReport, OperationStage,
    OperationStatus, ProcessResult, StartFacts, StdinOutput, StopAction, StopSignal, StreamFacts,
};
use crate::{ImageDescriptor, OutputError, ProgramId, RunInput};

/// Whether the invocation entered the monotonic-clock execution interval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionInterval {
    /// No Program reached an OCI `start` attempt.
    NotEntered {
        /// Why no Program reached a `start` attempt.
        reason: Explanation,
    },
    /// The interval was entered and both wall-clock observation points were retained.
    Entered {
        /// Wall-clock observation made when the first `start` was about to be attempted.
        started_at: DateTime<FixedOffset>,
        /// Wall-clock observation made when execution entered termination.
        ended_at: DateTime<FixedOffset>,
    },
}

impl ExecutionInterval {
    /// Records that execution never reached its first `start` attempt.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn not_entered(reason: impl Into<String>) -> Result<Self, OutputError> {
        Ok(Self::NotEntered {
            reason: Explanation::new(reason)?,
        })
    }

    /// Records both wall-clock observation points for an entered execution interval.
    ///
    /// The values are observations, not a duration measurement, and may move
    /// backwards if the wall clock is adjusted during execution.
    #[must_use]
    pub fn entered(started_at: DateTime<FixedOffset>, ended_at: DateTime<FixedOffset>) -> Self {
        Self::Entered {
            started_at,
            ended_at,
        }
    }

    /// Returns whether the monotonic-clock execution interval was entered.
    #[must_use]
    pub fn was_entered(&self) -> bool {
        matches!(self, Self::Entered { .. })
    }
}

/// Facts that apply to the complete Engine invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutput {
    interval: ExecutionInterval,
    timed_out: bool,
    cancelled: bool,
    errors: Box<[OperationError]>,
}

impl ExecutionOutput {
    /// Creates invocation-wide timing and termination facts.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] for contradictory termination causes, timeout
    /// without an entered interval, or errors stored at the wrong scope.
    pub fn new(
        interval: ExecutionInterval,
        timed_out: bool,
        cancelled: bool,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        if timed_out && cancelled {
            return Err(OutputError::new(
                "execution",
                "timeout and cancellation cannot both be the condition that entered termination",
            ));
        }
        if timed_out && !interval.was_entered() {
            return Err(OutputError::new(
                "execution.timed_out",
                "an execution deadline cannot expire before the execution interval is entered",
            ));
        }
        let allowed = [
            OperationStage::Preparation,
            OperationStage::Cleanup,
            OperationStage::Coordination,
            OperationStage::Timing,
        ];
        let errors = errors.into_iter().collect::<Vec<_>>().into_boxed_slice();
        if errors.iter().any(|error| !allowed.contains(&error.stage())) {
            return Err(OutputError::new(
                "execution.errors",
                "Program-scoped operation error stored at execution scope",
            ));
        }
        Ok(Self {
            interval,
            timed_out,
            cancelled,
            errors,
        })
    }

    /// Returns the execution interval state and its observation points.
    #[must_use]
    pub fn interval(&self) -> &ExecutionInterval {
        &self.interval
    }

    #[must_use]
    /// Returns whether the execution deadline caused termination.
    pub fn timed_out(&self) -> bool {
        self.timed_out
    }

    #[must_use]
    /// Returns whether caller cancellation caused termination.
    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Iterates invocation-scoped operation errors.
    pub fn errors(&self) -> impl Iterator<Item = &OperationError> {
        self.errors.iter()
    }
}

/// All facts collected for one Program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramOutput {
    create: OperationReport<CreateFacts>,
    start: OperationReport<StartFacts>,
    process: ProcessResult,
    stdin: StdinOutput,
    stdout: OperationReport<StreamFacts>,
    stderr: OperationReport<StreamFacts>,
    stop_actions: Box<[StopAction]>,
    final_environment: Availability<ImageDescriptor>,
    additional_errors: Box<[OperationError]>,
}

impl ProgramOutput {
    #[allow(
        clippy::too_many_arguments,
        reason = "each argument is a distinct protocol fact"
    )]
    /// Constructs a Program result after validating operation ownership and state coherence.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when operation reports, process facts, stop actions,
    /// or error ownership contradict one another.
    pub fn new(
        create: OperationReport<CreateFacts>,
        start: OperationReport<StartFacts>,
        process: ProcessResult,
        stdin: StdinOutput,
        stdout: OperationReport<StreamFacts>,
        stderr: OperationReport<StreamFacts>,
        stop_actions: impl IntoIterator<Item = StopAction>,
        final_environment: Availability<ImageDescriptor>,
        additional_errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        let output = Self {
            create,
            start,
            process,
            stdin,
            stdout,
            stderr,
            stop_actions: stop_actions
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            final_environment,
            additional_errors: additional_errors
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        };
        validate_program_shape(&output, "program")?;
        Ok(output)
    }

    #[must_use]
    /// Returns the OCI `create` operation report.
    pub fn create(&self) -> &OperationReport<CreateFacts> {
        &self.create
    }

    #[must_use]
    /// Returns the OCI `start` operation report.
    pub fn start(&self) -> &OperationReport<StartFacts> {
        &self.start
    }

    #[must_use]
    /// Returns the initial process result.
    pub fn process(&self) -> &ProcessResult {
        &self.process
    }

    #[must_use]
    /// Returns standard-input transfer results.
    pub fn stdin(&self) -> &StdinOutput {
        &self.stdin
    }

    #[must_use]
    /// Returns standard-output capture results.
    pub fn stdout(&self) -> &OperationReport<StreamFacts> {
        &self.stdout
    }

    #[must_use]
    /// Returns standard-error capture results.
    pub fn stderr(&self) -> &OperationReport<StreamFacts> {
        &self.stderr
    }

    #[must_use]
    /// Returns actual bounded-stop attempts in attempt order.
    pub fn stop_actions(&self) -> &[StopAction] {
        &self.stop_actions
    }

    #[must_use]
    /// Returns the final OCI Image descriptor or its unavailability reason.
    pub fn final_environment(&self) -> &Availability<ImageDescriptor> {
        &self.final_environment
    }

    /// Iterates every Program-scoped operation error exactly once.
    pub fn errors(&self) -> impl Iterator<Item = &OperationError> {
        self.create
            .errors()
            .chain(self.start.errors())
            .chain(self.stdin.write().errors())
            .chain(self.stdin.close().errors())
            .chain(self.stdout.errors())
            .chain(self.stderr.errors())
            .chain(
                self.stop_actions
                    .iter()
                    .flat_map(|action| action.result().errors()),
            )
            .chain(self.additional_errors.iter())
    }
}

/// Complete facts returned by one Run Engine invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOutput {
    execution: ExecutionOutput,
    programs: BTreeMap<ProgramId, ProgramOutput>,
}

impl RunOutput {
    /// Builds an output only when it has exactly one slot for every input Program.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] unless input and output Program key sets match.
    pub fn new(
        input: &RunInput,
        execution: ExecutionOutput,
        programs: BTreeMap<ProgramId, ProgramOutput>,
    ) -> Result<Self, OutputError> {
        let expected = input.programs().keys().collect::<Vec<_>>();
        let actual = programs.keys().collect::<Vec<_>>();
        if actual != expected {
            return Err(OutputError::new(
                "programs",
                "output Program keys must exactly match the RunInput Program keys",
            ));
        }
        for (program_id, output) in &programs {
            let input_program = &input.programs()[program_id];
            let path = format!("programs[{:?}]", program_id.as_str());
            validate_program(input_program, output, &path)?;
        }
        validate_execution(input, &execution, &programs)?;
        validate_dependency_start_order(&programs)?;
        Ok(Self {
            execution,
            programs,
        })
    }

    #[must_use]
    /// Returns invocation-wide facts.
    pub fn execution(&self) -> &ExecutionOutput {
        &self.execution
    }

    #[must_use]
    /// Returns exactly one result for each input Program.
    pub fn programs(&self) -> &BTreeMap<ProgramId, ProgramOutput> {
        &self.programs
    }
}

fn validate_program(
    input: &crate::ProgramInput,
    output: &ProgramOutput,
    path: &str,
) -> Result<(), OutputError> {
    validate_program_shape(output, path)?;

    let supplied = input.stdin().len() as u64;
    if let Some(write) = output.stdin.write().facts() {
        if write.bytes_written() > supplied {
            return Err(OutputError::new(
                format!("{path}.stdin.write.bytes_written"),
                "written byte count exceeds the supplied stdin length",
            ));
        }
        if output.stdin.write().status() == OperationStatus::Succeeded
            && write.bytes_written() != supplied
        {
            return Err(OutputError::new(
                format!("{path}.stdin.write.bytes_written"),
                "successful stdin write must include every supplied byte",
            ));
        }
    }
    Ok(())
}

fn validate_program_shape(output: &ProgramOutput, path: &str) -> Result<(), OutputError> {
    validate_report(
        &output.create,
        OperationStage::Create,
        false,
        &format!("{path}.create"),
    )?;
    validate_report(
        &output.start,
        OperationStage::Start,
        false,
        &format!("{path}.start"),
    )?;
    validate_report(
        output.stdin.write(),
        OperationStage::StdinWrite,
        true,
        &format!("{path}.stdin.write"),
    )?;
    validate_report(
        output.stdin.close(),
        OperationStage::StdinClose,
        false,
        &format!("{path}.stdin.close"),
    )?;
    validate_stream_report(
        &output.stdout,
        OperationStage::StdoutRead,
        &format!("{path}.stdout"),
    )?;
    validate_stream_report(
        &output.stderr,
        OperationStage::StderrRead,
        &format!("{path}.stderr"),
    )?;

    if output.create.status() != OperationStatus::Succeeded
        && output.start.status() != OperationStatus::NotAttempted
    {
        return Err(OutputError::new(
            format!("{path}.start"),
            "start cannot be attempted unless create succeeded",
        ));
    }
    match (output.start.status(), output.process()) {
        (OperationStatus::Succeeded, ProcessResult::NeverStarted { .. }) => {
            return Err(OutputError::new(
                format!("{path}.process"),
                "a successfully started process cannot be marked never started",
            ));
        }
        (OperationStatus::NotAttempted, ProcessResult::NeverStarted { .. })
        | (
            OperationStatus::Succeeded,
            ProcessResult::Exited { .. }
            | ProcessResult::Signaled { .. }
            | ProcessResult::Unknown { .. },
        )
        | (
            OperationStatus::Failed | OperationStatus::Unknown,
            ProcessResult::NeverStarted { .. }
            | ProcessResult::Exited { .. }
            | ProcessResult::Signaled { .. }
            | ProcessResult::Unknown { .. },
        ) => {}
        _ => {
            return Err(OutputError::new(
                format!("{path}.process"),
                "process result contradicts the observed start status",
            ));
        }
    }

    validate_stop_actions(output, path)?;

    let allowed_additional = [
        OperationStage::Preparation,
        OperationStage::ProcessSupervision,
        OperationStage::Wait,
        OperationStage::RuntimeFilesystemRemoval,
        OperationStage::FinalEnvironmentCapture,
        OperationStage::Cleanup,
    ];
    if let Some(error) = output
        .additional_errors
        .iter()
        .find(|error| !allowed_additional.contains(&error.stage()))
    {
        return Err(OutputError::new(
            format!("{path}.errors"),
            format!(
                "{:?} errors belong to a dedicated operation or execution scope",
                error.stage()
            ),
        ));
    }

    Ok(())
}

fn validate_report<T>(
    report: &OperationReport<T>,
    stage: OperationStage,
    partial_facts_allowed: bool,
    path: &str,
) -> Result<(), OutputError> {
    if report.errors().any(|error| error.stage() != stage) {
        return Err(OutputError::new(
            path,
            format!("operation report may contain only {stage:?} errors"),
        ));
    }
    if !partial_facts_allowed
        && report.status() != OperationStatus::Succeeded
        && report.facts().is_some()
    {
        return Err(OutputError::new(
            path,
            "partial facts are not meaningful for this operation",
        ));
    }
    Ok(())
}

fn validate_stream_report(
    report: &OperationReport<StreamFacts>,
    stage: OperationStage,
    path: &str,
) -> Result<(), OutputError> {
    validate_report(report, stage, true, path)?;
    if report.status() == OperationStatus::Succeeded
        && report.facts().is_some_and(|facts| !facts.eof())
    {
        return Err(OutputError::new(
            path,
            "a successful stream report must include an observed EOF",
        ));
    }
    Ok(())
}

fn validate_stop_actions(output: &ProgramOutput, path: &str) -> Result<(), OutputError> {
    if matches!(output.process(), ProcessResult::NeverStarted { .. })
        && !output.stop_actions.is_empty()
    {
        return Err(OutputError::new(
            format!("{path}.stop_actions"),
            "a Program known never to have started cannot have stop actions",
        ));
    }
    let signals = output
        .stop_actions
        .iter()
        .map(StopAction::signal)
        .collect::<Vec<_>>();
    if !matches!(
        signals.as_slice(),
        [] | [StopSignal::Term] | [StopSignal::Term, StopSignal::Kill]
    ) {
        return Err(OutputError::new(
            format!("{path}.stop_actions"),
            "stop actions must contain TERM followed by at most one KILL",
        ));
    }
    for action in &output.stop_actions {
        if action
            .result()
            .errors()
            .any(|error| error.stage() != OperationStage::Signal)
        {
            return Err(OutputError::new(
                format!("{path}.stop_actions"),
                "stop action results may contain only signal errors",
            ));
        }
    }
    Ok(())
}

fn validate_execution(
    input: &RunInput,
    execution: &ExecutionOutput,
    programs: &BTreeMap<ProgramId, ProgramOutput>,
) -> Result<(), OutputError> {
    let start_attempted = programs
        .values()
        .any(|program| program.start.status() != OperationStatus::NotAttempted);
    if start_attempted != execution.interval.was_entered() {
        return Err(OutputError::new(
            "execution",
            "execution interval timestamps must be available exactly when a start was attempted",
        ));
    }
    if execution.timed_out() && input.execution_timeout_ms().is_none() {
        return Err(OutputError::new(
            "execution.timed_out",
            "execution cannot time out when the input has no execution deadline",
        ));
    }
    Ok(())
}

fn validate_dependency_start_order(
    programs: &BTreeMap<ProgramId, ProgramOutput>,
) -> Result<(), OutputError> {
    let primary = &programs[&ProgramId::primary()];
    if primary.start.status() == OperationStatus::NotAttempted {
        return Ok(());
    }
    for (program_id, dependency) in programs {
        if program_id.is_primary() {
            continue;
        }
        if dependency.create.status() != OperationStatus::Succeeded
            || dependency.start.status() != OperationStatus::Succeeded
        {
            return Err(OutputError::new(
                "programs[\"primary\"].start",
                format!(
                    "primary start was attempted before dependency {program_id:?} had successfully started"
                ),
            ));
        }
    }
    Ok(())
}
