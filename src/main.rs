//! `runlab` executes OCI Images and preserves immutable Run Records.
//!
//! One binary, one composition root. `main` parses nothing itself; it hands the
//! process to `cli` and turns the result into an exit status.
//!
//! # Layering
//!
//! Dependencies run one way. Reading top to bottom, a module may use anything
//! below it and nothing above:
//!
//! ```text
//! cli                     argument shapes, JSON output, exit status
//! execution               acceptance -> execute -> terminal Run Record
//! native  docker          the two execution backends
//! image  ingress  storage Images in the Layout, Run Records in SQLite
//! oci  runtime  render    exact-byte Layout, Runtime config, Layer views
//! integrity               digests, canonical JSON, durable private writes
//! core                    the Run Protocol vocabulary
//! ```
//!
//! Two consequences worth keeping:
//!
//! - `core` states protocol invariants, so a Run Record is validated wherever it
//!   is produced rather than only where it is stored.
//! - `integrity` owns every exact-byte read and durable write, so "owner-only,
//!   crash-atomic, fsynced" is decided once.
//!
//! Everything specific to running an OCI bundle through runc on this host is
//! inside `native`, gated once at its `mod` declaration. No module outside
//! `docker` knows that a `docker` process exists.

use std::process::ExitCode;

#[cfg_attr(
    not(any(test, target_os = "linux")),
    allow(dead_code, reason = "OCI bundle execution is Linux-only")
)]
mod bundle;
mod catalog;
mod changeset;
mod cli;
mod core;
mod docker;
mod execution;
mod filesystem;
mod image;
mod ingress;
mod integrity;
mod maintenance;
mod managed_vm;
#[cfg(target_os = "linux")]
mod materialize;
#[cfg(target_os = "linux")]
mod native;
mod oci;
mod profiling;
mod reconciliation;
mod render;
mod runtime;
mod signal;
mod state;
mod storage;
mod subprocess;
mod topology;

fn main() -> ExitCode {
    match cli::run() {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            eprintln!("runlab: {failure:#}");
            ExitCode::FAILURE
        }
    }
}
