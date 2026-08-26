use std::time::Duration;

use thiserror::Error;

/// Protocol-wide grace period beginning with the first attempt to deliver `SIGTERM`.
///
/// A runtime helper's own deadline runs concurrently with this interval.
/// Waiting for that helper must not postpone the point at which `SIGKILL`
/// becomes eligible.
pub const STOP_GRACE_PERIOD: Duration = Duration::from_secs(10);

/// Largest configurable deadline for one Engine-owned operation.
pub const MAX_OPERATION_TIMEOUT: Duration = Duration::from_hours(24);

/// An invalid finite deadline supplied while configuring an Engine.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid {operation} timeout: deadlines must be between 1 ms and {max_milliseconds} ms")]
pub struct OperationTimeoutError {
    operation: &'static str,
    max_milliseconds: u128,
}

impl OperationTimeoutError {
    /// Returns the Engine operation whose deadline was invalid.
    #[must_use]
    pub fn operation(&self) -> &'static str {
        self.operation
    }
}

/// Fixed finite deadlines for Engine-owned operations.
///
/// These deadlines bound execution mechanics outside the caller-controlled
/// execution interval. An Engine stores one value at construction and must not
/// silently vary it between invocations. Start with [`Self::default`] and use
/// the checked `with_*` methods when an environment needs different bounds.
///
/// Preparation and cleanup each bound one invocation-wide stage. Create,
/// start, signal, wait, forced-stop confirmation, filesystem removal, and final
/// capture apply to one Program operation. Stream drain applies independently
/// to each output stream. The finite Program-count capability therefore also
/// bounds the total number of operation deadlines in one invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationTimeouts {
    preparation: Duration,
    create: Duration,
    start: Duration,
    term_signal: Duration,
    kill_signal: Duration,
    wait: Duration,
    forced_stop_confirmation: Duration,
    stream_drain: Duration,
    runtime_filesystem_removal: Duration,
    final_environment_capture: Duration,
    cleanup: Duration,
}

macro_rules! timeout_accessors {
    ($(($getter:ident, $setter:ident, $label:literal)),+ $(,)?) => {
        $(
            #[doc = concat!("Returns the finite deadline for ", $label, ".")]
            #[must_use]
            pub fn $getter(self) -> Duration {
                self.$getter
            }

            #[doc = concat!("Sets the finite deadline for ", $label, ".")]
            ///
            /// # Errors
            ///
            /// Returns [`OperationTimeoutError`] when `timeout` is shorter than
            /// one millisecond or longer than [`MAX_OPERATION_TIMEOUT`].
            pub fn $setter(mut self, timeout: Duration) -> Result<Self, OperationTimeoutError> {
                validate_timeout(stringify!($getter), timeout)?;
                self.$getter = timeout;
                Ok(self)
            }
        )+
    };
}

impl OperationTimeouts {
    timeout_accessors!(
        (
            preparation,
            with_preparation,
            "input and resource preparation"
        ),
        (create, with_create, "the runtime create operation"),
        (start, with_start, "the runtime start operation"),
        (
            term_signal,
            with_term_signal,
            "the `SIGTERM` runtime operation"
        ),
        (
            kill_signal,
            with_kill_signal,
            "the `SIGKILL` runtime operation"
        ),
        (wait, with_wait, "post-termination process waiting"),
        (
            forced_stop_confirmation,
            with_forced_stop_confirmation,
            "forced-stop confirmation"
        ),
        (stream_drain, with_stream_drain, "output stream draining"),
        (
            runtime_filesystem_removal,
            with_runtime_filesystem_removal,
            "runtime filesystem removal"
        ),
        (
            final_environment_capture,
            with_final_environment_capture,
            "final environment capture"
        ),
        (cleanup, with_cleanup, "invocation resource cleanup"),
    );
}

/// Uses 30 minutes for preparation and final capture, two minutes for each
/// runtime, signal, wait, stream-drain, filesystem-removal, and cleanup
/// operation, and 30 seconds for forced-stop confirmation.
impl Default for OperationTimeouts {
    fn default() -> Self {
        Self {
            preparation: Duration::from_mins(30),
            create: Duration::from_mins(2),
            start: Duration::from_mins(2),
            term_signal: Duration::from_mins(2),
            kill_signal: Duration::from_mins(2),
            wait: Duration::from_mins(2),
            forced_stop_confirmation: Duration::from_secs(30),
            stream_drain: Duration::from_mins(2),
            runtime_filesystem_removal: Duration::from_mins(2),
            final_environment_capture: Duration::from_mins(30),
            cleanup: Duration::from_mins(2),
        }
    }
}

fn validate_timeout(
    operation: &'static str,
    timeout: Duration,
) -> Result<(), OperationTimeoutError> {
    if timeout < Duration::from_millis(1) || timeout > MAX_OPERATION_TIMEOUT {
        return Err(OperationTimeoutError {
            operation,
            max_milliseconds: MAX_OPERATION_TIMEOUT.as_millis(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_publish_finite_checked_deadlines() {
        let timeouts = OperationTimeouts::default();
        let deadlines = [
            timeouts.preparation(),
            timeouts.create(),
            timeouts.start(),
            timeouts.term_signal(),
            timeouts.kill_signal(),
            timeouts.wait(),
            timeouts.forced_stop_confirmation(),
            timeouts.stream_drain(),
            timeouts.runtime_filesystem_removal(),
            timeouts.final_environment_capture(),
            timeouts.cleanup(),
        ];

        assert!(deadlines.into_iter().all(|deadline| {
            deadline >= Duration::from_millis(1) && deadline <= MAX_OPERATION_TIMEOUT
        }));
        assert_eq!(timeouts.preparation(), Duration::from_mins(30));
        assert_eq!(timeouts.create(), Duration::from_mins(2));
        assert_eq!(timeouts.start(), Duration::from_mins(2));
        assert_eq!(timeouts.term_signal(), Duration::from_mins(2));
        assert_eq!(timeouts.kill_signal(), Duration::from_mins(2));
        assert_eq!(timeouts.wait(), Duration::from_mins(2));
        assert_eq!(timeouts.forced_stop_confirmation(), Duration::from_secs(30));
        assert_eq!(timeouts.stream_drain(), Duration::from_mins(2));
        assert_eq!(
            timeouts.runtime_filesystem_removal(),
            Duration::from_mins(2)
        );
        assert_eq!(
            timeouts.final_environment_capture(),
            Duration::from_mins(30)
        );
        assert_eq!(timeouts.cleanup(), Duration::from_mins(2));
        assert_eq!(STOP_GRACE_PERIOD, Duration::from_secs(10));
    }

    #[test]
    fn configuration_rejects_zero_submillisecond_and_excessive_deadlines() {
        let standard = OperationTimeouts::default();
        for timeout in [
            Duration::ZERO,
            Duration::from_nanos(1),
            MAX_OPERATION_TIMEOUT + Duration::from_millis(1),
        ] {
            let error = standard.with_create(timeout).expect_err("invalid timeout");
            assert_eq!(error.operation(), "create");
        }
        assert_eq!(
            standard
                .with_create(Duration::from_millis(1))
                .expect("minimum timeout")
                .create(),
            Duration::from_millis(1)
        );
    }
}
