use std::fs;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use run_protocol::ProcessResult;
use rustix::process::Pid;

use super::fixtures::*;
use crate::native::budget::{BudgetedStore, OperationBudget};
use crate::native::container_path::safe_container_path;
use crate::native::linux_evidence::ProcExitMonitor;
use crate::native::program::ProgramRun;
use crate::native::subprocess::{
    HELPER_OUTPUT_LIMIT, InvocationSupervisor, SupervisorLifecycle, run_helper, run_helper_until,
};
use crate::native::time::POLL_INTERVAL;
use crate::native::wait::{poll_one, process_from_raw_wait_status};
use crate::{ContentErrorKind, OciContentStore};

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
    assert!(
        !error.to_string().contains("unknown supervisor token"),
        "{error:#}"
    );
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);
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
fn helper_deadline_kills_and_reaps_the_process_group() {
    let budget = OperationBudget::new(Duration::ZERO, "test preparation").expect("deadline");
    let store = BudgetedStore::new(Arc::new(UnavailableStore), budget);
    let Err(error) = store.open(test_image().as_oci()) else {
        panic!("expired budget reached the underlying store");
    };
    assert_eq!(error.kind(), ContentErrorKind::Internal);

    let supervisor = InvocationSupervisor::new();
    let began = Instant::now();
    let error = run_helper(
        &supervisor,
        Command::new("/bin/sh").args(["-c", "sleep 30"]),
        Duration::from_millis(20),
    )
    .expect_err("helper timeout");
    assert!(error.to_string().contains("deadline"));
    assert!(began.elapsed() < Duration::from_secs(2));
    assert_eq!(supervisor.lifecycle(), SupervisorLifecycle::Reaped);

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
    assert!(!marker_path.exists());
}

#[test]
fn stopped_state_waits_for_queued_raw_exit_evidence() {
    let (socket, _peer) = UnixStream::pair().expect("socket pair");
    let mut run = ProgramRun::unattempted();
    run.runtime.exit_monitor = Some(ProcExitMonitor::from_test_socket(socket.into(), 42));

    run.observe_runtime_stopped(Duration::from_secs(1));
    assert!(run.runtime.writer_stopped);
    assert!(run.facts.process.is_none());
    run.observe_raw_process_result(process_from_raw_wait_status(7 << 8));
    assert!(matches!(
        run.facts.process,
        Some(ProcessResult::Exited { code: 7, .. })
    ));
}

#[test]
fn runc_state_failure_is_wait_evidence() {
    let workspace = tempfile::tempdir().expect("state probe workspace");
    let mut run = ProgramRun::unattempted();
    run.runtime.coordinates = Some((
        "/bin/false".into(),
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
    assert!(error.to_string().contains("runc state"), "{error}");
    assert!(run.supervision.state_probe_failed);
}
