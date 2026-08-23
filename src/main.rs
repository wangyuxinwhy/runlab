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
mod distribution;
mod docker;
mod execution;
mod filesystem;
mod image;
mod image_ingress;
mod ingress;
mod integrity;
mod maintenance;
mod managed_vm;
#[cfg(target_os = "linux")]
mod materialize;
#[cfg(target_os = "linux")]
mod native_backend;
#[cfg(target_os = "linux")]
mod native_cgroup;
#[cfg(target_os = "linux")]
mod native_fs;
#[cfg(any(test, target_os = "linux"))]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "native execution is Linux-only")
)]
mod native_network;
#[cfg(target_os = "linux")]
mod native_reconcile;
#[cfg(any(test, target_os = "linux"))]
mod native_recovery;
#[cfg(target_os = "linux")]
mod native_resolver;
mod oci;
#[cfg(target_os = "linux")]
mod read_only_file;
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
