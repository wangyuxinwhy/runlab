use chrono::{DateTime, FixedOffset};

use super::{Explanation, OperationError};
use crate::OutputError;

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

    pub(super) fn errors(&self) -> Box<dyn Iterator<Item = &OperationError> + '_> {
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
