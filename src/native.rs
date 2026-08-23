//! Native Linux execution: the reference backend that runs an OCI bundle
//! through runc directly, without a container engine.
//!
//! Everything here is Linux-only. The whole subtree is gated once, at the
//! `mod native;` declaration in `main.rs`, so no module inside needs a
//! per-item `#[cfg(target_os = "linux")]` for that reason alone.
//!
//! Layering inside the subtree runs bottom-up:
//!
//! - `cgroup`, `fs`, `network`, `resolver`, `read_only_file` own one host
//!   resource each and know nothing about Runs.
//! - `runc` owns the runc subprocess: identity, lifecycle, streams, and raw
//!   observations. It is the only module that knows runc exists.
//! - `backend` composes those into preflight and realization facts, and is the
//!   only name execution needs in order to start a native Run.
//! - `recovery` owns the durable attempt journal; `reconcile` turns an
//!   interrupted attempt back into a terminal Run.
//!
//! Callers outside this subtree use the re-exports below. Nothing else is
//! reachable, which keeps runc mechanics from leaking into orchestration.

pub(crate) mod backend;
pub(crate) mod cgroup;
pub(crate) mod fs;
pub(crate) mod network;
pub(crate) mod read_only_file;
pub(crate) mod reconcile;
pub(crate) mod recovery;
pub(crate) mod resolver;
pub(crate) mod runc;
