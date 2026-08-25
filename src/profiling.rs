//! Compile-time opt-in timing for implementation experiments.
//!
//! `RUNLAB_INTERNAL_PROFILE` is read while building the binary, not from a
//! public CLI or Run input. Timings go to the `RunLab` process's stderr and
//! never enter Run Records or participant stream facts.

use std::time::Instant;

const ENABLED: bool = option_env!("RUNLAB_INTERNAL_PROFILE").is_some();

pub(crate) fn measure<T>(phase: &'static str, operation: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = operation();
    if ENABLED {
        eprintln!(
            "{{\"kind\":\"runlab_internal_profile\",\"phase\":\"{phase}\",\"elapsed_microseconds\":{}}}",
            started.elapsed().as_micros()
        );
    }
    result
}
