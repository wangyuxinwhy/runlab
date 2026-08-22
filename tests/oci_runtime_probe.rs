use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

const PROBE: &str = include_str!("fixtures/oci_runtime_linuxkit_probe.mjs");
const YOUKI_RELEASE_URL: &str =
    "https://github.com/youki-dev/youki/releases/download/v0.7.0/youki-0.7.0-aarch64-musl.tar.gz";
const YOUKI_ARCHIVE_SHA256: &str =
    "b96c05c2c82f1d20a74b611188fa120894c50a6128f73856bb371604ecb69bd0";
const YOUKI_BINARY_SHA256: &str =
    "9acced77db02503fa397cca082aa3f0e60aa9410ed70cc69344d4682dbeccbf4";

#[test]
#[ignore = "requires RUNLAB_TEST_RUNTIME_PROBE_IMAGE and privileged Docker Desktop LinuxKit access"]
fn linuxkit_runc_subprocess_lifecycle_probe() {
    let image = env::var("RUNLAB_TEST_RUNTIME_PROBE_IMAGE")
        .expect("RUNLAB_TEST_RUNTIME_PROBE_IMAGE must name an installed glibc Node 24 image");
    let container_name = format!("runlab-runc-probe-{}", std::process::id());
    let report = run_probe(&image, &container_name, None);
    assert_runc_report(&report);
}

#[test]
#[ignore = "requires RUNLAB_TEST_YOUKI, RUNLAB_TEST_RUNTIME_PROBE_IMAGE and privileged Docker Desktop LinuxKit access"]
fn linuxkit_youki_v0_7_0_subprocess_lifecycle_probe() {
    let image = env::var("RUNLAB_TEST_RUNTIME_PROBE_IMAGE")
        .expect("RUNLAB_TEST_RUNTIME_PROBE_IMAGE must name an installed glibc Node 24 image");
    let executable = env::var_os("RUNLAB_TEST_YOUKI").expect("RUNLAB_TEST_YOUKI");
    let executable = Path::new(&executable)
        .canonicalize()
        .expect("canonical Youki executable");
    assert_eq!(
        sha256_file(&executable),
        YOUKI_BINARY_SHA256,
        "RUNLAB_TEST_YOUKI must be extracted from {YOUKI_RELEASE_URL}; the release API declares archive SHA-256 {YOUKI_ARCHIVE_SHA256}"
    );
    let container_name = format!("runlab-youki-probe-{}", std::process::id());
    let report = run_probe(&image, &container_name, Some(&executable));
    assert_youki_report(&report);
}

fn run_probe(image: &str, container_name: &str, youki: Option<&Path>) -> Value {
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--rm",
        "-i",
        "--name",
        container_name,
        "--privileged",
        "--pid=host",
        "--cgroupns=host",
        "--network=none",
        "--read-only",
        "--tmpfs",
        "/tmp:exec,mode=1777",
        "--entrypoint",
        "/usr/local/bin/node",
    ]);
    if let Some(youki) = youki {
        let mut mount = OsString::from(youki.as_os_str());
        mount.push(":/run/runlab-test-youki:ro");
        command.arg("--volume").arg(mount).args([
            "--env",
            "RUNLAB_TEST_RUNTIME_PATH=/run/runlab-test-youki",
            "--env",
            "RUNLAB_TEST_RUNTIME_FAMILY=youki",
        ]);
    }
    let mut child = command
        .args([image, "--input-type=module", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start privileged LinuxKit probe");
    child
        .stdin
        .take()
        .expect("probe stdin")
        .write_all(PROBE.as_bytes())
        .expect("write probe");
    let output = wait_for_probe(child, container_name);
    assert!(
        output.status.success(),
        "probe failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("probe JSON report");
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&report).expect("format probe report")
    );
    report
}

fn assert_runc_report(report: &Value) {
    assert!(report["runtime"].as_str().is_some_and(|version| {
        version.starts_with("runc version 1.3.6\ncommit: v1.3.6-0-g491b69ba\nspec: 1.2.1\n")
    }));
    assert_eq!(report["cases"]["exact"]["status"], 0);
    assert_eq!(report["cases"]["exact"]["stdoutHex"], "006f75743a0041ff0a");
    assert_eq!(
        report["cases"]["exact"]["stderrHex"],
        "006572723a0041ff0aff"
    );
    assert_eq!(report["cases"]["exact"]["stateAbsent"], true);
    assert_eq!(report["cases"]["exitSeven"]["status"], 7);
    assert_eq!(
        report["cases"]["exitSeven"]["stdoutHex"],
        "736576656e2d6f7574"
    );
    assert_eq!(
        report["cases"]["exitSeven"]["stderrHex"],
        "736576656e2d657272"
    );
    assert_eq!(report["cases"]["exitSeven"]["stateAbsent"], true);
    assert_eq!(report["cases"]["fastExit"]["stateAbsent"], true);
    assert_eq!(report["cases"]["selfSignal"]["status"], 133);
    assert_eq!(
        report["cases"]["selfSignal"]["targetAction"],
        "process.abort"
    );
    assert_eq!(report["cases"]["cancelled"]["status"], 42);
    assert_eq!(report["cases"]["cancelled"]["trigger"], "cancel");
    assert_eq!(
        report["cases"]["cancelled"]["stderrHex"],
        "63616e63656c6c6564"
    );
    assert_eq!(report["cases"]["cancelled"]["processesGone"], true);
    assert_eq!(report["cases"]["cancelled"]["cgroupRemoved"], true);
    assert_eq!(report["cases"]["cancelled"]["stateAbsent"], true);
    assert_eq!(report["cases"]["timedOut"]["status"], 137);
    assert_eq!(report["cases"]["timedOut"]["trigger"], "deadline");
    assert_eq!(report["cases"]["timedOut"]["processesGone"], true);
    assert_eq!(report["cases"]["timedOut"]["cgroupRemoved"], true);
    assert_eq!(report["cases"]["timedOut"]["stateAbsent"], true);
    let oom = &report["cases"]["oom"];
    assert_eq!(oom["availability"], "available");
    assert_eq!(oom["requestedMemoryMax"], 201_326_592_u64);
    assert_eq!(oom["requestedMemorySwapMax"], 201_326_592_u64);
    assert_eq!(oom["observedMemoryMax"], "201326592");
    assert_eq!(oom["observedMemorySwapMax"], "0");
    assert!(oom["cgroupPath"].as_str().is_some_and(|cgroup| {
        let path = Path::new(cgroup);
        path.parent() == Some(Path::new("/sys/fs/cgroup"))
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("runlab-runc-oom-"))
    }));
    assert!(oom["oomKillDelta"].as_u64().is_some_and(|delta| delta > 0));
    assert_eq!(oom["stateRetainedUntilDelete"], true);
    assert_eq!(oom["cgroupRemovedAfterDelete"], true);
    assert_eq!(oom["stateAbsent"], true);
}

fn assert_youki_report(report: &Value) {
    assert_eq!(
        report["runtime"],
        "youki version: 0.7.0\ncommit: 0.7.0-94ba653efbb180ce04650f6ae01a8e6bc8f96d92\nspec: 1.1.0\nrustc: 1.96.0"
    );
    assert_eq!(report["cases"]["exact"]["status"], 0);
    assert_eq!(report["cases"]["exact"]["stdoutHex"], "006f75743a0041ff0a");
    assert_eq!(
        report["cases"]["exact"]["stderrHex"],
        "006572723a0041ff0aff"
    );
    assert_eq!(report["cases"]["exitSeven"]["status"], 7);
    assert_eq!(
        report["cases"]["exitSeven"]["stdoutHex"],
        "736576656e2d6f7574"
    );
    assert_eq!(
        report["cases"]["exitSeven"]["stderrHex"],
        "736576656e2d657272"
    );
    assert_eq!(report["cases"]["exitSeven"]["stateAbsent"], true);
    assert_eq!(report["cases"]["fastExit"]["status"], 0);
    assert_eq!(report["cases"]["fastExit"]["stateAbsent"], true);
    assert_eq!(report["cases"]["selfSignal"]["status"], 5);
    assert_eq!(report["cases"]["selfSignal"]["stateAbsent"], true);
    assert_eq!(report["cases"]["cancelled"]["status"], 42);
    assert!(
        report["cases"]["cancelled"]["observedPids"]
            .as_u64()
            .is_some_and(|count| count >= 2)
    );
    assert_eq!(report["cases"]["cancelled"]["trigger"], "cancel");
    assert_eq!(
        report["cases"]["cancelled"]["stderrHex"],
        "63616e63656c6c6564"
    );
    assert_eq!(report["cases"]["cancelled"]["processesGone"], true);
    assert_eq!(report["cases"]["cancelled"]["cgroupRemoved"], true);
    assert_eq!(report["cases"]["cancelled"]["stateAbsent"], true);
    assert_eq!(report["cases"]["timedOut"]["status"], 9);
    assert!(
        report["cases"]["timedOut"]["observedPids"]
            .as_u64()
            .is_some_and(|count| count >= 2)
    );
    assert_eq!(report["cases"]["timedOut"]["trigger"], "deadline");
    assert_eq!(report["cases"]["timedOut"]["processesGone"], true);
    assert_eq!(report["cases"]["timedOut"]["cgroupRemoved"], true);
    assert_eq!(report["cases"]["timedOut"]["stateAbsent"], true);
    let oom = &report["cases"]["oom"];
    assert_eq!(oom["availability"], "unavailable");
    assert_eq!(
        oom["reason"],
        "Youki v0.7.0 left memory.swap.max unlimited after accepting swap=limit"
    );
    assert_eq!(oom["requestedMemoryMax"], 201_326_592_u64);
    assert_eq!(oom["requestedMemorySwapMax"], 201_326_592_u64);
    assert_eq!(oom["observedMemoryMax"], "201326592");
    assert_eq!(oom["observedMemorySwapMax"], "max");
    assert!(
        oom["observedMemorySwapCurrent"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value > 0)
    );
    assert_eq!(oom["observedOomKillDelta"], 0);
    assert_eq!(oom["cgroupRemovedAfterControlKill"], true);
    assert_eq!(oom["stateAbsent"], true);
}

fn sha256_file(path: &Path) -> String {
    let mut file = File::open(path).expect("open Youki executable");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("read Youki executable");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut hex, "{byte:02x}").expect("format Youki digest");
    }
    hex
}

fn wait_for_probe(mut child: Child, container_name: &str) -> Output {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        if child.try_wait().expect("poll probe").is_some() {
            return child.wait_with_output().expect("collect probe output");
        }
        if Instant::now() >= deadline {
            let _ = Command::new("docker")
                .args(["container", "rm", "--force", container_name])
                .output();
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed out probe");
            panic!(
                "probe exceeded 60 seconds\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}
