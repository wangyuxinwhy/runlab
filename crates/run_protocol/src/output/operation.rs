use chrono::{DateTime, FixedOffset};

use crate::OutputError;

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
    /// Establishing the identity and containment needed to control a created process.
    ProcessSupervision,
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

    pub(super) fn with_facts(mut self, facts: T) -> Self {
        self.facts = Some(facts);
        self
    }
}
