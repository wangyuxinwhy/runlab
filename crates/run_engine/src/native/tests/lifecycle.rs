use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use run_protocol::{Availability, Network, OperationStage, OperationStatus, ProgramId, RunInput};
use rustix::process::geteuid;

use super::super::{ExecutionContext, apply_runtime_cleanup_report, capture_final, execute};
use super::fixtures::*;
use crate::native::budget::OperationBudget;
use crate::native::cleanup::{
    CgroupProcessProof, RuntimeCleanup, cleanup_invocation, cleanup_runtime,
};
use crate::native::program::{ProgramRun, RootfsStability};
use crate::native::report::operation_error;
use crate::native::start::{ProgramStarter, StartControl};
use crate::native::subprocess::{InvocationSupervisor, SupervisorLifecycle};
use crate::native::time::POLL_INTERVAL;
use crate::native::wait::finalize_children;
use crate::{CancellationToken, NativeEngine, OperationTimeouts};

#[test]
fn create_attach_failure_is_attempted_and_deleted_after_supervisor_is_safe() {
    let (_runc_workspace, runc, created, deleted) = fake_runc_with_create_markers(Duration::ZERO);
    let supervisor = InvocationSupervisor::new();
    let workspace = tempfile::tempdir().expect("invocation workspace");
    let mut prepared = empty_prepared_invocation(
        workspace,
        runc.clone(),
        supervisor.clone(),
        "run-engine-attach-fault-delete",
    );
    supervisor.inject_faults(0, 1, 0, 0);
    let mut run = ProgramStarter::new(
        &supervisor,
        &runc,
        &prepared.runtime_root,
        OperationTimeouts::default(),
    )
    .start(
        &prepared.programs[&ProgramId::primary()],
        &test_program(),
        StartControl::new(&CancellationToken::new(), None, None),
        &mut BTreeMap::new(),
    );
    assert_eq!(run.facts.create.status(), OperationStatus::Unknown);
    assert_eq!(run.facts.start.status(), OperationStatus::NotAttempted);
    assert!(run.runtime.attempted && !run.runtime.writer_stopped);
    assert!(run.runtime.coordinates.is_none());
    assert!(run.facts.process.is_none());
    assert!(run.facts.create.errors().any(|error| {
        error
            .message()
            .contains("attachment failed after registration")
    }));
    let marker_deadline = Instant::now() + Duration::from_secs(1);
    while !created.exists() && Instant::now() < marker_deadline {
        thread::sleep(POLL_INTERVAL);
    }
    assert!(created.exists(), "fake create side effect did not occur");

    let mut outcomes = BTreeMap::from([(ProgramId::primary(), run)]);
    finalize_children(&mut outcomes, OperationTimeouts::default());
    supervisor
        .finalize(Instant::now() + Duration::from_secs(1))
        .expect("create supervisor safe before runtime delete");
    run = outcomes.remove(&ProgramId::primary()).expect("run");
    let report = cleanup_runtime(RuntimeCleanup {
        runc: &runc,
        runtime_root: &prepared.runtime_root,
        program: &prepared.programs[&ProgramId::primary()],
        supervisor: &run.supervision.owner,
        runtime_attempted: run.runtime.attempted,
        observed_cgroup: run.runtime.cgroup_path.as_deref(),
        removal_timeout: OperationTimeouts::default().runtime_filesystem_removal(),
        supervisor_deadline: Instant::now() + Duration::from_secs(2),
    });
    apply_runtime_cleanup_report(&mut run, report);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(1))
        .expect("delete helper reaped");
    assert!(deleted.exists(), "safe runtime delete was not attempted");
    assert!(!created.exists(), "fake runtime object survived delete");
    assert!(
        run.facts.process.is_none(),
        "create failure must remain NeverStarted"
    );
    assert!(run.runtime.writer_stopped);
    if let Some(issue) = cleanup_invocation(
        &mut prepared,
        true,
        OperationBudget::new(Duration::from_secs(2), "test cleanup").expect("budget"),
    ) {
        let (message, code) = issue.into_parts();
        panic!("invocation cleanup failed: {message} ({code:?})");
    }
}

#[test]
fn unproved_late_create_cannot_race_runtime_cleanup_or_capture() {
    let (_runc_workspace, runc, created, deleted) =
        fake_runc_with_create_markers(Duration::from_millis(50));
    let supervisor = InvocationSupervisor::new();
    supervisor.inject_faults(0, 1, 0, usize::MAX);
    let workspace = tempfile::tempdir().expect("invocation workspace");
    let workspace_path = workspace.path().to_path_buf();
    let mut prepared = empty_prepared_invocation(
        workspace,
        runc.clone(),
        supervisor.clone(),
        "run-engine-late-create-gate",
    );
    let store = Arc::new(PublishCountingStore::default());
    let timeouts = OperationTimeouts::default()
        .with_cleanup(Duration::from_millis(100))
        .expect("cleanup timeout")
        .with_wait(Duration::from_millis(10))
        .expect("wait timeout")
        .with_forced_stop_confirmation(Duration::from_millis(10))
        .expect("confirmation timeout")
        .with_stream_drain(Duration::from_millis(10))
        .expect("drain timeout");
    let engine = NativeEngine::new(store.clone(), &workspace_path, runc, timeouts);
    let input = RunInput::new(
        BTreeMap::from([(ProgramId::primary(), test_program())]),
        None,
        Network::Isolated,
    )
    .expect("input");
    let error = execute(
        &engine.execution_context(),
        &input,
        &CancellationToken::new(),
        &mut prepared,
    )
    .expect_err("unproved create supervisor must withhold RunOutput");
    assert!(error.to_string().contains("preserved workspace"), "{error}");
    assert!(
        created.exists(),
        "late fake create side effect was not observed"
    );
    assert!(
        !deleted.exists(),
        "runtime delete crossed an unsafe boundary"
    );
    assert_eq!(
        store.publishes.load(Ordering::Relaxed),
        0,
        "capture published"
    );
    assert!(
        workspace_path.exists(),
        "unsafe workspace was not preserved"
    );

    supervisor.inject_faults(0, 0, 0, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("test teardown closes retained supervisor");
    drop(prepared);
    fs::remove_dir_all(&workspace_path).expect("remove preserved test workspace");
}

#[test]
fn kill_delivered_at_cleanup_deadline_preserves_without_drop_tail() {
    let (_runc_workspace, runc, _created, deleted) = fake_runc_with_create_markers(Duration::ZERO);
    let supervisor = InvocationSupervisor::new();
    supervisor.inject_faults(usize::MAX, 1, 0, 0);
    let workspace = tempfile::tempdir().expect("invocation workspace");
    let workspace_path = workspace.path().to_path_buf();
    let mut prepared = empty_prepared_invocation(
        workspace,
        runc.clone(),
        supervisor.clone(),
        "run-engine-expired-safe-gate",
    );
    let store = Arc::new(PublishCountingStore::default());
    let timeouts = OperationTimeouts::default()
        .with_cleanup(Duration::from_millis(100))
        .expect("cleanup timeout")
        .with_wait(Duration::from_millis(10))
        .expect("wait timeout")
        .with_forced_stop_confirmation(Duration::from_millis(10))
        .expect("confirmation timeout")
        .with_stream_drain(Duration::from_millis(10))
        .expect("drain timeout");
    let engine = NativeEngine::new(store.clone(), &workspace_path, runc, timeouts);
    let input = RunInput::new(
        BTreeMap::from([(ProgramId::primary(), test_program())]),
        None,
        Network::Isolated,
    )
    .expect("input");
    let began = Instant::now();
    let error = execute(
        &engine.execution_context(),
        &input,
        &CancellationToken::new(),
        &mut prepared,
    )
    .expect_err("KillDelivered without reap must withhold cleanup and capture");
    assert!(error.to_string().contains("preserved workspace"), "{error}");
    assert!(matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::KillDelivered { children: 1 }
    ));
    assert!(
        !deleted.exists(),
        "delete spawned after the absolute deadline"
    );
    assert_eq!(
        store.publishes.load(Ordering::Relaxed),
        0,
        "capture published"
    );
    assert!(
        workspace_path.exists(),
        "unsafe workspace was not preserved"
    );
    let raw_pid = supervisor.only_child_pid();
    drop(prepared);
    drop(supervisor);
    assert!(
        began.elapsed() < Duration::from_millis(300),
        "Drop added a hidden bounded-wait tail: {:?}",
        began.elapsed()
    );
    rustix::process::waitpid(Some(raw_pid), rustix::process::WaitOptions::empty())
        .expect("reap already-KILLed test child")
        .expect("already-KILLed child wait status");
    fs::remove_dir_all(&workspace_path).expect("remove preserved test workspace");
}

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
        observed_cgroup: None,
        removal_timeout: OperationTimeouts::default().runtime_filesystem_removal(),
        supervisor_deadline: Instant::now() + Duration::from_secs(2),
    });
    assert_eq!(report.cgroup_processes, CgroupProcessProof::Absent);
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
