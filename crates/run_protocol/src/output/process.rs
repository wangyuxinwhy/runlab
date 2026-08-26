use std::num::NonZeroU32;

use chrono::{DateTime, FixedOffset};

use super::{Availability, Explanation};
use crate::OutputError;

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
