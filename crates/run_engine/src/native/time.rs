use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult};
use chrono::{DateTime, FixedOffset, Local};

pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) fn checked_deadline(
    start: Instant,
    duration: Duration,
    operation: &str,
) -> AnyResult<Instant> {
    start
        .checked_add(duration)
        .with_context(|| format!("{operation} exceeds the monotonic clock range"))
}

pub(super) fn wall_clock_now() -> DateTime<FixedOffset> {
    Local::now().fixed_offset()
}

pub(super) fn execution_expired(start: Option<Instant>, limit: Option<Duration>) -> bool {
    start
        .zip(limit)
        .is_some_and(|(start, limit)| start.elapsed() >= limit)
}
