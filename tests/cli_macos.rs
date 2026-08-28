#![cfg(target_os = "macos")]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

#[test]
fn help_exposes_the_managed_vm_product_surface() {
    let top = run(&["--help"]);
    assert_success(&top);
    let stdout = text(&top.stdout);
    assert!(!stdout.contains("--state"));
    for command in ["filesystem", "image", "run", "vm"] {
        assert!(stdout.contains(&format!("\n  {command} ")));
    }
    for removed in ["docker", "managed-service", "runtime-config", "schema"] {
        assert!(!stdout.contains(removed));
    }

    let start = run(&["run", "start", "--help"]);
    assert_success(&start);
    let stdout = text(&start.stdout);
    assert!(stdout.contains("--secret-env"));
    assert!(stdout.contains("--secret-file"));
    assert!(stdout.contains("--description"));
    assert!(stdout.contains("--label <KEY=VALUE>"));
    assert!(stdout.contains("are not execution facts"));
    assert!(stdout.contains("stderr emits an NDJSON observation stream"));
    assert!(!stdout.contains("--secret-env-file"));

    let vm = run(&["vm", "--help"]);
    assert_success(&vm);
    let stdout = text(&vm.stdout);
    assert!(!stdout.contains("--state"));
    for command in ["create", "start", "install", "stop", "status"] {
        assert!(stdout.contains(&format!("\n  {command} ")));
    }
    for deferred in ["delete", "exec", "shell"] {
        assert!(!stdout.contains(&format!("\n  {deferred} ")));
    }
}

#[test]
fn managed_vm_does_not_expose_artifact_paths_or_custom_state() {
    let install = run(&["vm", "install", "--help"]);
    assert_success(&install);
    let stdout = text(&install.stdout);
    assert!(!stdout.contains("--binary"));
    assert!(!stdout.contains("--runc"));

    let state = tempfile::tempdir().expect("temporary state");
    for command in [
        &["--state", path(state.path()), "image", "list"][..],
        &["--state", path(state.path()), "vm", "status"][..],
    ] {
        let output = run(command);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(text(&output.stderr).contains("unexpected argument '--state'"));
    }

    let environment_state = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_STATE", state.path())
        .args(["image", "list"])
        .output()
        .expect("runlab process");
    assert!(!environment_state.status.success());
    assert!(environment_state.stdout.is_empty());
    assert!(text(&environment_state.stderr).contains("does not apply"));
}

#[test]
fn state_query_is_forwarded_to_the_fixed_guest_state() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args(["image", "list", "--limit", "7", "--after", "alpha"])
        .output()
        .expect("runlab process");

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "{\"schema_version\":1,\"images\":[],\"next_after\":null}\n"
    );
    let log = fs::read_to_string(log).expect("fake limactl log");
    assert!(log.contains(&format!(
        "/usr/bin/sudo /usr/local/libexec/runlab/{}/runlab --state /var/lib/runlab image list --limit 7 --after alpha",
        env!("CARGO_PKG_VERSION")
    )));
}

#[test]
fn run_start_forwards_guest_observations_before_completion() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", log)
        .args([
            "run",
            "start",
            "--id",
            "550e8400-e29b-41d4-a716-446655440000",
            "--image",
            "agent-base",
            "--description",
            "SWE-bench replay",
            "--label",
            "suite=swe-bench",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("runlab process");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut first = String::new();
    stderr.read_line(&mut first).expect("first observation");
    let first_elapsed = started.elapsed();
    assert_eq!(
        serde_json::from_str::<Value>(&first).expect("first event")["kind"],
        "run.stream"
    );
    let mut remaining = String::new();
    stderr
        .read_to_string(&mut remaining)
        .expect("remaining events");
    let output = child.wait_with_output().expect("runlab completion");
    assert_success(&output);
    let completed_elapsed = started.elapsed();
    assert!(completed_elapsed >= Duration::from_millis(900));
    assert!(
        completed_elapsed.saturating_sub(first_elapsed) >= Duration::from_millis(750),
        "first observation arrived at {first_elapsed:?}, completion at {completed_elapsed:?}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(remaining.trim()).expect("Program event")["kind"],
        "program.stdout"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).expect("final result")["lifecycle"],
        "terminal"
    );
}

fn fake_limactl(root: &Path) -> (PathBuf, PathBuf) {
    let architecture = std::env::consts::ARCH;
    let (location, digest) = match architecture {
        "aarch64" => (
            "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-arm64.img",
            "sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc",
        ),
        "x86_64" => (
            "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-amd64.img",
            "sha256:ffe6203da54deeb6db5d2a98a83f9ec8e55f149d3f7ba622e1abe5fa966ee3d6",
        ),
        value => panic!("unsupported test architecture: {value}"),
    };
    let executable = root.join("limactl");
    let log = root.join("calls.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$RUNLAB_FAKE_LOG"
case "$*" in
  "--version") printf '%s\n' 'limactl version 2.2.0' ;;
  "list --json") printf '%s\n' '{{"name":"runlab","status":"Running","vmType":"vz","arch":"{architecture}","cpus":4,"memory":4294967296,"disk":21474836480,"limaVersion":"2.2.0","config":{{"plain":true,"mounts":[],"images":[{{"location":"{location}","arch":"{architecture}","digest":"{digest}","variant":"server"}}]}}}}' ;;
  *"__managed-vm-handshake"*) printf '%s\n' '{{"schema_version":1,"transport_version":1,"runlab_version":"{version}","os":"linux","architecture":"{architecture}","capabilities":["native-engine","state-cli"]}}' ;;
  *"/usr/local/bin/runc --version"*) printf '%s\n' 'runc version 1.5.1' ;;
  *"/proc/sys/net/ipv4/ip_forward"*) printf '1\n' ;;
  *"/usr/bin/test"*|*"/usr/bin/grep"*) exit 0 ;;
  *"run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --network isolated --description SWE-bench replay --label suite=swe-bench"*)
    printf '%s\n' '{{"kind":"run.stream","schema_version":1,"run_id":"550e8400-e29b-41d4-a716-446655440000"}}' >&2
    sleep 1
    printf '%s\n' '{{"kind":"program.stdout","observed_at":"2026-08-28T00:00:00Z","program_id":"primary","byte_offset":0,"text":"ready\\n"}}' >&2
    printf '%s\n' '{{"schema_version":1,"created":true,"run_id":"550e8400-e29b-41d4-a716-446655440000","lifecycle":"terminal","completion":null}}'
    ;;
  *"image list --limit 7 --after alpha"*) printf '%s\n' '{{"schema_version":1,"images":[],"next_after":null}}' ;;
  *) printf '%s\n' "unexpected fake limactl call: $*" >&2; exit 2 ;;
esac
"#,
        version = env!("CARGO_PKG_VERSION")
    );
    fs::write(&executable, script).expect("fake limactl");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("permissions");
    (executable, log)
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(arguments)
        .output()
        .expect("runlab process")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        text(&output.stderr)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}
