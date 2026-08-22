use std::env;
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension as _};
use serde_json::{Value, json};

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(arguments)
        .output()
        .expect("runlab process")
}

fn json_output(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "runlab failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("runlab JSON output")
}

fn manifest_digest(value: &Value) -> &str {
    value["manifest"]["digest"]
        .as_str()
        .expect("manifest digest")
}

#[test]
#[ignore = "requires RUNLAB_TEST_IMAGE and a local Docker backend"]
#[allow(
    clippy::too_many_lines,
    reason = "one linear subprocess test intentionally demonstrates the complete public workflow"
)]
fn real_docker_run_preserves_bytes_filesystem_and_interruption() {
    let source_image = env::var("RUNLAB_TEST_IMAGE").expect("RUNLAB_TEST_IMAGE");
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    let state_text = state.to_str().expect("state path");

    let imported = json_output(&run(&[
        "--state",
        state_text,
        "docker",
        "image",
        "import",
        &source_image,
    ]));
    let initial = manifest_digest(&imported).to_owned();
    let checkout = json_output(&run(&[
        "--state", state_text, "docker", "image", "checkout", "create", &initial,
    ]));
    let checkout_id = checkout["checkout_id"].as_str().expect("checkout id");
    let changed = Command::new("docker")
        .args([
            "exec",
            checkout_id,
            "/bin/sh",
            "-c",
            "mkdir -p /workspace && printf 'prepared\\n' > /workspace/answer.md",
        ])
        .output()
        .expect("docker exec");
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let committed_output = run(&[
        "--state",
        state_text,
        "docker",
        "image",
        "checkout",
        "commit",
        checkout_id,
    ]);
    let _ = Command::new("docker")
        .args(["container", "rm", "--force", checkout_id])
        .output();
    let committed = json_output(&committed_output);
    let child = manifest_digest(&committed).to_owned();
    assert_eq!(committed["parent_manifest"], initial);
    assert_eq!(committed["added_layers"].as_array().map(Vec::len), Some(1));

    let config_path = directory.path().join("config.json");
    let generated = run(&[
        "--state",
        state_text,
        "runtime-config",
        "create",
        &child,
        "--output",
        config_path.to_str().expect("config path"),
    ]);
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let mut config: Value =
        serde_json::from_slice(&fs::read(&config_path).expect("runtime config")).expect("JSON");
    config
        .as_object_mut()
        .expect("Runtime Config object")
        .remove("mounts");
    config["process"]["args"] = json!([
        "/bin/sh",
        "-c",
        "cat; cat /workspace/answer.md; printf 'diagnostic\\n' >&2; printf 'run\\n' >> /workspace/result.txt; exit 7"
    ]);
    config["process"]["cwd"] = json!("/workspace");
    fs::write(&config_path, serde_json::to_vec(&config).expect("JSON")).expect("write config");
    let stdin_path = directory.path().join("stdin.bin");
    fs::write(&stdin_path, b"input\n").expect("stdin bytes");
    assert_egress_rejected_before_acceptance(&state, &child, &config_path);

    let executed = json_output(&run(&[
        "--state",
        state_text,
        "run",
        "start",
        &child,
        "--backend",
        "docker",
        "--runtime-config",
        config_path.to_str().expect("config path"),
        "--stdin",
        stdin_path.to_str().expect("stdin path"),
    ]));
    assert_eq!(executed["process"]["availability"], "available");
    assert_eq!(
        executed["process"]["facts"]["terminal_outcome"],
        "process_exited"
    );
    assert_eq!(executed["process"]["facts"]["exit_code"], 7);
    assert_eq!(executed["final_image"]["availability"], "available");
    let final_manifest = executed["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("final manifest");
    let result_path = directory.path().join("result.txt");
    let copied = run(&[
        "--state",
        state_text,
        "image",
        "file",
        "get",
        final_manifest,
        "/workspace/result.txt",
        "--output",
        result_path.to_str().expect("result path"),
    ]);
    assert!(
        copied.status.success(),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );
    assert_eq!(fs::read(&result_path).expect("result bytes"), b"run\n");

    let run_id = executed["run_id"].as_str().expect("run id");
    assert_stream(
        &state,
        run_id,
        "stdout",
        b"input\nprepared\n",
        directory.path(),
    );
    assert_stream(&state, run_id, "stderr", b"diagnostic\n", directory.path());
    let connection = Connection::open(state.join("runs.sqlite3")).expect("Run database");
    let row = connection
        .query_row(
            "SELECT lifecycle, stdout, stderr FROM runs WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .expect("terminal Run row");
    assert_eq!(
        row,
        (
            "terminal".to_owned(),
            b"input\nprepared\n".to_vec(),
            b"diagnostic\n".to_vec()
        )
    );

    let limited_run_id = assert_capture_limit(&state, &child, &config, directory.path());
    let timed_run_id = assert_timeout(&state, &child, &config, directory.path());

    config["process"]["args"] = json!(["/bin/sh", "-c", "sleep 60"]);
    let interrupt_config = directory.path().join("interrupt.json");
    fs::write(
        &interrupt_config,
        serde_json::to_vec(&config).expect("JSON"),
    )
    .expect("interrupt config");
    let mut interrupted = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args([
            "--state",
            state_text,
            "run",
            "start",
            &child,
            "--backend",
            "docker",
            "--runtime-config",
            interrupt_config.to_str().expect("config path"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("interrupted runlab");
    let interrupted_id = wait_for_new_run(&state.join("runs.sqlite3"), &timed_run_id);
    wait_for_container(&interrupted_id);
    send_interrupt(&mut interrupted);
    let output = interrupted.wait_with_output().expect("interrupted output");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("interrupted JSON");
    assert_eq!(value["process"]["facts"]["terminal_outcome"], "cancelled");
    assert_ne!(limited_run_id, interrupted_id);
    let interrupted_manifest = value["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("interrupted Final Manifest");
    assert_ne!(interrupted_manifest, child);
    let initial_view = json_output(&run(&["--state", state_text, "image", "inspect", &child]));
    let interrupted_view = json_output(&run(&[
        "--state",
        state_text,
        "image",
        "inspect",
        interrupted_manifest,
    ]));
    assert_eq!(
        interrupted_view["layers"].as_array().map(Vec::len),
        initial_view["layers"]
            .as_array()
            .map(|layers| layers.len() + 1)
    );
}

fn assert_egress_rejected_before_acceptance(state: &Path, manifest: &str, config: &Path) {
    let output = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "start",
        manifest,
        "--backend",
        "docker",
        "--runtime-config",
        config.to_str().expect("config path"),
        "--network",
        "egress",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot faithfully provision"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let connection = Connection::open(state.join("runs.sqlite3")).expect("Run database");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
        .expect("Run count");
    assert_eq!(count, 0);
}

fn assert_capture_limit(state: &Path, manifest: &str, base: &Value, root: &Path) -> String {
    let mut config = base.clone();
    config["process"]["args"] = json!(["/bin/sh", "-c", "printf '0123456789abcdef'"]);
    let path = root.join("capture-limit.json");
    fs::write(&path, serde_json::to_vec(&config).expect("JSON")).expect("capture config");
    let value = json_output(&run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "start",
        manifest,
        "--backend",
        "docker",
        "--runtime-config",
        path.to_str().expect("config path"),
        "--stdout-limit-bytes",
        "8",
    ]));
    assert_eq!(
        value["process"]["facts"]["terminal_outcome"],
        "capture_limit_exceeded"
    );
    assert_eq!(value["stdout"]["availability"], "partial");
    assert_eq!(value["stdout"]["size"], 8);
    let run_id = value["run_id"].as_str().expect("run id").to_owned();
    assert_stream(state, &run_id, "stdout", b"01234567", root);
    run_id
}

fn assert_timeout(state: &Path, manifest: &str, base: &Value, root: &Path) -> String {
    let mut config = base.clone();
    config["process"]["args"] = json!(["/bin/sh", "-c", "sleep 60"]);
    let path = root.join("timeout.json");
    fs::write(&path, serde_json::to_vec(&config).expect("JSON")).expect("timeout config");
    let value = json_output(&run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "start",
        manifest,
        "--backend",
        "docker",
        "--runtime-config",
        path.to_str().expect("config path"),
        "--timeout-seconds",
        "1",
    ]));
    assert_eq!(value["process"]["facts"]["terminal_outcome"], "timed_out");
    value["run_id"].as_str().expect("run id").to_owned()
}

fn assert_stream(state: &Path, run_id: &str, name: &str, expected: &[u8], root: &Path) {
    let output_path = root.join(format!("{run_id}-{name}"));
    let output = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        name,
        "get",
        run_id,
        "--output",
        output_path.to_str().expect("output path"),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(output_path).expect("stream bytes"), expected);
}

fn wait_for_new_run(database: &Path, prior: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(connection) = Connection::open(database)
            && let Ok(value) = connection
                .query_row(
                    "SELECT run_id FROM runs WHERE accepted_at > (SELECT accepted_at FROM runs WHERE run_id = ?1) ORDER BY accepted_at DESC LIMIT 1",
                    [prior],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            && let Some(run_id) = value
        {
            return run_id;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("interrupted Run was not accepted within 30 seconds");
}

fn wait_for_container(run_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let output = Command::new("docker")
            .args([
                "container",
                "ls",
                "--all",
                "--quiet",
                "--filter",
                &format!("label=runlab.run-id={run_id}"),
            ])
            .output()
            .expect("docker container ls");
        if !output.stdout.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("Run container was not created within 30 seconds");
}

fn send_interrupt(child: &mut Child) {
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(status.success());
}
