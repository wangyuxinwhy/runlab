use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};

use crate::{ImageDescriptor, OutputError, ProgramId, RunInput};

/// Maximum retained bytes for each Program output stream.
pub const MAX_CAPTURED_STREAM_BYTES: usize = 100 * 1024 * 1024;

/// Non-empty explanation for a missing, unknown, or unattempted fact.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Explanation(String);

impl Explanation {
    /// Creates an explanation that carries actual diagnostic information.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when the explanation is empty or whitespace.
    pub fn new(text: impl Into<String>) -> Result<Self, OutputError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(OutputError::new(
                "reason",
                "an unavailable or unknown fact requires a non-empty reason",
            ));
        }
        Ok(Self(text))
    }

    #[must_use]
    /// Returns the diagnostic text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A value that is either directly available or explicitly unavailable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability<T> {
    /// The Engine directly obtained or proved the value.
    Available(T),
    /// The value could not be obtained, with an explicit reason.
    Unavailable(Explanation),
}

impl<T> Availability<T> {
    /// Wraps an available value.
    #[must_use]
    pub fn available(value: T) -> Self {
        Self::Available(value)
    }

    /// Creates an explicitly unavailable value.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unavailable(reason: impl Into<String>) -> Result<Self, OutputError> {
        Ok(Self::Unavailable(Explanation::new(reason)?))
    }

    #[must_use]
    /// Returns the available value, if one was obtained.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }

    #[must_use]
    /// Returns the reason when the value is unavailable.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable(reason) => Some(reason.as_str()),
        }
    }
}

/// Engine operation whose failure can be observed independently of a Program result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperationStage {
    /// Invocation or Program preparation before runtime creation.
    Preparation,
    /// OCI runtime `create`.
    Create,
    /// OCI runtime `start`.
    Start,
    /// Writing bytes to the Program's standard input.
    StdinWrite,
    /// Closing the standard-input write end.
    StdinClose,
    /// Draining the Program's standard output.
    StdoutRead,
    /// Draining the Program's standard error.
    StderrRead,
    /// Sending a termination signal.
    Signal,
    /// Waiting for the initial process result.
    Wait,
    /// Removing runtime mounts from the controlled filesystem.
    RuntimeFilesystemRemoval,
    /// Constructing the final OCI Image.
    FinalEnvironmentCapture,
    /// Removing temporary resources.
    Cleanup,
    /// Coordinating multiple Programs.
    Coordination,
    /// Measuring a deadline or bounded internal operation.
    Timing,
}

/// One directly observed Engine operation error.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OperationError {
    observed_at: DateTime<FixedOffset>,
    stage: OperationStage,
    message: Explanation,
    code: Option<i64>,
}

impl OperationError {
    /// Records one directly observed operation error.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `message` is empty or whitespace.
    pub fn new(
        observed_at: DateTime<FixedOffset>,
        stage: OperationStage,
        message: impl Into<String>,
        code: Option<i64>,
    ) -> Result<Self, OutputError> {
        Ok(Self {
            observed_at,
            stage,
            message: Explanation::new(message)?,
            code,
        })
    }

    #[must_use]
    /// Returns when the Engine observed the error.
    pub fn observed_at(&self) -> DateTime<FixedOffset> {
        self.observed_at
    }

    #[must_use]
    /// Returns the operation that produced the error.
    pub fn stage(&self) -> OperationStage {
        self.stage
    }

    #[must_use]
    /// Returns the underlying diagnostic message.
    pub fn message(&self) -> &str {
        self.message.as_str()
    }

    #[must_use]
    /// Returns an underlying numeric error code when one was reported.
    pub fn code(&self) -> Option<i64> {
        self.code
    }
}

/// Whether an Engine operation was attempted and what can be proven about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationStatus {
    /// The Engine proved that it did not attempt the operation.
    NotAttempted,
    /// The operation completed successfully.
    Succeeded,
    /// The operation returned a known failure.
    Failed,
    /// The Engine cannot prove the complete operation result.
    Unknown,
}

/// Facts and errors from one Engine operation without duplicating them in a
/// separate Program-wide error list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationReport<T> {
    status: OperationStatus,
    facts: Option<T>,
    reason: Option<Explanation>,
    errors: Box<[OperationError]>,
}

impl<T> OperationReport<T> {
    /// Records proof that an operation was not attempted.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn not_attempted(reason: impl Into<String>) -> Result<Self, OutputError> {
        Ok(Self {
            status: OperationStatus::NotAttempted,
            facts: None,
            reason: Some(Explanation::new(reason)?),
            errors: Box::new([]),
        })
    }

    #[must_use]
    /// Records a successful operation and its directly observed facts.
    pub fn succeeded(facts: T) -> Self {
        Self {
            status: OperationStatus::Succeeded,
            facts: Some(facts),
            reason: None,
            errors: Box::new([]),
        }
    }

    #[must_use]
    /// Records a known operation failure and one or more owning errors.
    pub fn failed(
        first_error: OperationError,
        additional_errors: impl IntoIterator<Item = OperationError>,
    ) -> Self {
        let errors = std::iter::once(first_error)
            .chain(additional_errors)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            status: OperationStatus::Failed,
            facts: None,
            reason: None,
            errors,
        }
    }

    /// Records an operation whose complete result cannot be proved.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown(
        reason: impl Into<String>,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        Ok(Self {
            status: OperationStatus::Unknown,
            facts: None,
            reason: Some(Explanation::new(reason)?),
            errors: errors.into_iter().collect::<Vec<_>>().into_boxed_slice(),
        })
    }

    #[must_use]
    /// Returns the four-state operation result.
    pub fn status(&self) -> OperationStatus {
        self.status
    }

    #[must_use]
    /// Returns facts retained for success or a supported partial transfer.
    pub fn facts(&self) -> Option<&T> {
        self.facts.as_ref()
    }

    #[must_use]
    /// Returns why an operation was not attempted or remains unknown.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_ref().map(Explanation::as_str)
    }

    /// Iterates errors owned by this operation.
    pub fn errors(&self) -> impl Iterator<Item = &OperationError> {
        self.errors.iter()
    }
}

impl OperationReport<StdinWriteFacts> {
    /// Records a failed stdin write while retaining the observed byte count.
    #[must_use]
    pub fn failed_with_facts(
        facts: StdinWriteFacts,
        first_error: OperationError,
        additional_errors: impl IntoIterator<Item = OperationError>,
    ) -> Self {
        let mut report = Self::failed(first_error, additional_errors);
        report.facts = Some(facts);
        report
    }

    /// Records an indeterminate stdin write while retaining an observed byte count.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown_with_facts(
        facts: StdinWriteFacts,
        reason: impl Into<String>,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        let mut report = Self::unknown(reason, errors)?;
        report.facts = Some(facts);
        Ok(report)
    }
}

impl OperationReport<StreamFacts> {
    /// Records a failed stream read while retaining bytes observed before failure.
    #[must_use]
    pub fn failed_with_facts(
        facts: StreamFacts,
        first_error: OperationError,
        additional_errors: impl IntoIterator<Item = OperationError>,
    ) -> Self {
        let mut report = Self::failed(first_error, additional_errors);
        report.facts = Some(facts);
        report
    }

    /// Records an indeterminate stream read while retaining already observed facts.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown_with_facts(
        facts: StreamFacts,
        reason: impl Into<String>,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        let mut report = Self::unknown(reason, errors)?;
        report.facts = Some(facts);
        Ok(report)
    }
}

/// Time at which OCI `create` completed successfully.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateFacts {
    completed_at: DateTime<FixedOffset>,
}

impl CreateFacts {
    /// Records the wall-clock observation made after successful `create`.
    #[must_use]
    pub fn new(completed_at: DateTime<FixedOffset>) -> Self {
        Self { completed_at }
    }

    #[must_use]
    /// Returns the wall-clock completion observation.
    pub fn completed_at(&self) -> DateTime<FixedOffset> {
        self.completed_at
    }
}

/// Time at which OCI `start` successfully started the user process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartFacts {
    started_at: DateTime<FixedOffset>,
}

impl StartFacts {
    /// Records the wall-clock observation made after successful `start`.
    #[must_use]
    pub fn new(started_at: DateTime<FixedOffset>) -> Self {
        Self { started_at }
    }

    #[must_use]
    /// Returns the wall-clock start observation.
    pub fn started_at(&self) -> DateTime<FixedOffset> {
        self.started_at
    }
}

/// Directly observed result of a Program's initial process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessResult {
    /// The Engine proved that the user process never started.
    NeverStarted {
        /// Evidence explaining why the process did not start.
        reason: Explanation,
    },
    /// The initial process exited with a numeric status.
    Exited {
        /// Exit status reported by the runtime.
        code: i32,
        /// Wall-clock observation made when the result was obtained.
        ended_at: DateTime<FixedOffset>,
    },
    /// The initial process ended because of a signal.
    Signaled {
        /// Nonzero signal number reported by the runtime.
        signal: NonZeroU32,
        /// Wall-clock observation made when the result was obtained.
        ended_at: DateTime<FixedOffset>,
    },
    /// The Engine cannot prove how the initial process ended.
    Unknown {
        /// Why the process result is indeterminate.
        reason: Explanation,
        /// Process-end observation, if independently available.
        ended_at: Availability<DateTime<FixedOffset>>,
    },
}

impl ProcessResult {
    /// Records proof that the user process never started.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn never_started(reason: impl Into<String>) -> Result<Self, OutputError> {
        Ok(Self::NeverStarted {
            reason: Explanation::new(reason)?,
        })
    }

    /// Records a process result that cannot be proved.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown(
        reason: impl Into<String>,
        ended_at: Availability<DateTime<FixedOffset>>,
    ) -> Result<Self, OutputError> {
        Ok(Self::Unknown {
            reason: Explanation::new(reason)?,
            ended_at,
        })
    }
}

/// Number of input bytes accepted by the Program's stdin pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StdinWriteFacts {
    bytes_written: u64,
}

impl StdinWriteFacts {
    /// Records the exact count accepted by the standard-input pipe.
    #[must_use]
    pub fn new(bytes_written: u64) -> Self {
        Self { bytes_written }
    }

    #[must_use]
    /// Returns the number of accepted input bytes.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// Separate reports for writing stdin and closing the write end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StdinOutput {
    write: OperationReport<StdinWriteFacts>,
    close: OperationReport<()>,
}

impl StdinOutput {
    /// Combines independent write and close operation reports.
    #[must_use]
    pub fn new(write: OperationReport<StdinWriteFacts>, close: OperationReport<()>) -> Self {
        Self { write, close }
    }

    #[must_use]
    /// Returns the standard-input write report.
    pub fn write(&self) -> &OperationReport<StdinWriteFacts> {
        &self.write
    }

    #[must_use]
    /// Returns the report for closing the write end.
    pub fn close(&self) -> &OperationReport<()> {
        &self.close
    }
}

/// Retained prefix and drain facts for one output stream.
#[derive(Clone, Eq, PartialEq)]
pub struct StreamFacts {
    bytes: Arc<[u8]>,
    omitted_after_limit: bool,
    eof: bool,
}

impl StreamFacts {
    /// Creates retained stream facts without allocating omitted bytes.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when retained bytes exceed the fixed limit or
    /// omission is claimed before that limit is reached.
    pub fn new(
        bytes: impl Into<Vec<u8>>,
        omitted_after_limit: bool,
        eof: bool,
    ) -> Result<Self, OutputError> {
        let bytes = bytes.into();
        validate_stream_shape(bytes.len(), omitted_after_limit)?;
        Ok(Self {
            bytes: Arc::from(bytes),
            omitted_after_limit,
            eof,
        })
    }

    #[must_use]
    /// Returns the retained prefix of raw stream bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    /// Returns whether bytes after the fixed limit were observed and omitted.
    pub fn omitted_after_limit(&self) -> bool {
        self.omitted_after_limit
    }

    #[must_use]
    /// Returns whether the Engine observed end-of-file.
    pub fn eof(&self) -> bool {
        self.eof
    }
}

fn validate_stream_shape(
    retained_bytes: usize,
    omitted_after_limit: bool,
) -> Result<(), OutputError> {
    if retained_bytes > MAX_CAPTURED_STREAM_BYTES {
        return Err(OutputError::new(
            "stream.bytes",
            format!("retained {retained_bytes} bytes; the maximum is {MAX_CAPTURED_STREAM_BYTES}"),
        ));
    }
    if omitted_after_limit && retained_bytes != MAX_CAPTURED_STREAM_BYTES {
        return Err(OutputError::new(
            "stream.omitted_after_limit",
            "omitted output requires retaining exactly the fixed stream limit",
        ));
    }
    Ok(())
}

impl std::fmt::Debug for StreamFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamFacts")
            .field("byte_len", &self.bytes.len())
            .field("omitted_after_limit", &self.omitted_after_limit)
            .field("eof", &self.eof)
            .finish()
    }
}

/// Signal used for one bounded-stop attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopSignal {
    /// Graceful `SIGTERM` request.
    Term,
    /// Forced `SIGKILL` request after the shared grace period.
    Kill,
}

/// Runtime result of one attempted stop signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StopActionResult {
    /// The runtime accepted the signal operation.
    Accepted,
    /// The runtime returned a known signal error.
    Rejected(OperationError),
    /// Signal acceptance could not be proved.
    Unknown {
        /// Why acceptance is indeterminate.
        reason: Explanation,
        /// Signal-operation errors observed while determining the result.
        errors: Box<[OperationError]>,
    },
}

impl StopActionResult {
    /// Records a stop action whose acceptance cannot be proved.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown(
        reason: impl Into<String>,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        Ok(Self::Unknown {
            reason: Explanation::new(reason)?,
            errors: errors.into_iter().collect::<Vec<_>>().into_boxed_slice(),
        })
    }

    fn errors(&self) -> Box<dyn Iterator<Item = &OperationError> + '_> {
        match self {
            Self::Accepted => Box::new(std::iter::empty()),
            Self::Rejected(error) => Box::new(std::iter::once(error)),
            Self::Unknown { errors, .. } => Box::new(errors.iter()),
        }
    }
}

/// One actual attempt to stop a Program, retained in attempt order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopAction {
    signal: StopSignal,
    attempted_at: DateTime<FixedOffset>,
    result: StopActionResult,
}

impl StopAction {
    /// Records one signal attempt and its runtime result.
    #[must_use]
    pub fn new(
        signal: StopSignal,
        attempted_at: DateTime<FixedOffset>,
        result: StopActionResult,
    ) -> Self {
        Self {
            signal,
            attempted_at,
            result,
        }
    }

    #[must_use]
    /// Returns the attempted signal.
    pub fn signal(&self) -> StopSignal {
        self.signal
    }

    #[must_use]
    /// Returns the wall-clock observation made for the attempt.
    pub fn attempted_at(&self) -> DateTime<FixedOffset> {
        self.attempted_at
    }

    #[must_use]
    /// Returns the observed runtime result.
    pub fn result(&self) -> &StopActionResult {
        &self.result
    }
}

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
            .chain(self.stdin.write.errors())
            .chain(self.stdin.close.errors())
            .chain(self.stdout.errors())
            .chain(self.stderr.errors())
            .chain(
                self.stop_actions
                    .iter()
                    .flat_map(|action| action.result.errors()),
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
    if let Some(write) = output.stdin.write.facts() {
        if write.bytes_written() > supplied {
            return Err(OutputError::new(
                format!("{path}.stdin.write.bytes_written"),
                "written byte count exceeds the supplied stdin length",
            ));
        }
        if output.stdin.write.status() == OperationStatus::Succeeded
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
        &output.stdin.write,
        OperationStage::StdinWrite,
        true,
        &format!("{path}.stdin.write"),
    )?;
    validate_report(
        &output.stdin.close,
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
            .result
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use chrono::DateTime;
    use oci_spec::image::Descriptor;

    use super::*;
    use crate::{Network, ProgramInput, RuntimeConfig};

    fn at(second: u32) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(&format!("2026-08-25T12:00:{second:02}+08:00"))
            .expect("timestamp")
    }

    fn image(byte: char) -> ImageDescriptor {
        let digest = byte.to_string().repeat(64);
        let descriptor: Descriptor = serde_json::from_value(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{digest}"),
            "size": 123
        }))
        .expect("OCI Descriptor");
        ImageDescriptor::new(descriptor).expect("Image Manifest")
    }

    fn runtime() -> RuntimeConfig {
        RuntimeConfig::parse(
            serde_json::to_vec(&serde_json::json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "terminal": false,
                    "args": ["/bin/true"],
                    "cwd": "/",
                    "user": {"uid": 0, "gid": 0}
                },
                "linux": {}
            }))
            .expect("runtime JSON"),
        )
        .expect("RuntimeConfig")
    }

    fn input(program_ids: &[&str]) -> RunInput {
        let programs = program_ids
            .iter()
            .map(|program_id| {
                (
                    ProgramId::new(*program_id),
                    ProgramInput::new(image('a'), runtime(), Vec::new()).expect("ProgramInput"),
                )
            })
            .collect();
        RunInput::new(programs, NonZeroU64::new(1000), Network::Isolated).expect("RunInput")
    }

    fn error(second: u32, stage: OperationStage, message: &str) -> OperationError {
        OperationError::new(at(second), stage, message, None).expect("OperationError")
    }

    fn simple_program() -> ProgramOutput {
        program_with_lifecycle(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            OperationReport::succeeded(StartFacts::new(at(2))),
            ProcessResult::Exited {
                code: 0,
                ended_at: at(3),
            },
        )
        .expect("valid ProgramOutput")
    }

    fn program_with_lifecycle(
        create: OperationReport<CreateFacts>,
        start: OperationReport<StartFacts>,
        process: ProcessResult,
    ) -> Result<ProgramOutput, OutputError> {
        ProgramOutput::new(
            create,
            start,
            process,
            StdinOutput::new(
                OperationReport::succeeded(StdinWriteFacts::new(0)),
                OperationReport::succeeded(()),
            ),
            OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stdout")),
            OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
            [],
            Availability::available(image('b')),
            [],
        )
    }

    #[test]
    fn operation_report_constructors_keep_four_states_distinct() {
        let not_attempted =
            OperationReport::<CreateFacts>::not_attempted("dependency create failed")
                .expect("not attempted");
        assert_eq!(not_attempted.status(), OperationStatus::NotAttempted);
        assert!(not_attempted.facts().is_none());
        assert_eq!(not_attempted.errors().count(), 0);

        let succeeded = OperationReport::succeeded(CreateFacts::new(at(1)));
        assert_eq!(succeeded.status(), OperationStatus::Succeeded);
        assert!(succeeded.facts().is_some());

        let failed = OperationReport::<CreateFacts>::failed(
            error(2, OperationStage::Create, "runtime rejected create"),
            [],
        );
        assert_eq!(failed.status(), OperationStatus::Failed);
        assert!(failed.facts().is_none());
        assert_eq!(failed.errors().count(), 1);

        let unknown = OperationReport::<CreateFacts>::unknown(
            "runtime result was not observable",
            [error(2, OperationStage::Create, "inspect timed out")],
        )
        .expect("unknown");
        assert_eq!(unknown.status(), OperationStatus::Unknown);
        assert_eq!(unknown.errors().count(), 1);

        assert!(OperationReport::<()>::not_attempted(" ").is_err());
        assert!(OperationReport::<()>::unknown("", []).is_err());
    }

    #[test]
    fn unavailable_and_unknown_require_reasons() {
        assert!(Availability::<()>::unavailable("").is_err());
        assert!(ProcessResult::never_started(" ").is_err());
        assert!(
            ProcessResult::unknown(
                "",
                Availability::unavailable("end time not observed").expect("unavailable"),
            )
            .is_err()
        );

        let never_started =
            ProcessResult::never_started("OCI start was not attempted").expect("known fact");
        let unknown = ProcessResult::unknown(
            "wait result unavailable",
            Availability::unavailable("end time unavailable").expect("unavailable"),
        )
        .expect("unknown fact");
        assert_ne!(never_started, unknown);
    }

    #[test]
    fn stream_shape_enforces_the_fixed_limit_without_guessing_truncation() {
        assert!(validate_stream_shape(0, false).is_ok());
        assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES, false).is_ok());
        assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES, true).is_ok());
        assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES - 1, true).is_err());
        assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES + 1, false).is_err());

        let partial = StreamFacts::new(b"partial".to_vec(), false, false).expect("partial stream");
        let report = OperationReport::<StreamFacts>::failed_with_facts(
            partial,
            error(3, OperationStage::StdoutRead, "read failed"),
            [],
        );
        assert_eq!(report.facts().expect("partial facts").bytes(), b"partial");
    }

    #[test]
    fn program_errors_are_aggregated_once_from_their_owning_operations() {
        let program = ProgramOutput::new(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            OperationReport::unknown(
                "start result unavailable",
                [error(2, OperationStage::Start, "start")],
            )
            .expect("unknown start"),
            ProcessResult::unknown(
                "process result unavailable",
                Availability::unavailable("end time unavailable").expect("unavailable end"),
            )
            .expect("unknown process"),
            StdinOutput::new(
                OperationReport::<StdinWriteFacts>::failed_with_facts(
                    StdinWriteFacts::new(2),
                    error(2, OperationStage::StdinWrite, "stdin"),
                    [],
                ),
                OperationReport::succeeded(()),
            ),
            OperationReport::<StreamFacts>::failed_with_facts(
                StreamFacts::new(b"x".to_vec(), false, false).expect("partial stdout"),
                error(3, OperationStage::StdoutRead, "stdout"),
                [],
            ),
            OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
            [StopAction::new(
                StopSignal::Term,
                at(4),
                StopActionResult::Rejected(error(4, OperationStage::Signal, "signal")),
            )],
            Availability::unavailable("rootfs never became stable")
                .expect("unavailable final environment"),
            [error(5, OperationStage::Cleanup, "cleanup")],
        )
        .expect("valid ProgramOutput");

        let messages = program
            .errors()
            .map(OperationError::message)
            .collect::<Vec<_>>();
        assert_eq!(messages, ["start", "stdin", "stdout", "signal", "cleanup"]);
    }

    #[test]
    fn output_program_keys_must_exactly_match_input() {
        let input = input(&["dependency", "primary"]);
        let execution =
            ExecutionOutput::new(ExecutionInterval::entered(at(1), at(3)), false, false, [])
                .expect("execution");

        let mut missing = BTreeMap::new();
        missing.insert(ProgramId::primary(), simple_program());
        assert!(RunOutput::new(&input, execution.clone(), missing).is_err());

        let mut exact = BTreeMap::new();
        exact.insert(ProgramId::new("dependency"), simple_program());
        exact.insert(ProgramId::primary(), simple_program());
        RunOutput::new(&input, execution, exact).expect("matching output");
    }

    #[test]
    fn execution_times_and_termination_cause_are_coherent() {
        let not_entered =
            ExecutionInterval::not_entered("cancelled before start").expect("not-entered interval");
        ExecutionOutput::new(not_entered.clone(), false, true, []).expect("cancelled before start");

        ExecutionOutput::new(ExecutionInterval::entered(at(2), at(1)), false, false, [])
            .expect("wall clock observations may move backwards");
        assert!(ExecutionOutput::new(not_entered, true, false, []).is_err());
        assert!(
            ExecutionOutput::new(ExecutionInterval::entered(at(1), at(2)), true, true, [],)
                .is_err()
        );
    }

    #[test]
    fn primary_cannot_start_until_every_dependency_started_successfully() {
        let cases = [
            program_with_lifecycle(
                OperationReport::failed(error(1, OperationStage::Create, "create failed"), []),
                OperationReport::not_attempted("create failed").expect("not attempted"),
                ProcessResult::never_started("start was not attempted").expect("never started"),
            )
            .expect("not-attempted dependency"),
            program_with_lifecycle(
                OperationReport::succeeded(CreateFacts::new(at(1))),
                OperationReport::failed(error(2, OperationStage::Start, "start failed"), []),
                ProcessResult::never_started("start failed").expect("never started"),
            )
            .expect("failed dependency"),
            program_with_lifecycle(
                OperationReport::succeeded(CreateFacts::new(at(1))),
                OperationReport::unknown(
                    "start result unavailable",
                    [error(2, OperationStage::Start, "start unavailable")],
                )
                .expect("unknown start"),
                ProcessResult::unknown(
                    "process state unavailable",
                    Availability::unavailable("end unavailable").expect("unavailable end"),
                )
                .expect("unknown process"),
            )
            .expect("unknown dependency"),
        ];

        for dependency in cases {
            let input = input(&["dependency", "primary"]);
            let execution =
                ExecutionOutput::new(ExecutionInterval::entered(at(1), at(3)), false, false, [])
                    .expect("execution");
            let programs = BTreeMap::from([
                (ProgramId::new("dependency"), dependency),
                (ProgramId::primary(), simple_program()),
            ]);

            let error = RunOutput::new(&input, execution, programs)
                .expect_err("primary start must be rejected");
            assert_eq!(error.path(), "programs[\"primary\"].start");
        }
    }

    #[test]
    fn program_constructor_rejects_stream_success_without_eof() {
        let error = ProgramOutput::new(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            OperationReport::succeeded(StartFacts::new(at(2))),
            ProcessResult::Exited {
                code: 0,
                ended_at: at(3),
            },
            StdinOutput::new(
                OperationReport::succeeded(StdinWriteFacts::new(0)),
                OperationReport::succeeded(()),
            ),
            OperationReport::succeeded(
                StreamFacts::new(b"partial".to_vec(), false, false).expect("stream facts"),
            ),
            OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
            [],
            Availability::available(image('b')),
            [],
        )
        .expect_err("success requires EOF");

        assert_eq!(error.path(), "program.stdout");
    }

    #[test]
    fn program_constructor_enforces_error_ownership_without_value_deduplication() {
        let misplaced = program_with_lifecycle(
            OperationReport::failed(error(1, OperationStage::StderrRead, "wrong owner"), []),
            OperationReport::not_attempted("create failed").expect("not attempted"),
            ProcessResult::never_started("start was not attempted").expect("never started"),
        )
        .expect_err("create cannot own stderr errors");
        assert_eq!(misplaced.path(), "program.create");

        let duplicate = error(4, OperationStage::Cleanup, "cleanup failed");
        let base = simple_program();
        let repeated_observations = ProgramOutput::new(
            base.create.clone(),
            base.start.clone(),
            base.process.clone(),
            base.stdin.clone(),
            base.stdout.clone(),
            base.stderr.clone(),
            [],
            base.final_environment.clone(),
            [duplicate.clone(), duplicate],
        )
        .expect("equal values may represent separate observations");
        assert_eq!(repeated_observations.errors().count(), 2);
    }

    #[test]
    fn failed_or_unknown_start_does_not_override_later_process_evidence() {
        for start in [
            OperationReport::failed(error(2, OperationStage::Start, "runtime failed"), []),
            OperationReport::unknown(
                "runtime result unavailable",
                [error(2, OperationStage::Start, "runtime unavailable")],
            )
            .expect("unknown start"),
        ] {
            program_with_lifecycle(
                OperationReport::succeeded(CreateFacts::new(at(1))),
                start,
                ProcessResult::Exited {
                    code: 17,
                    ended_at: at(3),
                },
            )
            .expect("direct process evidence survives start-operation uncertainty");
        }
    }

    #[test]
    fn bounded_stop_never_starts_with_kill() {
        let base = simple_program();
        let error = ProgramOutput::new(
            base.create,
            base.start,
            base.process,
            base.stdin,
            base.stdout,
            base.stderr,
            [StopAction::new(
                StopSignal::Kill,
                at(4),
                StopActionResult::Accepted,
            )],
            base.final_environment,
            [],
        )
        .expect_err("TERM must be attempted first");

        assert_eq!(error.path(), "program.stop_actions");
    }

    #[test]
    fn execution_scope_rejects_program_operation_errors() {
        let error = ExecutionOutput::new(
            ExecutionInterval::entered(at(1), at(2)),
            false,
            false,
            [error(1, OperationStage::Create, "misplaced create error")],
        )
        .expect_err("create error belongs to a Program");

        assert_eq!(error.path(), "execution.errors");
    }
}
