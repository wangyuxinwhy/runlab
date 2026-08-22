#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tar::{Builder, EntryType, Header};
use tempfile::TempDir;

const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

struct RootlessFixture {
    state: TempDir,
    tools: TempDir,
    agent: PathBuf,
    initial: String,
    runtime: PathBuf,
}

impl RootlessFixture {
    fn new() -> Self {
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "test must run as non-root"
        );
        let state = tempfile::tempdir().expect("state");
        let tools = tempfile::tempdir().expect("tools");
        copy_tool(tools.path(), "runc");
        if let Some(aa_exec) = find_tool("aa-exec") {
            copy_tool_from(tools.path(), "aa-exec", &aa_exec);
        }
        let agent = compile_agent(state.path());
        let initial = write_image(state.path(), &agent);
        let runtime = state.path().join("runtime.json");
        fs::write(&runtime, runtime_config()).expect("Runtime Config");
        Self {
            state,
            tools,
            agent,
            initial,
            runtime,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        runlab(self.tools.path(), self.state.path(), arguments)
    }
}

#[test]
#[ignore = "requires non-root Linux, static cc, runc 1.5.1, and an available rootless user namespace realization"]
fn rootless_native_cli_preserves_oci_inputs_and_captures_logical_ownership() {
    let fixture = RootlessFixture::new();
    let stdin = fixture.state.path().join("stdin");
    fs::write(&stdin, b"rootless\0stdin\n").expect("stdin");

    let started = fixture.run(&[
        "run",
        "start",
        &fixture.initial,
        "--runtime-config",
        path(&fixture.runtime),
        "--stdin",
        path(&stdin),
    ]);
    let started = json_output(&started, 0);
    assert_eq!(started["process"]["facts"]["exit_code"], 7);
    assert_eq!(started["process"]["facts"]["oom_killed"], Value::Null);
    assert_eq!(started["operation_errors"], json!([]));
    let run_id = started["run_id"].as_str().expect("Run ID");
    let final_manifest = started["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("Final Manifest");

    let terminal = json_output(&fixture.run(&["run", "get", run_id]), 0);
    assert_rootless_runtime_facts(&terminal, fixture.state.path(), run_id);
    assert_rootless_outputs(&fixture, run_id, final_manifest);
    assert_logical_ownership(&fixture, final_manifest);
    assert_recovery_empty(fixture.state.path());
}

fn assert_rootless_runtime_facts(terminal: &Value, state: &Path, run_id: &str) {
    assert_eq!(
        terminal["backend"]["details"]["filesystem"]["kind"],
        "writable_materialized"
    );
    assert_eq!(
        terminal["backend"]["details"]["filesystem"]["host_uid"],
        u64::from(rustix::process::geteuid().as_raw())
    );
    assert!(matches!(
        terminal["backend"]["details"]["runtime_invocation"]["kind"].as_str(),
        Some("direct" | "apparmor_profile")
    ));
    assert_eq!(
        terminal["backend"]["details"]["runtime_config"]["kind"],
        "rootless_single_id"
    );

    let database = rusqlite::Connection::open(state.join("runs.sqlite3")).expect("Run database");
    let accepted: Vec<u8> = database
        .query_row(
            "SELECT runtime_config FROM runs WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .expect("accepted Runtime Config");
    assert_ne!(
        terminal["backend"]["details"]["runtime_config"]["digest"],
        digest(&accepted)
    );
    assert!(
        terminal["backend"]["details"]["runtime_config"]["size"]
            .as_u64()
            .is_some_and(|size| size > u64::try_from(accepted.len()).expect("accepted size"))
    );
    let accepted: Value = serde_json::from_slice(&accepted).expect("accepted Runtime Config JSON");
    assert!(accepted["linux"].get("uidMappings").is_none());
    assert!(accepted["linux"].get("gidMappings").is_none());
    assert!(
        accepted["linux"]["namespaces"]
            .as_array()
            .expect("accepted namespaces")
            .iter()
            .all(|namespace| namespace["type"] != "user")
    );
}

fn assert_rootless_outputs(fixture: &RootlessFixture, run_id: &str, final_manifest: &str) {
    assert_eq!(
        read_stream(fixture.tools.path(), fixture.state.path(), run_id, "stdout"),
        b"rootless\0stdin\n"
    );
    assert_eq!(
        read_stream(fixture.tools.path(), fixture.state.path(), run_id, "stderr"),
        b"rootless diagnostic\n"
    );
    assert_eq!(
        read_image_file(
            fixture.tools.path(),
            fixture.state.path(),
            final_manifest,
            "/workspace/result"
        ),
        b"captured\n"
    );
    assert_eq!(
        read_image_file(
            fixture.tools.path(),
            fixture.state.path(),
            final_manifest,
            "/hard-b"
        ),
        b"linked\n"
    );
}

fn assert_logical_ownership(fixture: &RootlessFixture, final_manifest: &str) {
    let diff = json_output(
        &fixture.run(&["image", "diff", &fixture.initial, final_manifest]),
        0,
    );
    let result = diff["filesystem"]["changes"]
        .as_array()
        .expect("filesystem changes")
        .iter()
        .find(|change| change["path"] == "/workspace/result")
        .expect("captured result metadata");
    assert_eq!(result["after"]["uid"], 0);
    assert_eq!(result["after"]["gid"], 0);
}

fn assert_recovery_empty(state: &Path) {
    assert!(
        fs::read_dir(state.join("recovery/native"))
            .expect("native recovery directory")
            .next()
            .is_none()
    );
}

#[test]
#[ignore = "requires non-root Linux, static cc, runc 1.5.1, and an available rootless user namespace realization"]
fn rootless_native_rejects_unsupported_inputs_before_acceptance() {
    let fixture = RootlessFixture::new();
    assert_rejects_rootless_egress(&fixture);
    assert_rejects_rootless_resources(&fixture);
    assert_rejects_rootless_mount(&fixture);
    assert_rejects_rootless_managed_service(&fixture);
    assert_rejects_rootless_nonzero_owner(&fixture);
}

fn assert_rejects_rootless_egress(fixture: &RootlessFixture) {
    assert_preaccept_failure(
        fixture.tools.path(),
        fixture.state.path(),
        &[
            "run",
            "start",
            &fixture.initial,
            "--runtime-config",
            path(&fixture.runtime),
            "--network",
            "egress",
        ],
        "rootless native execution only supports network=none",
    );
}

fn assert_rejects_rootless_resources(fixture: &RootlessFixture) {
    let resources = fixture.state.path().join("resources.json");
    let mut resources_value: Value =
        serde_json::from_slice(&runtime_config()).expect("Runtime Config JSON");
    resources_value["linux"]["resources"] = json!({});
    fs::write(
        &resources,
        serde_json::to_vec(&resources_value).expect("resources JSON"),
    )
    .expect("resources Runtime Config");
    assert_preaccept_failure(
        fixture.tools.path(),
        fixture.state.path(),
        &[
            "run",
            "start",
            &fixture.initial,
            "--runtime-config",
            path(&resources),
        ],
        "rootless native execution does not support linux.resources",
    );
}

fn assert_rejects_rootless_mount(fixture: &RootlessFixture) {
    let host_file = fixture.state.path().join("host-file");
    fs::write(&host_file, b"host\n").expect("host file");
    let mounts = fixture.state.path().join("host-mount.json");
    let mut mount_value: Value =
        serde_json::from_slice(&runtime_config()).expect("Runtime Config JSON");
    mount_value["mounts"]
        .as_array_mut()
        .expect("mount array")
        .push(json!({
            "destination": "/input",
            "source": path(&host_file),
            "type": "bind",
            "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
        }));
    fs::write(
        &mounts,
        serde_json::to_vec(&mount_value).expect("mount JSON"),
    )
    .expect("mount Runtime Config");
    assert_preaccept_failure(
        fixture.tools.path(),
        fixture.state.path(),
        &[
            "run",
            "start",
            &fixture.initial,
            "--runtime-config",
            path(&mounts),
        ],
        "rootless native execution does not support read-only host mounts",
    );
}

fn assert_rejects_rootless_managed_service(fixture: &RootlessFixture) {
    let managed = fixture.state.path().join("managed.json");
    fs::write(
        &managed,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "service",
            "initial_manifest": fixture.initial,
            "runtime_config_file": fixture.runtime,
            "readiness": {"kind": "tcp", "port": 1, "timeout_seconds": 1}
        }))
        .expect("Managed Service JSON"),
    )
    .expect("Managed Service declaration");
    assert_preaccept_failure(
        fixture.tools.path(),
        fixture.state.path(),
        &[
            "run",
            "start",
            &fixture.initial,
            "--runtime-config",
            path(&fixture.runtime),
            "--managed-service",
            path(&managed),
        ],
        "rootless native execution does not support Managed Service",
    );
}

fn assert_rejects_rootless_nonzero_owner(fixture: &RootlessFixture) {
    let unsupported_image = write_image_with_owner(fixture.state.path(), &fixture.agent, 1, 0);
    assert_preaccept_failure(
        fixture.tools.path(),
        fixture.state.path(),
        &[
            "run",
            "start",
            &unsupported_image,
            "--runtime-config",
            path(&fixture.runtime),
        ],
        "rootless native execution only supports Image filesystem uid=0 and gid=0",
    );
}

#[test]
#[ignore = "requires non-root Linux, static cc, runc 1.5.1, and an available rootless user namespace realization"]
fn rootless_native_reconciles_supervisor_loss() {
    let fixture = RootlessFixture::new();
    let runtime = fixture.state.path().join("wait.json");
    let mut config: Value = serde_json::from_slice(&runtime_config()).expect("Runtime Config JSON");
    config["process"]["env"] = json!(["RUNLAB_WAIT=1"]);
    fs::write(
        &runtime,
        serde_json::to_vec(&config).expect("Runtime Config JSON"),
    )
    .expect("Runtime Config");

    let mut child = runlab_command(
        fixture.tools.path(),
        fixture.state.path(),
        &[
            "run",
            "start",
            &fixture.initial,
            "--runtime-config",
            path(&runtime),
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn runlab");
    let run_id = wait_for_rootless_init(&mut child, fixture.state.path());
    let result = fixture
        .state
        .path()
        .join("recovery/native")
        .join(&run_id)
        .join("workspace/bundle/rootfs/workspace/result");
    wait_for_path(&mut child, &result);
    child.kill().expect("kill runlab supervisor");
    child.wait().expect("reap runlab supervisor");

    let reconciled = json_output(&fixture.run(&["run", "reconcile", &run_id]), 0);
    assert_eq!(reconciled["status"], "reconciled");
    assert_eq!(reconciled["terminalized"], true);
    assert_eq!(reconciled["resources_absent"], true);

    let terminal = json_output(&fixture.run(&["run", "get", &run_id]), 0);
    assert_eq!(terminal["lifecycle"], "terminal");
    assert_eq!(terminal["process"]["availability"], "unavailable");
    let final_manifest = terminal["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("reconciled Final Manifest");
    assert_eq!(
        read_image_file(
            fixture.tools.path(),
            fixture.state.path(),
            final_manifest,
            "/workspace/result",
        ),
        b"captured\n"
    );
    assert_recovery_empty(fixture.state.path());
}

fn assert_preaccept_failure(tools: &Path, state: &Path, arguments: &[&str], expected: &str) {
    let output = runlab(tools, state, arguments);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "expected {expected:?}, received {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let database = rusqlite::Connection::open(state.join("runs.sqlite3")).expect("Run database");
    let count: i64 = database
        .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
        .expect("Run count");
    assert_eq!(
        count, 0,
        "unsupported rootless input created an accepted Run"
    );
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 fixture path")
}

fn runlab(tools: &Path, state: &Path, arguments: &[&str]) -> Output {
    runlab_command(tools, state, arguments)
        .output()
        .expect("run runlab")
}

fn runlab_command(tools: &Path, state: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_runlab"));
    command
        .env_clear()
        .env("PATH", tools)
        .arg("--state")
        .arg(state)
        .args(arguments);
    command
}

fn wait_for_rootless_init(child: &mut Child, state: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let run_id = fs::read_dir(state.join("recovery/native"))
            .ok()
            .and_then(|entries| {
                entries.filter_map(Result::ok).find_map(|entry| {
                    entry
                        .path()
                        .join("workspace/runtime/init.pid")
                        .is_file()
                        .then(|| entry.file_name().to_string_lossy().into_owned())
                })
            });
        if let Some(run_id) = run_id {
            return run_id;
        }
        if let Some(status) = child.try_wait().expect("poll runlab") {
            panic!("runlab exited before runc init was observable: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill stuck runlab");
    child.wait().expect("reap stuck runlab");
    panic!("runc init was not observable within 30 seconds");
}

fn wait_for_path(child: &mut Child, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll runlab") {
            panic!(
                "runlab exited before {} was observable: {status}",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill stuck runlab");
    child.wait().expect("reap stuck runlab");
    panic!("{} was not observable within 30 seconds", path.display());
}

fn json_output(output: &Output, expected_code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

fn read_stream(tools: &Path, state: &Path, run_id: &str, stream: &str) -> Vec<u8> {
    let destination = state.join(format!("{stream}-{run_id}"));
    let output = runlab(
        tools,
        state,
        &[
            "run",
            stream,
            "get",
            run_id,
            "--output",
            destination.to_str().expect("stream output path"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(destination).expect("read captured stream")
}

fn read_image_file(tools: &Path, state: &Path, manifest: &str, source: &str) -> Vec<u8> {
    let output = state.join(format!("file-{}", source.replace('/', "-")));
    let result = runlab(
        tools,
        state,
        &[
            "image",
            "file",
            "get",
            manifest,
            source,
            "--output",
            output.to_str().expect("output path"),
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::read(output).expect("extracted Image file")
}

fn find_tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn copy_tool(directory: &Path, name: &str) {
    let source = find_tool(name).unwrap_or_else(|| panic!("required tool is unavailable: {name}"));
    copy_tool_from(directory, name, &source);
}

fn copy_tool_from(directory: &Path, name: &str, source: &Path) {
    let target = directory.join(name);
    fs::copy(source, &target).unwrap_or_else(|error| panic!("copy {name}: {error}"));
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("set {name} mode: {error}"));
}

fn compile_agent(directory: &Path) -> PathBuf {
    let source = directory.join("agent.c");
    let executable = directory.join("agent");
    fs::write(
        &source,
        br#"
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

static int write_file(const char *path, const char *bytes, size_t size) {
    int file = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (file < 0) return -1;
    if (write(file, bytes, size) != (ssize_t)size) return -1;
    return close(file);
}

int main(void) {
    char buffer[4096];
    ssize_t count;
    while ((count = read(0, buffer, sizeof(buffer))) > 0) {
        if (write(1, buffer, (size_t)count) != count) return 20;
    }
    if (count < 0) return 21;
    if (write(2, "rootless diagnostic\n", 20) != 20) return 22;
    if (mkdir("/workspace", 0755) < 0) return 23;
    if (write_file("/workspace/result", "captured\n", 9) < 0) return 24;
    if (write_file("/hard-a", "linked\n", 7) < 0) return 25;
    if (getenv("RUNLAB_WAIT") != NULL) for (;;) pause();
    return 7;
}
"#,
    )
    .expect("agent source");
    let output = Command::new("cc")
        .args(["-static", "-O2", "-o"])
        .arg(&executable)
        .arg(&source)
        .output()
        .expect("static C compiler");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

fn write_image(state: &Path, agent: &Path) -> String {
    write_image_with_owner(state, agent, 0, 0)
}

fn write_image_with_owner(state: &Path, agent: &Path, uid: u64, gid: u64) -> String {
    let layout = state.join("oci");
    let blobs = layout.join("blobs/sha256");
    fs::create_dir_all(&blobs).expect("blob directory");
    fs::write(
        layout.join("oci-layout"),
        b"{\"imageLayoutVersion\":\"1.0.0\"}\n",
    )
    .expect("oci-layout");
    fs::write(
        layout.join("index.json"),
        b"{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[]}\n",
    )
    .expect("index");

    let uncompressed = layer(agent, uid, gid);
    let diff_id = digest(&uncompressed);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&uncompressed).expect("compress Layer");
    let compressed = encoder.finish().expect("finish compression");
    let layer = put_blob(&blobs, &compressed, LAYER_MEDIA_TYPE);
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => panic!("unsupported fixture architecture: {other}"),
    };
    let config = put_blob(
        &blobs,
        &serde_json::to_vec(&json!({
            "architecture": architecture,
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": [diff_id]},
            "config": {"Entrypoint": ["/agent"], "Env": [], "WorkingDir": "/"},
            "history": []
        }))
        .expect("Image Config JSON"),
        CONFIG_MEDIA_TYPE,
    );
    let manifest = put_blob(
        &blobs,
        &serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST_MEDIA_TYPE,
            "config": config,
            "layers": [layer]
        }))
        .expect("Manifest JSON"),
        MANIFEST_MEDIA_TYPE,
    );
    manifest["digest"]
        .as_str()
        .expect("Manifest digest")
        .to_owned()
}

fn layer(agent: &Path, uid: u64, gid: u64) -> Vec<u8> {
    let mut tar = Builder::new(Vec::new());
    append_entry(
        &mut tar,
        "agent",
        EntryType::Regular,
        0o755,
        uid,
        gid,
        &fs::read(agent).expect("agent"),
    );
    append_entry(
        &mut tar,
        "hard-a",
        EntryType::Regular,
        0o644,
        uid,
        gid,
        b"before\n",
    );
    let mut hardlink = Header::new_gnu();
    hardlink.set_entry_type(EntryType::Link);
    hardlink.set_mode(0o644);
    hardlink.set_uid(uid);
    hardlink.set_gid(gid);
    hardlink.set_mtime(0);
    hardlink.set_size(0);
    hardlink.set_link_name("hard-a").expect("hardlink target");
    hardlink.set_cksum();
    tar.append_data(&mut hardlink, "hard-b", std::io::empty())
        .expect("hardlink");
    tar.finish().expect("finish Layer tar");
    tar.into_inner().expect("Layer tar bytes")
}

fn append_entry(
    tar: &mut Builder<Vec<u8>>,
    path: &str,
    kind: EntryType,
    mode: u32,
    uid: u64,
    gid: u64,
    bytes: &[u8],
) {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_uid(uid);
    header.set_gid(gid);
    header.set_mtime(0);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, path, bytes)
        .expect("Layer entry");
}

fn put_blob(directory: &Path, bytes: &[u8], media_type: &str) -> Value {
    let digest = digest(bytes);
    fs::write(directory.join(&digest[7..]), bytes).expect("blob");
    json!({"mediaType": media_type, "digest": digest, "size": bytes.len()})
}

fn digest(bytes: &[u8]) -> String {
    let mut value = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("digest");
    }
    value
}

fn runtime_config() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "ociVersion": "1.2.0",
        "root": {"path": "rootfs", "readonly": false},
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": ["/agent"],
            "env": [],
            "cwd": "/",
            "noNewPrivileges": true
        },
        "hostname": "runlab-rootless-test",
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]},
            {"destination": "/dev/pts", "type": "devpts", "source": "devpts", "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]},
            {"destination": "/dev/shm", "type": "tmpfs", "source": "shm", "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]},
            {"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue", "options": ["nosuid", "noexec", "nodev"]}
        ],
        "linux": {"namespaces": [
            {"type": "pid"},
            {"type": "network"},
            {"type": "ipc"},
            {"type": "uts"},
            {"type": "mount"},
            {"type": "cgroup"}
        ]}
    }))
    .expect("Runtime Config JSON")
}
