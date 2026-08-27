use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use oci_spec::image::{Descriptor, MediaType};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const PATCH: &[u8] = b"diff --git a/example.py b/example.py\n";

#[test]
fn help_exposes_only_the_minimal_product_surface() {
    let top = run(&["--help"]);
    assert_success(&top);
    let stdout = text(&top.stdout);
    assert!(stdout.contains("image"));
    assert!(stdout.contains("run"));
    for removed in [
        "docker",
        "managed-service",
        "runtime-config",
        "schema",
        "vm",
    ] {
        assert!(
            !stdout.contains(removed),
            "legacy command leaked: {removed}"
        );
    }

    let image = run(&["image", "--help"]);
    assert_success(&image);
    let stdout = text(&image.stdout);
    for command in ["import", "list", "get", "file"] {
        assert!(stdout.contains(command), "missing image command: {command}");
    }

    let run_help = run(&["run", "--help"]);
    assert_success(&run_help);
    let stdout = text(&run_help.stdout);
    for command in ["config", "start", "get", "list"] {
        assert!(stdout.contains(command), "missing run command: {command}");
    }
    for removed in ["stdout", "stderr", "verify", "reconcile", "diff"] {
        assert!(
            !stdout.contains(removed),
            "unadmitted run command leaked: {removed}"
        );
    }

    let config_help = run(&["run", "config", "generate", "--help"]);
    assert_success(&config_help);
    let stdout = text(&config_help.stdout);
    assert!(stdout.contains("--image"));
    assert!(!stdout.contains("--network"));
    assert!(stdout.contains("no Run Protocol network policy"));
    assert!(stdout.contains("jq '.process.args"));

    let start_help = run(&["run", "start", "--help"]);
    assert_success(&start_help);
    let stdout = text(&start_help.stdout);
    assert!(stdout.contains("generated from the Image when omitted"));
    assert!(stdout.contains("possible values: isolated, egress"));
    assert!(stdout.contains("Examples:"));
}

#[test]
fn image_commands_import_discover_and_read_one_file() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout(temporary.path());

    let imported = run_with_state(
        &state,
        &[
            "image",
            "import",
            path(&layout),
            "--name",
            "swebench/example:v1",
        ],
    );
    assert_success(&imported);
    let imported = json_output(&imported);
    assert_eq!(imported["name"], "swebench/example:v1");
    let manifest = imported["manifest"]["digest"]
        .as_str()
        .expect("manifest digest")
        .to_owned();

    let listed = run_with_state(&state, &["image", "list"]);
    assert_success(&listed);
    let listed = json_output(&listed);
    assert_eq!(listed["images"][0]["name"], "swebench/example:v1");
    assert!(listed["next_after"].is_null());

    let by_name = run_with_state(&state, &["image", "get", "swebench/example:v1"]);
    assert_success(&by_name);
    assert_eq!(json_output(&by_name)["manifest"]["digest"], manifest);

    let by_digest = run_with_state(&state, &["image", "get", &manifest]);
    assert_success(&by_digest);
    assert_eq!(json_output(&by_digest)["platform"]["os"], "linux");

    let output = temporary.path().join("actual.patch");
    let file = run_with_state(
        &state,
        &[
            "image",
            "file",
            "get",
            &manifest,
            "/workspace/result.patch",
            "--output",
            path(&output),
        ],
    );
    assert_success(&file);
    assert_eq!(fs::read(&output).expect("output file"), PATCH);
    let file_json = json_output(&file);
    assert_eq!(file_json["source"], "/workspace/result.patch");
    assert_eq!(file_json["size"], PATCH.len());

    let overwrite = run_with_state(
        &state,
        &[
            "image",
            "file",
            "get",
            &manifest,
            "/workspace/result.patch",
            "--output",
            path(&output),
        ],
    );
    assert!(!overwrite.status.success());
    assert!(overwrite.stdout.is_empty());
    assert!(text(&overwrite.stderr).contains("failed to create output file"));
}

#[test]
fn generated_runtime_config_is_exact_json_and_network_independent() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout(temporary.path());
    let imported = run_with_state(
        &state,
        &["image", "import", path(&layout), "--name", "agent-base"],
    );
    assert_success(&imported);

    let arguments = ["run", "config", "generate", "--image", "agent-base"];
    let generated = run_with_state(&state, &arguments);
    assert_success(&generated);
    assert_eq!(generated.stdout.last(), Some(&b'\n'));
    let value = json_output(&generated);
    assert_eq!(
        value["process"]["args"],
        json!(["/agent/pi", "--mode", "run"])
    );
    assert_eq!(value["process"]["cwd"], "/workspace");
    assert_eq!(value["process"]["user"], json!({"uid": 1000, "gid": 1001}));
    assert_eq!(value["linux"]["namespaces"][1], json!({"type": "network"}));
    assert!(value.get("network").is_none());

    let generated_again = run_with_state(&state, &arguments);
    assert_success(&generated_again);
    assert_eq!(generated.stdout, generated_again.stdout);
}

#[test]
fn invalid_requests_do_not_emit_success_json() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");

    let missing = run_with_state(
        &state,
        &["run", "get", "550e8400-e29b-41d4-a716-446655440000"],
    );
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(text(&missing.stderr).contains("Run does not exist"));

    let bad_id = run_with_state(&state, &["run", "get", "not-a-run-id"]);
    assert!(!bad_id.status.success());
    assert!(bad_id.stdout.is_empty());
    assert!(text(&bad_id.stderr).contains("UUID v4"));
}

fn create_layout(root: &Path) -> PathBuf {
    let layout = root.join("layout");
    fs::create_dir_all(layout.join("blobs/sha256")).expect("blob directory");

    let layer = layer_bytes();
    let layer_descriptor = descriptor(&MediaType::ImageLayer, &layer);
    let diff_id = sha256_digest(&layer);
    let config = serde_json::to_vec(&json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": [diff_id],
        },
        "config": {
            "User": "1000:1001",
            "Env": ["PATH=/agent/bin", "MODEL=example"],
            "Entrypoint": ["/agent/pi"],
            "Cmd": ["--mode", "run"],
            "WorkingDir": "/workspace"
        },
    }))
    .expect("config JSON");
    let config_descriptor = descriptor(&MediaType::ImageConfig, &config);
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": config_descriptor,
        "layers": [layer_descriptor],
    }))
    .expect("manifest JSON");
    let manifest_descriptor = descriptor(&MediaType::ImageManifest, &manifest);

    write_blob(&layout, &layer_descriptor, &layer);
    write_blob(&layout, &config_descriptor, &config);
    write_blob(&layout, &manifest_descriptor, &manifest);
    fs::write(
        layout.join("index.json"),
        serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "manifests": [manifest_descriptor],
        }))
        .expect("index JSON"),
    )
    .expect("write index");
    fs::write(
        layout.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .expect("write layout marker");
    layout
}

fn layer_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        let mut root = tar::Header::new_gnu();
        root.set_size(0);
        root.set_mode(0o755);
        root.set_uid(0);
        root.set_gid(0);
        root.set_mtime(0);
        root.set_entry_type(tar::EntryType::Directory);
        root.set_cksum();
        builder
            .append_data(&mut root, ".", Cursor::new([]))
            .expect("append Image root");
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(PATCH.len()).expect("patch size"));
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, "workspace/result.patch", Cursor::new(PATCH))
            .expect("append patch");
        builder.finish().expect("finish layer");
    }
    bytes
}

fn descriptor(media_type: &MediaType, bytes: &[u8]) -> Descriptor {
    serde_json::from_value(json!({
        "mediaType": media_type,
        "digest": sha256_digest(bytes),
        "size": bytes.len(),
    }))
    .expect("descriptor")
}

fn write_blob(layout: &Path, descriptor: &Descriptor, bytes: &[u8]) {
    let digest = descriptor.digest().to_string();
    let encoded = digest.strip_prefix("sha256:").expect("sha256 digest");
    fs::write(layout.join("blobs/sha256").join(encoded), bytes).expect("write blob");
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(arguments)
        .output()
        .expect("runlab process")
}

fn run_with_state(state: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_runlab"));
    command.arg("--state").arg(state).args(arguments);
    command.output().expect("runlab process")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        text(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        text(&output.stderr)
    );
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid JSON: {error}; stdout={}", text(&output.stdout)))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}
