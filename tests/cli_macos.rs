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
    for command in [
        "exec",
        "filesystem",
        "image",
        "run",
        "schema",
        "query",
        "storage",
        "vm",
    ] {
        assert!(stdout.contains(&format!("\n  {command} ")));
    }
    for removed in ["docker", "managed-service", "runtime-config"] {
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
    assert!(stdout.contains("streams NDJSON observations"));
    assert!(stdout.contains("must be read-only regular files or directories"));
    assert!(stdout.contains("--detach"));
    assert!(!stdout.contains("--detached-worker"));
    assert!(!stdout.contains("--secret-env-file"));

    let cancel = run(&["run", "cancel", "--help"]);
    assert_success(&cancel);
    let stdout = text(&cancel.stdout);
    assert!(stdout.contains("request was stored"));
    assert!(stdout.contains("run get"));

    let exec = run(&["exec", "--help"]);
    assert_success(&exec);
    let stdout = text(&exec.stdout);
    assert!(stdout.contains("--image"));
    assert!(stdout.contains("run_id:null"));
    assert!(!stdout.contains("--id"));
    assert!(!stdout.contains("--description"));
    assert!(!stdout.contains("--secret-env-file"));

    let query = run(&["query", "run", "--help"]);
    assert_success(&query);
    let stdout = text(&query.stdout);
    assert!(stdout.contains("--stdin"));
    assert!(stdout.contains("--max-output-bytes"));
    assert!(stdout.contains("Only the public Relations"));

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
    let (limactl, log_path) = fake_limactl(temporary.path());
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log_path)
        .args(["image", "list", "--limit", "7", "--after", "alpha"])
        .output()
        .expect("runlab process");

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "{\"schema_version\":1,\"images\":[],\"next_after\":null}\n"
    );
    let log = fs::read_to_string(&log_path).expect("fake limactl log");
    assert!(log.contains(&format!(
        "/usr/bin/sudo /usr/local/libexec/runlab/{}/runlab --state /var/lib/runlab image list --limit 7 --after alpha",
        env!("CARGO_PKG_VERSION")
    )));

    let schema = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log_path)
        .args(["schema", "get", "runs", "--compact"])
        .output()
        .expect("runlab process");
    assert_success(&schema);
    assert_eq!(
        serde_json::from_slice::<Value>(&schema.stdout).expect("schema JSON")["objects"][0]["name"],
        "runs"
    );
    let log = fs::read_to_string(&log_path).expect("fake limactl log");
    assert!(log.contains("schema get runs --compact"));

    let cancelled = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log_path)
        .args(["run", "cancel", "550e8400-e29b-41d4-a716-446655440000"])
        .output()
        .expect("runlab process");
    assert_success(&cancelled);
    assert_eq!(
        serde_json::from_slice::<Value>(&cancelled.stdout).expect("cancel result")["cancellation_requested"],
        true
    );
    let log = fs::read_to_string(log_path).expect("fake limactl log");
    assert!(log.contains("run cancel 550e8400-e29b-41d4-a716-446655440000"));
}

#[test]
fn image_export_transfers_one_private_checked_archive() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let destination = temporary.path().join("image.oci.tar");
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args([
            "image",
            "export",
            "--image",
            "agent-base",
            "--output",
            path(&destination),
        ])
        .output()
        .expect("runlab process");

    assert_success(&output);
    assert_eq!(
        fs::read(&destination).expect("exported archive"),
        b"oci-archive"
    );
    let result = serde_json::from_slice::<Value>(&output.stdout).expect("export result");
    assert_eq!(
        PathBuf::from(result["output"].as_str().expect("output path")),
        destination.canonicalize().expect("canonical output path")
    );
    let log = fs::read_to_string(log).expect("fake limactl log");
    assert!(log.contains("/usr/bin/id -u"));
    assert!(log.contains("/usr/bin/id -g"));
    assert!(log.contains("/usr/bin/sudo /usr/bin/chown -- 501:20 /var/tmp/runlab-image-export-"));
    assert!(log.contains("/usr/bin/sudo /usr/bin/chmod 0600 -- /var/tmp/runlab-image-export-"));
}

#[test]
fn managed_vm_preserves_the_guest_structured_error() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args(["image", "get", "missing"])
        .output()
        .expect("runlab process");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = serde_json::from_slice::<Value>(&output.stderr).expect("structured error");
    assert_eq!(error["kind"], "runlab.error");
    assert_eq!(error["category"], "not_found");
    assert_eq!(error["stage"], "image_resolution");
    assert_eq!(error["accepted"], false);
    assert_eq!(error["run_created"], false);
    assert_eq!(error["retryable"], false);
    assert_eq!(error["message"], "local Image name is unknown: missing");

    let streamed = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", fake_limactl(temporary.path()).0)
        .env("RUNLAB_FAKE_LOG", temporary.path().join("streamed.log"))
        .args(["exec", "--image", "missing"])
        .output()
        .expect("runlab streamed process");
    assert!(!streamed.status.success());
    assert!(streamed.stdout.is_empty());
    let streamed_stderr = text(&streamed.stderr);
    let lines = streamed_stderr.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "remote error must not be emitted twice");
    assert_eq!(
        serde_json::from_str::<Value>(lines[0]).expect("streamed structured error")["category"],
        "not_found"
    );
}

#[test]
fn filesystem_get_transfers_files_directories_and_symlinks() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());

    let file = temporary.path().join("file.txt");
    let file_result = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args([
            "filesystem",
            "get",
            "--image",
            "agent-base",
            "/workspace/file",
            "--output",
            path(&file),
        ])
        .output()
        .expect("runlab file get");
    assert_success(&file_result);
    assert_eq!(fs::read(&file).expect("file payload"), b"payload\n");
    assert_eq!(
        serde_json::from_slice::<Value>(&file_result.stdout).expect("file result")["kind"],
        "file"
    );

    let directory = temporary.path().join("directory");
    let directory_result = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args([
            "filesystem",
            "get",
            "--image",
            "agent-base",
            "/workspace",
            "--output",
            path(&directory),
        ])
        .output()
        .expect("runlab directory get");
    assert_success(&directory_result);
    assert_eq!(
        fs::read(directory.join("nested/file.txt")).expect("directory payload"),
        b"nested\n"
    );
    assert_eq!(
        fs::read_link(directory.join("latest")).expect("directory symlink"),
        Path::new("nested/file.txt")
    );

    let link = temporary.path().join("link");
    let link_result = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args([
            "filesystem",
            "get",
            "--image",
            "agent-base",
            "/workspace/link",
            "--output",
            path(&link),
        ])
        .output()
        .expect("runlab symlink get");
    assert_success(&link_result);
    assert_eq!(
        fs::read_link(link).expect("symlink payload"),
        Path::new("file")
    );
}

#[test]
fn read_only_host_mounts_use_the_execution_mount_namespace() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let source = temporary.path().join("evidence.jsonl");
    fs::write(&source, b"evidence\n").expect("host mount source");
    let config = temporary.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "mounts": [{
                "destination": "/evidence/input.jsonl",
                "type": "bind",
                "source": source,
                "options": ["rbind", "ro"]
            }]
        }))
        .expect("runtime config"),
    )
    .expect("write runtime config");

    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", &limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args([
            "exec",
            "--image",
            "agent-base",
            "--runtime-config",
            path(&config),
        ])
        .output()
        .expect("runlab process");
    assert_success(&output);
    let calls = fs::read_to_string(&log).expect("fake limactl log");
    assert!(calls.contains("BindReadOnlyPaths=/var/tmp/runlab-mount-"));
    assert!(calls.contains(&format!(":{}", source.display())));

    let writable_config = temporary.path().join("writable.json");
    fs::write(
        &writable_config,
        serde_json::to_vec(&serde_json::json!({
            "mounts": [{
                "destination": "/evidence/input.jsonl",
                "type": "bind",
                "source": source,
                "options": ["rbind", "rw"]
            }]
        }))
        .expect("runtime config"),
    )
    .expect("write runtime config");
    let rejected = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args([
            "exec",
            "--image",
            "agent-base",
            "--runtime-config",
            path(&writable_config),
        ])
        .output()
        .expect("runlab process");
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    let error = serde_json::from_slice::<Value>(&rejected.stderr).expect("structured error");
    assert_eq!(error["category"], "invalid_input");
    assert_eq!(error["stage"], "mount_staging");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("only read-only"))
    );
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

#[test]
fn detached_run_returns_after_acceptance_while_the_worker_continues() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let accepted = temporary.path().join("accepted");
    let completed = temporary.path().join("completed");
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .env("RUNLAB_FAKE_ACCEPTED", &accepted)
        .env("RUNLAB_FAKE_COMPLETED", &completed)
        .args([
            "run",
            "start",
            "--detach",
            "--id",
            "e11ce005-0000-4000-8000-000000000005",
            "--image",
            "agent-base",
        ])
        .output()
        .expect("runlab process");
    assert_success(&output);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "detach acceptance wait took {elapsed:?}"
    );
    let result = serde_json::from_slice::<Value>(&output.stdout).expect("detached result");
    assert_eq!(result["detached"], true);
    assert_eq!(result["created"], true);
    assert_eq!(result["lifecycle"], "accepted");
    assert_eq!(
        result["recovery"],
        "runlab run get e11ce005-0000-4000-8000-000000000005"
    );
    assert!(
        !completed.exists(),
        "worker unexpectedly completed before detach returned"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !completed.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(completed.exists(), "detached worker did not complete");
}

#[test]
fn exec_forwards_an_unidentified_observation_stream_without_persistent_arguments() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .args(["exec", "--image", "agent-base"])
        .output()
        .expect("runlab process");
    assert_success(&output);

    let events = text(&output.stderr)
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("observation event"))
        .collect::<Vec<_>>();
    assert_eq!(events[0]["kind"], "run.stream");
    assert!(events[0]["run_id"].is_null());
    assert_eq!(events[1]["kind"], "program.stdout");
    let result = serde_json::from_slice::<Value>(&output.stdout).expect("exec result");
    assert_eq!(result["result"]["kind"], "output");
    assert_eq!(
        result["result"]["output"]["programs"]["primary"]["final_environment"]["availability"],
        "not_requested"
    );

    let calls = fs::read_to_string(log).expect("fake limactl log");
    assert!(calls.contains("exec --image agent-base --network isolated"));
    assert!(!calls.contains("run start"));
    assert!(!calls.contains("--id"));
}

#[test]
fn foreground_signal_cancels_the_exact_managed_vm_execution() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (limactl, log) = fake_limactl(temporary.path());
    let marker = temporary.path().join("cancelled");
    let mut child = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_LIMACTL", limactl)
        .env("RUNLAB_FAKE_LOG", &log)
        .env("RUNLAB_FAKE_CANCEL", &marker)
        .args(["exec", "--image", "agent-base"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("runlab process");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut header = String::new();
    stderr.read_line(&mut header).expect("observation header");
    assert_eq!(
        serde_json::from_str::<Value>(&header).expect("header")["kind"],
        "run.stream"
    );

    let signal = Command::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .output()
        .expect("send SIGINT");
    assert_success(&signal);
    let mut remaining = String::new();
    stderr
        .read_to_string(&mut remaining)
        .expect("remaining observations");
    let output = child.wait_with_output().expect("runlab completion");
    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).expect("exec result")["result"]["output"]["execution"]
            ["cancelled"],
        true
    );
    let calls = fs::read_to_string(log).expect("fake limactl log");
    assert!(
        calls
            .contains("/usr/bin/systemctl kill --signal=SIGINT --kill-whom=main runlab-execution-")
    );
}

// One dispatcher keeps every emulated process-boundary call visible in a single fixture.
#[allow(clippy::too_many_lines)]
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
  *"/usr/bin/df -B1 --output=used,avail /var/lib/runlab"*) printf 'Used Available\n10737418240 10737418240\n' ;;
  *"/usr/bin/id -u"*) printf '501\n' ;;
  *"/usr/bin/id -g"*) printf '20\n' ;;
  *"/usr/bin/sudo /usr/bin/chown -- 501:20 /var/tmp/runlab-image-export-"*) : ;;
  *"/usr/bin/sudo /usr/bin/chmod 0600 -- /var/tmp/runlab-image-export-"*) : ;;
  *"/usr/bin/test"*|*"/usr/bin/grep"*) exit 0 ;;
  *"/usr/bin/install -d -m 0700 /var/tmp/runlab-output-"*)
    for value do output_root="$value"; done
    mkdir -p "$output_root"
    ;;
  *"filesystem get --image agent-base /workspace/file --output /var/tmp/runlab-output-"*)
    for value do output_path="$value"; done
    printf 'payload\n' > "$output_path"
    digest=$(shasum -a 256 "$output_path" | awk '{{print $1}}')
    printf '{{"schema_version":1,"kind":"file","digest":"sha256:%s","size":8}}\n' "$digest"
    ;;
  *"filesystem get --image agent-base /workspace/link --output /var/tmp/runlab-output-"*)
    for value do output_path="$value"; done
    ln -s file "$output_path"
    printf '%s\n' '{{"schema_version":1,"kind":"symlink","target":"file"}}'
    ;;
  *"filesystem get --image agent-base /workspace --output /var/tmp/runlab-output-"*)
    for value do output_path="$value"; done
    mkdir -p "$output_path/nested"
    printf 'nested\n' > "$output_path/nested/file.txt"
    ln -s nested/file.txt "$output_path/latest"
    printf '%s\n' '{{"schema_version":1,"kind":"directory"}}'
    ;;
  *"image export --image agent-base --output /var/tmp/runlab-image-export-"*)
    for value do output_path="$value"; done
    printf 'oci-archive' > "$output_path"
    printf '%s\n' '{{"schema_version":1,"manifest":{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1}},"output":"guest"}}'
    ;;
  *"/usr/bin/tar --format=pax -cf /var/tmp/runlab-output-"*)
    while test "$1" != "-cf"; do shift; done
    archive_path="$2"
    shift 2
    while test "$1" != "-C"; do shift; done
    source_root="$2"
    COPYFILE_DISABLE=1 /usr/bin/tar -cf "$archive_path" -C "$source_root" payload
    ;;
  *"/usr/bin/sha256sum -- /var/tmp/runlab-"*)
    for value do input_path="$value"; done
    shasum -a 256 "$input_path"
    ;;
  *"/usr/bin/stat -c %s -- /var/tmp/runlab-"*)
    for value do input_path="$value"; done
    wc -c < "$input_path"
    ;;
  "copy --backend=scp runlab:/var/tmp/runlab-output-"*)
    source_path="$3"
    source_path=${{source_path#runlab:}}
    cp "$source_path" "$4"
    ;;
  "copy --backend=scp runlab:/var/tmp/runlab-image-export-"*)
    source_path="$3"
    source_path=${{source_path#runlab:}}
    cp "$source_path" "$4"
    ;;
  "copy --backend=scp "*)
    destination_path="$4"
    destination_path=${{destination_path#runlab:}}
    cp "$3" "$destination_path"
    ;;
  *" -- /usr/bin/sudo /usr/bin/rm -rf -- /var/tmp/runlab-"*)
    while test "$1" != "/usr/bin/rm"; do shift; done
    shift
    while test "$1" != "--"; do shift; done
    shift
    rm -rf -- "$@"
    ;;
  *"/usr/bin/sudo /usr/bin/rm -f -- /var/tmp/runlab-image-export-"*)
    for value do output_path="$value"; done
    rm -f -- "$output_path"
    ;;
  *"run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --network isolated --description SWE-bench replay --label suite=swe-bench"*)
    printf '%s\n' '{{"kind":"run.stream","schema_version":1,"run_id":"550e8400-e29b-41d4-a716-446655440000"}}' >&2
    sleep 1
    printf '%s\n' '{{"kind":"program.stdout","observed_at":"2026-08-28T00:00:00Z","program_id":"primary","byte_offset":0,"text":"ready\\n"}}' >&2
    printf '%s\n' '{{"schema_version":1,"created":true,"run_id":"550e8400-e29b-41d4-a716-446655440000","lifecycle":"terminal","completion":null}}'
    ;;
  *"run start --id e11ce005-0000-4000-8000-000000000005 --detached-worker --image agent-base --network isolated"*)
    : > "$RUNLAB_FAKE_ACCEPTED"
    sleep 1
    printf '%s\n' '{{"schema_version":1,"created":true,"run_id":"e11ce005-0000-4000-8000-000000000005","lifecycle":"terminal","completion":null}}'
    : > "$RUNLAB_FAKE_COMPLETED"
    ;;
  *"exec --image missing --network isolated"*)
    printf '%s\n' '{{"schema_version":1,"kind":"runlab.error","category":"not_found","stage":"image_resolution","message":"local Image name is unknown: missing","run_id":null,"accepted":false,"run_created":false,"retryable":false,"recovery":null}}' >&2
    exit 1
    ;;
  *"exec --image agent-base --network isolated"*)
    printf '%s\n' '{{"kind":"run.stream","schema_version":1,"run_id":null}}' >&2
    if test -n "$RUNLAB_FAKE_CANCEL"; then
      while test ! -e "$RUNLAB_FAKE_CANCEL"; do sleep 0.02; done
      printf '%s\n' '{{"schema_version":1,"result":{{"kind":"output","output":{{"execution":{{"interval":{{"kind":"entered"}},"timed_out":false,"cancelled":true,"errors":[]}},"programs":{{"primary":{{"final_environment":{{"availability":"not_requested"}}}}}}}}}}}}'
    else
      printf '%s\n' '{{"kind":"program.stdout","observed_at":"2026-08-28T00:00:00Z","program_id":"primary","byte_offset":0,"text":"ready\n"}}' >&2
      printf '%s\n' '{{"schema_version":1,"result":{{"kind":"output","output":{{"execution":{{"interval":{{"kind":"entered"}},"timed_out":false,"cancelled":false,"errors":[]}},"programs":{{"primary":{{"final_environment":{{"availability":"not_requested"}}}}}}}}}}}}'
    fi
    ;;
  *"/usr/bin/systemctl kill --signal=SIGINT --kill-whom=main runlab-execution-"*)
    : > "$RUNLAB_FAKE_CANCEL"
    ;;
  *"run cancel 550e8400-e29b-41d4-a716-446655440000"*) printf '%s\n' '{{"schema_version":1,"run_id":"550e8400-e29b-41d4-a716-446655440000","lifecycle":"accepted","cancellation_requested":true,"cancellation_requested_at":"2026-08-28T00:00:00Z","terminal_at":null}}' ;;
  *"run get e11ce005-0000-4000-8000-000000000005"*)
    if test -e "$RUNLAB_FAKE_ACCEPTED"; then
      printf '%s\n' '{{"schema_version":1,"run_id":"e11ce005-0000-4000-8000-000000000005","lifecycle":"accepted"}}'
    else
      printf '%s\n' '{{"schema_version":1,"kind":"runlab.error","category":"not_found","stage":"run_lookup","message":"Run does not exist","run_id":null,"accepted":false,"run_created":false,"retryable":false,"recovery":null}}' >&2
      exit 1
    fi
    ;;
  *"image list --limit 7 --after alpha"*) printf '%s\n' '{{"schema_version":1,"images":[],"next_after":null}}' ;;
  *"schema get runs --compact"*) printf '%s\n' '{{"schema_version":1,"objects":[{{"name":"runs","columns":[]}}]}}' ;;
  *"image get missing"*)
    printf '%s\n' '{{"schema_version":1,"kind":"runlab.error","category":"not_found","stage":"image_resolution","message":"local Image name is unknown: missing","run_id":null,"accepted":false,"run_created":false,"retryable":false,"recovery":null}}' >&2
    exit 1
    ;;
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
