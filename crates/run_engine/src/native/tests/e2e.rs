use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::net::TcpListener;
use std::num::NonZeroU64;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use run_protocol::{
    ExecutionInterval, Network, OperationStatus, ProcessResult, ProgramId, RunControls, RunInput,
    StopSignal,
};
use rustix::process::geteuid;

use super::fixtures::*;
use crate::native::network::HOST_ADDRESS;
use crate::{
    CancellationToken, EngineObserver, NativeEngine, OperationTimeouts, ProgramStream, RunEngine,
    STOP_GRACE_PERIOD,
};

#[test]
#[ignore = "set RUNLAB_NATIVE_E2E_OCI_LAYOUT and run as root on the runlab Linux VM"]
#[allow(
    clippy::too_many_lines,
    reason = "one opt-in real-runtime lifecycle shares one imported image while asserting cross-phase evidence"
)]
fn real_runc_exercises_native_engine_contract() {
    assert_eq!(geteuid().as_raw(), 0, "real NativeEngine E2E requires root");
    let layout = PathBuf::from(
        std::env::var_os("RUNLAB_NATIVE_E2E_OCI_LAYOUT")
            .expect("RUNLAB_NATIVE_E2E_OCI_LAYOUT must name an OCI Image Layout directory"),
    );
    let runc = PathBuf::from(
        std::env::var_os("RUNLAB_NATIVE_E2E_RUNC").unwrap_or_else(|| "/usr/local/bin/runc".into()),
    );
    let store = Arc::new(MemoryStore::default());
    let initial = import_oci_layout(store.as_ref(), &layout);
    let workspace_owner = tempfile::tempdir().expect("workspace owner");
    let workspace = workspace_owner
        .path()
        .join("state-path-long-enough-to-exceed-the-filesystem-unix-socket-address-limit")
        .join("engine");
    fs::create_dir_all(&workspace).expect("long Engine workspace");
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).expect("private workspace");
    assert!(
        workspace
            .join("invocations/run-engine-native-XXXXXX/runtime/run-engine-1234567-0-0.pidfd.sock")
            .as_os_str()
            .as_bytes()
            .len()
            >= 108,
        "fixture must exceed the Linux pathname Unix socket address limit"
    );
    let engine = Arc::new(NativeEngine::new(
        store.clone(),
        &workspace,
        runc,
        OperationTimeouts::default(),
    ));

    let cgroups_before = current_process_engine_cgroups();
    let first = engine
            .run(
                e2e_input(
                    &initial,
                    "stdio-delta",
                    "IFS= read -r line; mkdir -p /result; printf 'out:%s' \"$line\"; printf err >&2; printf delta >/result/value; cat /proc/self/cgroup >/result/cgroup; test \"$(wc -l </proc/net/route)\" -eq 1; printf ignored >/runtime-created/nested/ephemeral; sleep .2; exit 7",
                    b"hello\n",
                    None,
                ),
                CancellationToken::new(),
            )
            .expect("nonzero workload is a complete RunOutput");
    let program = &first.programs()[&ProgramId::primary()];
    assert_eq!(program.create().status(), OperationStatus::Succeeded);
    assert_eq!(program.start().status(), OperationStatus::Succeeded);
    assert!(matches!(
        program.process(),
        ProcessResult::Exited { code: 7, .. }
    ));
    assert!(matches!(
        first.execution().interval(),
        ExecutionInterval::Entered { .. }
    ));
    assert_eq!(
        program.stdout().facts().expect("stdout facts").bytes(),
        b"out:hello"
    );
    assert_eq!(
        program.stderr().facts().expect("stderr facts").bytes(),
        b"err"
    );
    assert_eq!(
        program
            .stdin()
            .write()
            .facts()
            .expect("stdin facts")
            .bytes_written(),
        6
    );
    let final_image = program.final_environment().value().unwrap_or_else(|| {
        panic!(
            "final image unavailable: {:?}; errors: {:?}",
            program.final_environment().unavailable_reason(),
            program.errors().collect::<Vec<_>>()
        )
    });
    assert_final_delta(store.as_ref(), final_image);
    assert_engine_workspace_clean(&workspace);
    assert_eq!(
        current_process_engine_cgroups(),
        cgroups_before,
        "owned cgroup leaked"
    );

    let target = EgressTarget::start(
        1,
        b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\negress-ok",
    );
    let egress_output = engine.run(
        e2e_input_with_network(
            &initial,
            "egress",
            &format!(
                "command -v wget >/dev/null || {{ printf 'fixture lacks wget\\n' >&2; exit 127; }}; wget -qO- http://{HOST_ADDRESS}:{}/",
                target.port()
            ),
            Network::Egress,
        ),
        CancellationToken::new(),
    );
    let connections = target.finish();
    let egress_output = egress_output.expect("egress output");
    let egress = &egress_output.programs()[&ProgramId::primary()];
    assert_eq!(
        egress.create().status(),
        OperationStatus::Succeeded,
        "egress create failed: {:?}",
        egress.errors().collect::<Vec<_>>()
    );
    assert_eq!(
        egress.start().status(),
        OperationStatus::Succeeded,
        "egress start failed: {:?}",
        egress.errors().collect::<Vec<_>>()
    );
    assert!(
        matches!(egress.process(), ProcessResult::Exited { code: 0, .. }),
        "egress process failed: {:?}; errors: {:?}",
        egress.process(),
        egress.errors().collect::<Vec<_>>()
    );
    assert_eq!(
        egress.stdout().facts().expect("egress stdout").bytes(),
        b"egress-ok"
    );
    assert_eq!(egress.errors().count(), 0, "egress cleanup polluted output");
    assert_eq!(
        egress_output.execution().errors().count(),
        0,
        "egress polluted execution output"
    );
    assert_eq!(connections, 1, "egress target accepted no connection");
    assert_engine_workspace_clean(&workspace);

    let target = EgressTarget::start(
        2,
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nconcurrent",
    );
    let port = target.port();
    let barrier = Arc::new(Barrier::new(2));
    let workers = (0..2)
        .map(|index| {
            let engine = Arc::clone(&engine);
            let initial = initial.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                engine.run(
                    e2e_input_with_network(
                        &initial,
                        &format!("concurrent-egress-{index}"),
                        &format!(
                            "command -v wget >/dev/null || {{ printf 'fixture lacks wget\\n' >&2; exit 127; }}; wget -qO- http://{HOST_ADDRESS}:{port}/"
                        ),
                        Network::Egress,
                    ),
                    CancellationToken::new(),
                )
            })
        })
        .collect::<Vec<_>>();
    let outputs = workers
        .into_iter()
        .map(JoinHandle::join)
        .collect::<Vec<_>>();
    let connections = target.finish();
    for output in outputs {
        let output = output
            .expect("concurrent egress worker")
            .expect("concurrent egress RunOutput");
        let program = &output.programs()[&ProgramId::primary()];
        assert!(matches!(
            program.process(),
            ProcessResult::Exited { code: 0, .. }
        ));
        assert_eq!(
            program.stdout().facts().expect("concurrent stdout").bytes(),
            b"concurrent"
        );
        assert_eq!(
            program.errors().count(),
            0,
            "concurrent egress polluted Program output"
        );
        assert_eq!(
            output.execution().errors().count(),
            0,
            "concurrent egress polluted execution output"
        );
    }
    assert_eq!(
        connections, 2,
        "concurrent egress target accepted {connections} connections"
    );
    assert_engine_workspace_clean(&workspace);

    let file_mount_source = tempfile::NamedTempFile::new().expect("file mount source");
    fs::write(file_mount_source.path(), b"task").expect("file mount contents");
    let file_bind = engine
        .run(
            e2e_input_with_file_bind(&initial, file_mount_source.path()),
            CancellationToken::new(),
        )
        .expect("file bind mount output");
    assert!(
        file_bind.programs()[&ProgramId::primary()]
            .final_environment()
            .value()
            .is_some(),
        "file bind mount artifact prevented final environment capture"
    );
    assert_engine_workspace_clean(&workspace);

    let secrets = engine
        .run(e2e_input_with_secrets(&initial), CancellationToken::new())
        .expect("Secret delivery output");
    let secret_program = &secrets.programs()[&ProgramId::primary()];
    assert!(matches!(
        secret_program.process(),
        ProcessResult::Exited { code: 0, .. }
    ));
    let secret_final = secret_program
        .final_environment()
        .value()
        .expect("final image after Secret delivery");
    assert_final_delta(store.as_ref(), secret_final);
    assert_engine_workspace_clean(&workspace);

    let exit_zero = engine
        .run(
            e2e_input(&initial, "exit-zero", "sleep .2; exit 0", b"", None),
            CancellationToken::new(),
        )
        .expect("exit zero output");
    assert!(matches!(
        exit_zero.programs()[&ProgramId::primary()].process(),
        ProcessResult::Exited { code: 0, .. }
    ));

    let signaled = engine
        .run(
            e2e_input(
                &initial,
                "forced-signal",
                "trap '' TERM; while :; do sleep 1; done",
                b"",
                NonZeroU64::new(100),
            ),
            CancellationToken::new(),
        )
        .expect("signal output");
    assert!(signaled.execution().timed_out());
    assert!(
        matches!(
            signaled.programs()[&ProgramId::primary()].process(),
            ProcessResult::Signaled { signal, .. } if signal.get() == 9
        ),
        "{:?}",
        signaled.programs()[&ProgramId::primary()].process()
    );

    let blocked_stdin = vec![b'x'; 10 * 1024 * 1024];
    let timed_out = engine
        .run(
            e2e_input(
                &initial,
                "timeout",
                "sleep 30",
                &blocked_stdin,
                NonZeroU64::new(100),
            ),
            CancellationToken::new(),
        )
        .expect("timeout output");
    assert!(timed_out.execution().timed_out());
    assert!(
        !timed_out.programs()[&ProgramId::primary()]
            .stop_actions()
            .is_empty()
    );
    let timed_out_stdin = timed_out.programs()[&ProgramId::primary()].stdin();
    assert_eq!(timed_out_stdin.write().status(), OperationStatus::Failed);
    assert!(
        timed_out_stdin
            .write()
            .facts()
            .expect("partial stdin facts")
            .bytes_written()
            < u64::try_from(blocked_stdin.len()).expect("stdin length")
    );
    assert_eq!(timed_out_stdin.close().status(), OperationStatus::Succeeded);

    let cancellation = CancellationToken::new();
    let cancellation_input = e2e_input(
        &initial,
        "cancel",
        "printf cancel-ready; sleep 30",
        &blocked_stdin,
        None,
    );
    let cancellation_output = engine
        .run_observed(
            &cancellation_input,
            &cancellation,
            Arc::new(CancelOnPrimaryOutput {
                cancellation: cancellation.clone(),
            }),
        )
        .expect("cancelled output");
    assert!(cancellation_output.execution().cancelled());
    assert!(
        !cancellation_output.programs()[&ProgramId::primary()]
            .stop_actions()
            .is_empty()
    );
    assert_eq!(
        cancellation_output.programs()[&ProgramId::primary()]
            .stdin()
            .write()
            .status(),
        OperationStatus::Failed
    );

    let shared_grace = RunInput::new(
        BTreeMap::from([
            (
                ProgramId::new("dependency"),
                e2e_program(
                    &initial,
                    "shared-grace-dependency",
                    "trap '' TERM; while :; do sleep 1; done",
                    b"",
                    true,
                ),
            ),
            (
                ProgramId::primary(),
                e2e_program(
                    &initial,
                    "shared-grace-primary",
                    "trap '' TERM; while :; do sleep 1; done",
                    b"",
                    true,
                ),
            ),
        ]),
        RunControls::new(NonZeroU64::new(100), Network::Isolated, true),
    )
    .expect("multi-Program input");
    let shared_grace_started = Instant::now();
    let shared_grace = engine
        .run(shared_grace, CancellationToken::new())
        .expect("multi-Program timeout output");
    let shared_grace_elapsed = shared_grace_started.elapsed();
    let term_attempts = shared_grace
        .programs()
        .values()
        .filter(|program| {
            program
                .stop_actions()
                .iter()
                .any(|action| action.signal() == StopSignal::Term)
        })
        .count();
    assert_eq!(term_attempts, 2);
    assert!(
        shared_grace_elapsed < STOP_GRACE_PERIOD + Duration::from_secs(5),
        "multi-Program stop exceeded one shared monotonic TERM grace: {shared_grace_elapsed:?}"
    );
    assert!(shared_grace.programs().values().all(|program| {
        program
            .stop_actions()
            .iter()
            .any(|action| action.signal() == StopSignal::Kill)
    }));

    let dependency_create_failure = RunInput::new(
        BTreeMap::from([
            (
                ProgramId::new("dependency"),
                e2e_program_with_options(&initial, "invalid-dependency", "exit 0", b"", "/", true),
            ),
            (
                ProgramId::primary(),
                e2e_program(&initial, "blocked-primary", "exit 0", b"", true),
            ),
        ]),
        RunControls::new(None, Network::Isolated, true),
    )
    .expect("dependency failure input");
    let dependency_create_failure = engine
        .run(dependency_create_failure, CancellationToken::new())
        .expect("dependency create failure is structured output");
    let dependency = &dependency_create_failure.programs()[&ProgramId::new("dependency")];
    let primary = &dependency_create_failure.programs()[&ProgramId::primary()];
    assert_eq!(dependency.create().status(), OperationStatus::Failed);
    assert_eq!(primary.create().status(), OperationStatus::NotAttempted);
    assert_eq!(primary.start().status(), OperationStatus::NotAttempted);
    assert!(
        !dependency_create_failure
            .execution()
            .interval()
            .was_entered()
    );

    let large_output = RunInput::new(
            BTreeMap::from([
                (
                    ProgramId::new("dependency"),
                    e2e_program(
                        &initial,
                        "large-output-dependency",
                        "dd if=/dev/zero bs=1048576 count=2 2>/dev/null; trap '' TERM; while :; do sleep 1; done",
                        b"",
                        true,
                    ),
                ),
                (
                    ProgramId::primary(),
                    e2e_program(
                        &initial,
                        "large-output-primary",
                        "trap '' TERM; while :; do sleep 1; done",
                        b"",
                        true,
                    ),
                ),
            ]),
            RunControls::new(NonZeroU64::new(100), Network::Isolated, true),
        )
        .expect("large-output multi-Program input");
    let large_output = engine
        .run(large_output, CancellationToken::new())
        .expect("large-output multi-Program timeout");
    assert_eq!(
        large_output.programs()[&ProgramId::new("dependency")]
            .stdout()
            .facts()
            .expect("dependency stdout")
            .bytes()
            .len(),
        2 * 1024 * 1024
    );

    let first_input = e2e_input_uncgrouped(
        &initial,
        "first-concurrent",
        "printf first; sleep .2; exit 0",
        b"",
        None,
    );
    let second_input = e2e_input_uncgrouped(
        &initial,
        "second-concurrent",
        "printf second; sleep .2; exit 3",
        b"",
        None,
    );
    let first_engine = Arc::clone(&engine);
    let first_worker =
        thread::spawn(move || first_engine.run(first_input, CancellationToken::new()));
    let second_engine = Arc::clone(&engine);
    let second_worker =
        thread::spawn(move || second_engine.run(second_input, CancellationToken::new()));
    let first_concurrent = first_worker
        .join()
        .expect("first worker")
        .expect("first concurrent Run");
    let second_concurrent = second_worker
        .join()
        .expect("second worker")
        .expect("second concurrent Run");
    let first_program = &first_concurrent.programs()[&ProgramId::primary()];
    let second_program = &second_concurrent.programs()[&ProgramId::primary()];
    assert_eq!(
        first_program
            .stdout()
            .facts()
            .expect("first stdout")
            .bytes(),
        b"first"
    );
    assert!(matches!(
        first_program.process(),
        ProcessResult::Exited { code: 0, .. }
    ));
    assert_eq!(
        second_program
            .stdout()
            .facts()
            .expect("second stdout")
            .bytes(),
        b"second"
    );
    assert!(matches!(
        second_program.process(),
        ProcessResult::Exited { code: 3, .. }
    ));

    let (_create_wrapper_workspace, create_wrapper) =
        delayed_runc_wrapper(&engine.runc_executable, "create");
    let create_deadline_engine = NativeEngine::new(
        store.clone(),
        &workspace,
        create_wrapper,
        OperationTimeouts::default()
            .with_create(Duration::from_millis(10))
            .expect("minimum create timeout"),
    );
    let create_deadline = create_deadline_engine
        .run(
            e2e_input(&initial, "create-deadline", "exit 0", b"", None),
            CancellationToken::new(),
        )
        .expect("create timeout is structured output");
    let create_deadline = &create_deadline.programs()[&ProgramId::primary()];
    assert_eq!(create_deadline.create().status(), OperationStatus::Unknown);
    assert_eq!(
        create_deadline.start().status(),
        OperationStatus::NotAttempted
    );
    assert!(
        create_deadline
            .create()
            .errors()
            .any(|error| error.message().contains("create deadline exceeded"))
    );

    let (_noisy_wrapper_workspace, noisy_wrapper) = noisy_runc_wrapper(&engine.runc_executable);
    let noisy_engine = NativeEngine::new(
        store.clone(),
        &workspace,
        noisy_wrapper,
        OperationTimeouts::default(),
    );
    let noisy_create = noisy_engine
        .run(
            e2e_input(&initial, "noisy-create", "exit 0", b"", None),
            CancellationToken::new(),
        )
        .expect("bounded create diagnostics remain structured output");
    let noisy_program = &noisy_create.programs()[&ProgramId::primary()];
    assert_eq!(noisy_program.create().status(), OperationStatus::Unknown);
    assert!(
        noisy_program
            .create()
            .errors()
            .any(|error| error.message().contains("diagnostics exceeded"))
    );

    let (_start_wrapper_workspace, start_wrapper) =
        delayed_runc_wrapper(&engine.runc_executable, "start");
    let start_deadline_engine = NativeEngine::new(
        store.clone(),
        &workspace,
        start_wrapper,
        OperationTimeouts::default()
            .with_start(Duration::from_millis(10))
            .expect("minimum start timeout"),
    );
    let start_deadline = start_deadline_engine
        .run(
            e2e_input(&initial, "start-deadline", "sleep 1", b"", None),
            CancellationToken::new(),
        )
        .expect("start timeout is structured output");
    let start_program = &start_deadline.programs()[&ProgramId::primary()];
    assert_eq!(start_program.create().status(), OperationStatus::Succeeded);
    assert_eq!(start_program.start().status(), OperationStatus::Unknown);
    assert!(start_deadline.execution().interval().was_entered());
    assert!(
        start_program
            .start()
            .errors()
            .any(|error| error.message().contains("start deadline exceeded"))
    );

    let create_failure = engine
        .run(
            e2e_input_with_invalid_rlimit(&initial, "create-failure", "exit 0"),
            CancellationToken::new(),
        )
        .expect("create failure is structured output");
    let create_failure = &create_failure.programs()[&ProgramId::primary()];
    assert_eq!(create_failure.create().status(), OperationStatus::Failed);
    assert_eq!(
        create_failure.start().status(),
        OperationStatus::NotAttempted
    );
    assert_eq!(
        create_failure.stdout().status(),
        OperationStatus::NotAttempted
    );
    assert_eq!(
        create_failure.stderr().status(),
        OperationStatus::NotAttempted
    );
    assert!(
        create_failure
            .create()
            .errors()
            .any(|error| error.message().contains("runc log:"))
    );
    assert!(
        create_failure.create().errors().any(|error| {
            let message = error.message().to_ascii_lowercase();
            message.contains("rlimit")
        }),
        "create failure did not retain the target rlimit diagnostic: {:?}",
        create_failure.create().errors().collect::<Vec<_>>()
    );
    assert_engine_workspace_clean(&workspace);
    assert_eq!(
        current_process_engine_cgroups(),
        cgroups_before,
        "owned cgroup leaked"
    );
}

struct EgressTarget {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<std::io::Result<usize>>,
}

struct CancelOnPrimaryOutput {
    cancellation: CancellationToken,
}

impl EngineObserver for CancelOnPrimaryOutput {
    fn program_output(
        &self,
        program_id: &ProgramId,
        stream: ProgramStream,
        _byte_offset: u64,
        _bytes: &[u8],
    ) {
        if program_id == &ProgramId::primary() && stream == ProgramStream::Stdout {
            self.cancellation.cancel();
        }
    }
}

impl EgressTarget {
    fn start(expected_connections: usize, response: &'static [u8]) -> Self {
        let listener = TcpListener::bind(("0.0.0.0", 0)).expect("egress target");
        listener
            .set_nonblocking(true)
            .expect("nonblocking egress target");
        let port = listener.local_addr().expect("egress target address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut accepted = 0;
            while accepted < expected_connections {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        connection.write_all(response)?;
                        accepted += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(accepted)
        });
        Self { port, stop, worker }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn finish(self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.worker
            .join()
            .expect("egress target thread")
            .expect("egress target listener")
    }
}
