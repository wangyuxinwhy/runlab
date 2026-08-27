use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use run_protocol::{Availability, OperationStage, ProgramId};
use rustix::process::geteuid;

use super::super::{ExecutionContext, apply_runtime_cleanup_report, capture_final};
use super::fixtures::*;
use crate::OperationTimeouts;
use crate::native::cleanup::{RuntimeCleanup, cleanup_runtime};
use crate::native::program::{ProgramRun, RootfsStability};
use crate::native::report::operation_error;
use crate::native::subprocess::InvocationSupervisor;

#[test]
fn unattempted_output_is_structurally_valid_and_timeouts_are_stable() {
    let engine = test_engine();
    assert_eq!(engine.operation_timeouts(), OperationTimeouts::default());
    let mut run = ProgramRun::unattempted();
    run.output(Availability::unavailable("not captured").expect("reason"))
        .expect("valid output");
}

#[test]
fn final_environment_unavailability_preserves_the_failed_capture_evidence() {
    let workspace = tempfile::tempdir().expect("capture workspace");
    let supervisor = InvocationSupervisor::new();
    let prepared =
        empty_prepared_invocation(workspace, "/bin/true".into(), supervisor, "capture-failure");
    let program = &prepared.programs[&ProgramId::primary()];
    let context = ExecutionContext::new(Arc::new(UnavailableStore), OperationTimeouts::default());

    let mut active_writer = ProgramRun::unattempted();
    active_writer.runtime.writer_stopped = false;
    let unavailable = capture_final(&context, program, &mut active_writer);
    assert_eq!(
        unavailable.unavailable_reason().expect("writer reason"),
        "a process that could still write the rootfs was not proved stopped"
    );
    assert!(active_writer.facts.errors.is_empty());

    let mut residual_mount = ProgramRun::unattempted();
    residual_mount.runtime.writer_stopped = true;
    let unavailable = capture_final(&context, program, &mut residual_mount);
    assert_eq!(
        unavailable.unavailable_reason().expect("mount reason"),
        "the underlying rootfs was not proved stable after mount and artifact cleanup"
    );

    let mut failed_capture = ProgramRun::unattempted();
    failed_capture.runtime.writer_stopped = true;
    failed_capture.runtime.rootfs_stability = RootfsStability::Stable;
    failed_capture.facts.errors.push(operation_error(
        OperationStage::RuntimeFilesystemRemoval,
        "proved-empty cgroup could not be removed",
        None,
    ));
    let unavailable = capture_final(&context, program, &mut failed_capture);
    assert!(
        unavailable
            .unavailable_reason()
            .expect("capture reason")
            .contains("failed to capture final environment")
    );
    assert!(failed_capture.facts.errors.iter().any(|error| {
        error.stage() == OperationStage::FinalEnvironmentCapture
            && error
                .message()
                .contains("failed to capture final environment")
    }));
    assert!(failed_capture.facts.errors.iter().any(|error| {
        error.stage() == OperationStage::RuntimeFilesystemRemoval
            && error.message().contains("proved-empty cgroup")
    }));
}

#[test]
fn runtime_cleanup_reports_nonempty_mount_artifact_as_rootfs_instability() {
    if !geteuid().is_root() {
        return;
    }
    let workspace = tempfile::tempdir().expect("cleanup workspace");
    let supervisor = InvocationSupervisor::new();
    let mut prepared = empty_prepared_invocation(
        workspace,
        "/bin/true".into(),
        supervisor,
        "artifact-instability",
    );
    let program = prepared
        .programs
        .get_mut(&ProgramId::primary())
        .expect("primary program");
    let artifact = PathBuf::from("runtime-created/nested");
    fs::create_dir_all(program.rootfs.path().join(&artifact)).expect("artifact directory");
    fs::write(
        program.rootfs.path().join(&artifact).join("change"),
        b"keep",
    )
    .expect("rootfs change");
    program.artifacts = vec![PathBuf::from("runtime-created"), artifact];

    let mut run = ProgramRun::unattempted();
    let report = cleanup_runtime(RuntimeCleanup {
        runc: &prepared.runc,
        runtime_root: &prepared.runtime_root,
        program: &prepared.programs[&ProgramId::primary()],
        supervisor: &run.supervision.owner,
        runtime_attempted: false,
        removal_timeout: OperationTimeouts::default().runtime_filesystem_removal(),
        supervisor_deadline: Instant::now() + Duration::from_secs(2),
    });
    assert_eq!(report.rootfs_stability, RootfsStability::Unproved);
    apply_runtime_cleanup_report(&mut run, report);
    assert!(run.runtime.writer_stopped);
    assert_eq!(run.runtime.rootfs_stability, RootfsStability::Unproved);
    assert!(run.facts.errors.iter().any(|error| {
        error
            .message()
            .contains("left the rootfs unstable: rootfs instability")
    }));
    let context = ExecutionContext::new(Arc::new(UnavailableStore), OperationTimeouts::default());
    let unavailable = capture_final(
        &context,
        &prepared.programs[&ProgramId::primary()],
        &mut run,
    );
    assert_eq!(
        unavailable.unavailable_reason().expect("rootfs reason"),
        "the underlying rootfs was not proved stable after mount and artifact cleanup"
    );
    assert_eq!(
        fs::read(
            prepared.programs[&ProgramId::primary()]
                .rootfs
                .path()
                .join("runtime-created/nested/change")
        )
        .expect("preserved rootfs change"),
        b"keep"
    );
}
