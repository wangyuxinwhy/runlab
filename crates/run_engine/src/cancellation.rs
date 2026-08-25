use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cloneable, invocation-scoped request to terminate an in-progress execution.
///
/// Clones share one cancellation state. Tokens created by separate calls to
/// [`CancellationToken::new`] remain independent. Dropping a token does not
/// request cancellation.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    requested: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates a token whose cancellation has not been requested.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation for every clone of this token.
    ///
    /// Repeated requests are idempotent.
    pub fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_idempotent_request() {
        let first = CancellationToken::new();
        let second = first.clone();

        assert!(!first.is_cancelled());
        second.cancel();
        second.cancel();

        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
    }

    #[test]
    fn invocations_are_isolated_and_drop_is_not_cancellation() {
        let first = CancellationToken::new();
        let dropped_clone = first.clone();
        let independent = CancellationToken::new();

        drop(dropped_clone);
        assert!(!first.is_cancelled());

        first.cancel();
        assert!(!independent.is_cancelled());
    }

    #[test]
    fn token_is_thread_safe() {
        let token = CancellationToken::new();
        let worker = token.clone();
        std::thread::spawn(move || worker.cancel())
            .join()
            .expect("cancelling thread");

        assert!(token.is_cancelled());
    }
}
