use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use run_protocol::{Network, ProcessResult, ProgramId, RunInput};
use rustix::process::Pid;

use super::fixtures::*;
use crate::native::NativeEngine;
use crate::native::budget::{BudgetedStore, OperationBudget};
use crate::native::container_path::safe_container_path;
use crate::native::execution::preserve_workspace_after_supervisor_failure;
use crate::native::linux_evidence::ProcExitMonitor;
use crate::native::program::ProgramRun;
use crate::native::subprocess::{
    HELPER_OUTPUT_LIMIT, InvocationSupervisor, SupervisorLifecycle, run_helper, run_helper_until,
};
use crate::native::time::POLL_INTERVAL;
use crate::native::wait::{poll_one, process_from_raw_wait_status};
use crate::{CancellationToken, ContentErrorKind, OciContentStore, OperationTimeouts};

#[test]
fn helper_output_and_container_paths_are_bounded() {
    let pid_file = tempfile::NamedTempFile::new().expect("pid file");
    let supervisor = InvocationSupervisor::new();
    let error = run_helper(
        &supervisor,
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "printf %s \"$$\" > \"$1\"; head -c {} /dev/zero; sleep 30",
                HELPER_OUTPUT_LIMIT + 1
            ))
            .arg("runlab-helper-test")
            .arg(pid_file.path()),
        Duration::from_secs(5),
    )
    .expect_err("helper output cap");
    assert!(error.to_string().contains("output exceeds"), "{error:#}");
    let pid = fs::read_to_string(pid_file.path())
        .expect("helper pid")
        .parse::<i32>()
        .expect("numeric helper pid");
    let pid = Pid::from_raw(pid).expect("positive helper pid");
    assert_eq!(
        rustix::process::test_kill_process(pid).expect_err("helper must be gone"),
        rustix::io::Errno::SRCH
    );
    assert!(safe_container_path("/a/b").is_ok());
    assert!(safe_container_path("/a/../b").is_err());
    assert!(safe_container_path("relative").is_err());
}

#[test]
fn operation_budget_and_helper_deadline_are_explicit() {
    let budget = OperationBudget::new(Duration::ZERO, "test preparation").expect("deadline");
    let store = BudgetedStore::new(Arc::new(UnavailableStore), budget);
    let Err(error) = store.open(test_image().as_oci()) else {
        panic!("expired budget reached the underlying store");
    };
    assert_eq!(error.kind(), ContentErrorKind::Internal);
    assert!(error.reason().contains("deadline exceeded"));

    let began = Instant::now();
    let supervisor = InvocationSupervisor::new();
    let error = run_helper(
        &supervisor,
        Command::new("/bin/sh").args(["-c", "sleep 30"]),
        Duration::from_millis(20),
    )
    .expect_err("helper timeout");
    assert!(error.to_string().contains("deadline"));
    assert!(began.elapsed() < Duration::from_secs(2));

    let marker = tempfile::NamedTempFile::new().expect("marker path");
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    let error = run_helper_until(
        &supervisor,
        Command::new("/bin/sh")
            .arg("-c")
            .arg("printf spawned > \"$1\"")
            .arg("tiny-deadline")
            .arg(&marker_path),
        Instant::now() + POLL_INTERVAL,
        None,
    )
    .expect_err("tiny remaining interval must reject before spawn");
    assert!(error.to_string().contains("insufficient time"));
    assert!(!marker_path.exists(), "tiny-deadline helper was spawned");
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);

    assert!(matches!(
        process_from_raw_wait_status(7 << 8),
        ProcessResult::Exited { code: 7, .. }
    ));
    assert!(matches!(
        process_from_raw_wait_status(9),
        ProcessResult::Signaled { signal, .. } if signal.get() == 9
    ));
    assert!(matches!(
        process_from_raw_wait_status(0x7f),
        ProcessResult::Unknown { .. }
    ));
}

#[test]
fn stopped_state_waits_for_already_queued_raw_exit_evidence() {
    let (socket, _peer) = UnixStream::pair().expect("socket pair");
    let mut run = ProgramRun::unattempted();
    run.runtime.exit_monitor = Some(ProcExitMonitor::from_test_socket(socket.into(), 42));

    run.observe_runtime_stopped(Duration::from_secs(1));
    assert!(run.runtime.writer_stopped);
    assert!(
        run.facts.process.is_none(),
        "state must not preempt raw evidence"
    );
    assert!(
        run.runtime
            .stopped_observation
            .is_some_and(|(_, deadline)| deadline > Instant::now())
    );

    run.observe_raw_process_result(process_from_raw_wait_status(7 << 8));
    assert!(matches!(
        run.facts.process,
        Some(ProcessResult::Exited { code: 7, .. })
    ));
    assert!(run.runtime.stopped_observation.is_none());
}

#[test]
fn invocation_supervisor_retries_transient_kill_and_wait_failures() {
    let supervisor = InvocationSupervisor::new();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    supervisor.spawn(&mut command).expect("registered child");
    supervisor.inject_faults(1, 0, 1, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("final guard retries transient failures");
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
}

#[test]
fn invocation_supervisor_distinguishes_kill_delivery_from_unproved_termination() {
    let marker = tempfile::NamedTempFile::new().expect("marker path");
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    let supervisor = InvocationSupervisor::new();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep .2; printf late > \"$1\"")
        .arg("supervisor-child")
        .arg(&marker_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    supervisor.spawn(&mut command).expect("registered child");
    supervisor.inject_faults(usize::MAX, 0, 0, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_millis(100))
        .expect_err("wait evidence remains unavailable");
    assert!(matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::KillDelivered { children: 1 }
    ));
    thread::sleep(Duration::from_millis(300));
    assert!(!marker_path.exists(), "proved KILL allowed a late marker");
    supervisor.inject_faults(0, 0, 0, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("reap after fault removal");

    let supervisor = InvocationSupervisor::new();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    supervisor.spawn(&mut command).expect("registered child");
    supervisor.inject_faults(usize::MAX, 0, usize::MAX, 0);
    let failure = supervisor
        .finalize(Instant::now() + Duration::from_millis(30))
        .expect_err("termination cannot be proved");
    assert!(matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::TerminationUnproven {
            kill_delivered: 0,
            unproved: 1
        }
    ));
    let workspace = tempfile::tempdir().expect("workspace");
    let preserved = workspace.path().to_path_buf();
    let error = preserve_workspace_after_supervisor_failure(workspace, &failure);
    assert!(error.to_string().contains("preserved workspace"));
    assert!(preserved.is_dir());
    supervisor.inject_faults(0, 0, 0, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("supervisor still owns and reaps child after fault removal");
    fs::remove_dir(&preserved).expect("remove empty preserved test workspace");
}

#[test]
fn pidfd_open_failure_is_registered_and_bounded_by_run_owner() {
    let supervisor = InvocationSupervisor::new();
    supervisor.inject_faults(0, 1, 0, 0);
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let began = Instant::now();
    let spawn_error = supervisor
        .spawn(&mut command)
        .expect_err("pidfd_open injection");
    assert!(spawn_error.to_string().contains("pidfd_open"));
    assert!(supervisor.only_child_termination_started());
    assert!(matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::TerminationUnproven {
            kill_delivered: 0,
            unproved: 1
        }
    ));
    supervisor
        .finalize(Instant::now() + Duration::from_millis(100))
        .expect("unreaped pid permits bounded process-group cleanup");
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
}

#[test]
fn preflight_pidfd_open_failure_uses_preparation_deadline_not_drop() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private workspace");
    let runc = workspace.path().join("runc");
    fs::write(&runc, "#!/bin/sh\nsleep 30\n").expect("fake runc");
    fs::set_permissions(&runc, fs::Permissions::from_mode(0o700)).expect("executable runc");
    let engine = NativeEngine::new(
        Arc::new(UnavailableStore),
        workspace.path(),
        runc,
        OperationTimeouts::default()
            .with_preparation(Duration::from_millis(100))
            .expect("preparation timeout"),
    );
    let supervisor = InvocationSupervisor::new();
    supervisor.inject_faults(0, 1, 0, 0);
    let input = RunInput::new(
        BTreeMap::from([(ProgramId::primary(), test_program())]),
        None,
        Network::Isolated,
    )
    .expect("input");
    let began = Instant::now();
    let error = engine
        .run_supervised(&input, &CancellationToken::new(), &supervisor)
        .expect_err("preflight pidfd failure");
    assert!(began.elapsed() < Duration::from_secs(1));
    assert!(error.to_string().contains("pidfd_open"));
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
    assert_eq!(
        fs::read_dir(workspace.path())
            .expect("workspace entries")
            .count(),
        1,
        "preflight must not create an invocation workspace"
    );
}

#[test]
fn group_kill_failure_keeps_zombie_identity_until_descendants_are_killed() {
    let marker = tempfile::NamedTempFile::new().expect("marker path");
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    let supervisor = InvocationSupervisor::new();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("(sleep .4; printf late > \"$1\") & wait")
        .arg("supervisor-descendant")
        .arg(&marker_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    supervisor
        .spawn(&mut command)
        .expect("registered process group");
    supervisor.inject_faults(0, 0, 0, usize::MAX);
    supervisor
        .finalize(Instant::now() + Duration::from_millis(100))
        .expect_err("group KILL remains unproved");
    let (exit_observed, leader_kill, group_kill, leader_reaped) = supervisor.only_child_facts();
    assert!(exit_observed && leader_kill);
    assert!(!group_kill && !leader_reaped);
    assert!(matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::TerminationUnproven {
            kill_delivered: 0,
            unproved: 1
        }
    ));
    supervisor.inject_faults(0, 0, 0, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("retry proves group KILL and reaps leader");
    thread::sleep(Duration::from_millis(500));
    assert!(
        !marker_path.exists(),
        "descendant wrote after proved group KILL"
    );
}

#[test]
fn pidfd_open_failure_keeps_fast_exit_unreaped_until_group_cleanup() {
    let marker = tempfile::NamedTempFile::new().expect("marker path");
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    let supervisor = InvocationSupervisor::new();
    supervisor.inject_faults(0, 1, 0, usize::MAX);
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("(sleep .4; printf late > \"$1\") & exit 0")
        .arg("supervisor-no-pidfd-descendant")
        .arg(&marker_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    supervisor
        .spawn(&mut command)
        .expect_err("production-path pidfd_open fault");
    assert!(supervisor.only_child_termination_started());
    supervisor
        .finalize(Instant::now() + Duration::from_millis(100))
        .expect_err("persistent group failure must remain unproved");
    let (exit_observed, leader_kill, group_kill, leader_reaped) = supervisor.only_child_facts();
    assert!(
        exit_observed,
        "fast leader exit was not observed with WNOWAIT"
    );
    assert!(!leader_kill && !group_kill && !leader_reaped);
    assert!(matches!(
        supervisor.lifecycle(),
        SupervisorLifecycle::TerminationUnproven {
            kill_delivered: 0,
            unproved: 1
        }
    ));
    supervisor.inject_faults(0, 0, 0, 0);
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("retained zombie identity permits a later group cleanup and reap");
    thread::sleep(Duration::from_millis(500));
    assert!(
        !marker_path.exists(),
        "descendant wrote after no-pidfd process-group cleanup"
    );
}

#[test]
fn pidfd_fast_exit_keeps_zombie_identity_until_descendants_are_killed() {
    let marker = tempfile::NamedTempFile::new().expect("marker path");
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    let supervisor = InvocationSupervisor::new();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("(sleep .4; printf late > \"$1\") & exit 0")
        .arg("supervisor-pidfd-descendant")
        .arg(&marker_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let token = supervisor
        .spawn(&mut command)
        .expect("production pidfd attach");
    let exit_deadline = Instant::now() + Duration::from_secs(2);
    while !supervisor.only_child_exit_ready() {
        assert!(
            Instant::now() < exit_deadline,
            "leader did not exit in time"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !supervisor
            .progress_kill(token)
            .expect("exited leader still permits process-group cleanup"),
        "an already-exited leader must not be reported as receiving KILL"
    );
    supervisor
        .finalize(Instant::now() + Duration::from_secs(2))
        .expect("exited leader identity permits process-group cleanup and reap");
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
    thread::sleep(Duration::from_millis(500));
    assert!(
        !marker_path.exists(),
        "descendant wrote after pidfd process-group cleanup"
    );
}

#[test]
fn runc_state_failures_are_wait_evidence_and_do_not_respawn() {
    for (label, executable) in [("nonzero", "/bin/false"), ("invalid-json", "/bin/echo")] {
        let workspace = tempfile::tempdir().expect("state probe workspace");
        let mut run = ProgramRun::unattempted();
        run.runtime.coordinates = Some((
            executable.into(),
            workspace.path().to_path_buf(),
            "state-test".to_owned(),
            Duration::from_secs(1),
        ));
        let error = loop {
            match poll_one(&mut run) {
                Ok(_) => thread::sleep(POLL_INTERVAL),
                Err(error) => break error,
            }
        };
        assert!(
            error.to_string().contains("runc state")
                || error.kind() == std::io::ErrorKind::InvalidData,
            "{label}: {error}"
        );
        assert!(run.supervision.state_probe_failed, "{label}");
        assert!(run.supervision.state_probe.is_none(), "{label}");
        assert!(!poll_one(&mut run).expect("failed probe stays disabled"));
    }
}
