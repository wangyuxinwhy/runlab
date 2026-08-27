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
    assert!(stdout.contains("filesystem"));
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
    for command in ["import", "list", "get"] {
        assert!(stdout.contains(command), "missing image command: {command}");
    }
    assert!(!stdout.contains("file"));

    let filesystem = run(&["filesystem", "--help"]);
    assert_success(&filesystem);
    let stdout = text(&filesystem.stdout);
    assert!(stdout.contains("get"));
    assert!(!stdout.contains("tree"));

    let filesystem_get = run(&["filesystem", "get", "--help"]);
    assert_success(&filesystem_get);
    let stdout = text(&filesystem_get.stdout);
    for argument in ["--run", "--image", "--program", "--output"] {
        assert!(
            stdout.contains(argument),
            "missing filesystem get argument: {argument}"
        );
    }
    assert!(stdout.contains("existing path is never overwritten"));

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
fn image_and_filesystem_commands_import_discover_and_get_paths() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout(temporary.path());
    let imported = import_image(&state, &layout, "swebench/example:v1");
    assert_eq!(imported["name"], "swebench/example:v1");
    let manifest_descriptor = imported["manifest"].clone();
    let manifest = manifest_descriptor["digest"]
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
            "filesystem",
            "get",
            "--image",
            &manifest,
            "/workspace/result.patch",
            "--output",
            path(&output),
        ],
    );
    assert_success(&file);
    assert_eq!(fs::read(&output).expect("output file"), PATCH);
    let file_json = json_output(&file);
    assert_eq!(file_json["source"]["kind"], "image");
    assert_eq!(file_json["path"], "/workspace/result.patch");
    assert_eq!(file_json["kind"], "file");
    assert_eq!(file_json["size"], PATCH.len());

    let directory = temporary.path().join("workspace");
    let directory_get = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--image",
            "swebench/example:v1",
            "/workspace",
            "--output",
            path(&directory),
        ],
    );
    assert_success(&directory_get);
    assert_eq!(
        fs::read(directory.join("result.patch")).expect("directory file"),
        PATCH
    );
    assert_eq!(json_output(&directory_get)["kind"], "directory");

    let run_id = "550e8400-e29b-41d4-a716-446655440000";
    insert_terminal_run(&state, run_id, &manifest_descriptor);
    let run_output = temporary.path().join("from-run.patch");
    let from_run = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--run",
            run_id,
            "/workspace/result.patch",
            "--output",
            path(&run_output),
        ],
    );
    assert_success(&from_run);
    assert_eq!(fs::read(&run_output).expect("Run output file"), PATCH);
    let run_json = json_output(&from_run);
    assert_eq!(run_json["source"]["kind"], "run");
    assert_eq!(run_json["source"]["run_id"], run_id);
    assert_eq!(run_json["source"]["program"], "primary");

    let overwrite = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--image",
            &manifest,
            "/workspace/result.patch",
            "--output",
            path(&output),
        ],
    );
    assert!(!overwrite.status.success());
    assert!(overwrite.stdout.is_empty());
    assert!(text(&overwrite.stderr).contains("output path already exists"));
}

fn import_image(state: &Path, layout: &Path, name: &str) -> Value {
    let imported = run_with_state(state, &["image", "import", path(layout), "--name", name]);
    assert_success(&imported);
    json_output(&imported)
}

#[cfg(unix)]
#[test]
fn filesystem_directory_get_applies_layers_whiteouts_and_symlinks() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let lower = filesystem_layer(|builder| {
        append_file(builder, "workspace/keep.txt", b"keep");
        append_file(builder, "workspace/remove.txt", b"remove");
        append_file(builder, "workspace/sub/old.txt", b"old");
        append_symlink(builder, "workspace/latest", "keep.txt");
    });
    let upper = filesystem_layer(|builder| {
        append_file(builder, "workspace/.wh.remove.txt", b"");
        append_file(builder, "workspace/sub/.wh..wh..opq", b"");
        append_file(builder, "workspace/sub/new.txt", b"new");
        append_file(builder, "workspace/added.txt", b"added");
    });
    let layout = create_layout_with_layers(temporary.path(), &[lower, upper]);
    let imported = run_with_state(
        &state,
        &["image", "import", path(&layout), "--name", "layered"],
    );
    assert_success(&imported);

    let output = temporary.path().join("workspace");
    let directory = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--image",
            "layered",
            "/workspace",
            "--output",
            path(&output),
        ],
    );
    assert_success(&directory);
    assert_eq!(fs::read(output.join("keep.txt")).expect("keep"), b"keep");
    assert_eq!(fs::read(output.join("added.txt")).expect("added"), b"added");
    assert_eq!(fs::read(output.join("sub/new.txt")).expect("new"), b"new");
    assert!(!output.join("remove.txt").exists());
    assert!(!output.join("sub/old.txt").exists());
    assert_eq!(
        fs::read_link(output.join("latest")).expect("symlink"),
        Path::new("keep.txt")
    );

    let link_output = temporary.path().join("latest");
    let symlink = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--image",
            "layered",
            "/workspace/latest",
            "--output",
            path(&link_output),
        ],
    );
    assert_success(&symlink);
    assert_eq!(json_output(&symlink)["kind"], "symlink");
    assert_eq!(
        fs::read_link(link_output).expect("direct symlink"),
        Path::new("keep.txt")
    );

    let removed_output = temporary.path().join("removed");
    let removed = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--image",
            "layered",
            "/workspace/remove.txt",
            "--output",
            path(&removed_output),
        ],
    );
    assert!(!removed.status.success());
    assert!(removed.stdout.is_empty());
    assert!(text(&removed.stderr).contains("does not exist"));
}

fn insert_terminal_run(state: &Path, run_id: &str, manifest: &Value) {
    let connection =
        rusqlite::Connection::open(state.join("runlab.sqlite3")).expect("Run database");
    let completion = json!({
        "kind": "engine_returned",
        "result": {
            "kind": "output",
            "output": {
                "programs": {
                    "primary": {
                        "final_environment": {
                            "availability": "available",
                            "value": manifest,
                        }
                    }
                }
            }
        }
    });
    connection
        .execute(
            "INSERT INTO runs(
                run_id, accepted_at, input_json, input_identity_json, terminal_at, completion_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                run_id,
                "2026-08-27T00:00:00Z",
                "{}",
                "{}",
                "2026-08-27T00:00:01Z",
                serde_json::to_string(&completion).expect("completion JSON"),
            ],
        )
        .expect("insert terminal Run");
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
    create_layout_with_layers(root, &[layer_bytes()])
}

fn create_layout_with_layers(root: &Path, layers: &[Vec<u8>]) -> PathBuf {
    let layout = root.join("layout");
    fs::create_dir_all(layout.join("blobs/sha256")).expect("blob directory");

    let layer_descriptors = layers
        .iter()
        .map(|layer| descriptor(&MediaType::ImageLayer, layer))
        .collect::<Vec<_>>();
    let diff_ids = layers
        .iter()
        .map(|layer| sha256_digest(layer))
        .collect::<Vec<_>>();
    let config = serde_json::to_vec(&json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids,
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
        "layers": layer_descriptors,
    }))
    .expect("manifest JSON");
    let manifest_descriptor = descriptor(&MediaType::ImageManifest, &manifest);

    for (descriptor, layer) in layer_descriptors.iter().zip(layers) {
        write_blob(&layout, descriptor, layer);
    }
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

fn filesystem_layer(build: impl FnOnce(&mut tar::Builder<&mut Vec<u8>>)) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        build(&mut builder);
        builder.finish().expect("finish filesystem layer");
    }
    bytes
}

fn append_file(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(bytes.len()).expect("file size"));
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new(bytes))
        .expect("append file");
}

fn append_symlink(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, target: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_link_name(target).expect("symlink target");
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new([]))
        .expect("append symlink");
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
