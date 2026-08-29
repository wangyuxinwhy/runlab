#![cfg(not(target_os = "macos"))]

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
    let top_stdout = text(&top.stdout);
    assert!(top_stdout.contains("image"));
    assert!(top_stdout.contains("run"));
    assert!(top_stdout.contains("filesystem"));
    for command in ["exec", "schema", "query", "storage"] {
        assert!(top_stdout.contains(command), "missing command: {command}");
    }
    for removed in ["docker", "managed-service", "runtime-config"] {
        assert!(
            !top_stdout.contains(removed),
            "legacy command leaked: {removed}"
        );
    }
    #[cfg(target_os = "macos")]
    assert!(top_stdout.contains("vm"));
    #[cfg(not(target_os = "macos"))]
    assert!(!top_stdout.contains("vm"));

    let image = run(&["image", "--help"]);
    assert_success(&image);
    let stdout = text(&image.stdout);
    for command in ["import", "list", "get", "export"] {
        assert!(stdout.contains(command), "missing image command: {command}");
    }
    assert!(!stdout.contains("file"));

    let image_import = run(&["image", "import", "--help"]);
    assert_success(&image_import);
    let stdout = text(&image_import.stdout);
    for argument in ["--description", "--label <KEY=VALUE>"] {
        assert!(
            stdout.contains(argument),
            "missing metadata argument: {argument}"
        );
    }
    assert!(stdout.contains("do not change the OCI Manifest digest"));
    assert!(stdout.contains("Keys are not interpreted"));

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
    for command in ["config", "start", "cancel", "get", "list"] {
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
    assert!(stdout.contains("compact JSON summary"));
    assert!(stdout.contains("stderr emits an NDJSON observation stream"));
    assert!(stdout.contains("Use run get for the complete persisted Run record"));
    assert!(stdout.contains("--secret-env"));
    assert!(stdout.contains("--secret-file"));
    assert!(stdout.contains("--description"));
    assert!(stdout.contains("--label <KEY=VALUE>"));
    assert!(stdout.contains("are not execution facts"));
    assert!(stdout.contains("Keys are not interpreted"));
    assert!(stdout.contains("Examples:"));
}

#[test]
fn image_export_round_trips_catalog_and_final_images_without_overwrite() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout_with_layers(temporary.path(), &[layer_bytes()]);
    let imported = run_with_state(
        &state,
        &["image", "import", path(&layout), "--name", "base"],
    );
    assert_success(&imported);
    let manifest = json_output(&imported)["manifest"].clone();

    let catalog_archive = temporary.path().join("base.oci.tar");
    let exported = run_with_state(
        &state,
        &[
            "image",
            "export",
            "--image",
            "base",
            "--output",
            path(&catalog_archive),
        ],
    );
    assert_success(&exported);
    assert_eq!(json_output(&exported)["manifest"], manifest);
    assert!(catalog_archive.is_file());
    let second_state = temporary.path().join("second-state");
    assert_success(&run_with_state(
        &second_state,
        &[
            "image",
            "import",
            path(&catalog_archive),
            "--name",
            "roundtrip",
        ],
    ));
    assert_eq!(
        json_output(&run_with_state(
            &second_state,
            &["image", "get", "roundtrip"]
        ))["manifest"],
        manifest
    );

    let run_id = "550e8400-e29b-41d4-a716-446655440001";
    let connection = rusqlite::Connection::open(state.join("runlab.sqlite3")).expect("database");
    connection
        .execute(
            "INSERT INTO runs(
                run_id, accepted_at, initial_image_name, metadata_json, input_json,
                input_identity_json, terminal_at, completion_json
             ) VALUES (?1, '2026-08-28T00:00:00Z', 'base',
                       '{\"description\":null,\"labels\":{}}', '{}', '{}',
                       '2026-08-28T00:00:01Z', ?2)",
            rusqlite::params![
                run_id,
                serde_json::to_string(&json!({
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
                }))
                .expect("completion JSON")
            ],
        )
        .expect("terminal Run");
    let final_archive = temporary.path().join("final.oci.tar");
    assert_success(&run_with_state(
        &state,
        &[
            "image",
            "export",
            "--run",
            run_id,
            "--output",
            path(&final_archive),
        ],
    ));
    assert!(final_archive.is_file());
    let overwrite = run_with_state(
        &state,
        &[
            "image",
            "export",
            "--image",
            "base",
            "--output",
            path(&catalog_archive),
        ],
    );
    assert!(!overwrite.status.success());
    let error: Value = serde_json::from_slice(&overwrite.stderr).expect("structured error");
    assert_eq!(error["kind"], "runlab.error");
}

#[test]
fn storage_prune_removes_only_rebuildable_and_unreferenced_content() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout_with_layers(temporary.path(), &[layer_bytes()]);
    assert_success(&run_with_state(
        &state,
        &["image", "import", path(&layout), "--name", "base"],
    ));

    let unreferenced = state
        .join("oci/blobs/sha256")
        .join("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    fs::write(&unreferenced, b"unreferenced").expect("unreferenced blob");
    let snapshot = state.join("engine/snapshots-v3/chains/stale/upper");
    fs::create_dir_all(&snapshot).expect("snapshot cache");
    fs::write(snapshot.join("cache"), b"cache").expect("snapshot content");
    let invocation = state.join("engine/invocations/stale");
    fs::create_dir_all(&invocation).expect("invocation staging");
    fs::write(invocation.join("work"), b"work").expect("invocation content");

    let status = run_with_state(&state, &["storage", "status"]);
    assert_success(&status);
    let status = json_output(&status);
    assert_eq!(status["assets"]["catalog_images"], 1);
    assert_eq!(status["assets"]["referenced_oci_blobs"], 3);
    assert_eq!(status["reclaimable"]["unreferenced_oci_blobs"], 1);
    assert!(status["reclaimable"]["snapshot_cache_bytes"].as_u64() > Some(0));
    assert!(status["reclaimable"]["invocation_staging_bytes"].as_u64() > Some(0));

    let check = run_with_state(&state, &["storage", "prune", "check"]);
    assert_success(&check);
    assert_eq!(json_output(&check)["mode"], "check");
    assert!(unreferenced.exists());
    assert!(snapshot.exists());
    assert!(invocation.exists());

    let apply = run_with_state(&state, &["storage", "prune", "apply"]);
    assert_success(&apply);
    let apply = json_output(&apply);
    assert_eq!(apply["mode"], "apply");
    assert_eq!(apply["exclusive"], true);
    assert_eq!(apply["remaining_reclaimable"]["unreferenced_oci_blobs"], 0);
    assert!(!unreferenced.exists());
    assert!(!snapshot.exists());
    assert!(!invocation.exists());
    assert_success(&run_with_state(&state, &["image", "get", "base"]));
}

#[test]
fn storage_prune_apply_refuses_concurrent_state_use() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    assert_success(&run_with_state(&state, &["image", "list"]));
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("maintenance.lock"))
        .expect("State maintenance lock");
    rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockShared).expect("shared State lease");

    let apply = run_with_state(&state, &["storage", "prune", "apply"]);
    assert!(!apply.status.success());
    assert!(apply.stdout.is_empty());
    let error: Value = serde_json::from_slice(&apply.stderr).expect("structured error");
    assert_eq!(error["kind"], "runlab.error");
    assert_eq!(error["category"], "conflict");
    assert_eq!(error["stage"], "storage_prune");
    assert_eq!(error["retryable"], true);
}

#[test]
fn run_cancel_is_persisted_idempotently_and_preserves_terminal_runs() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    assert_success(&run_with_state(&state, &["image", "list"]));
    let run_id = "550e8400-e29b-41d4-a716-446655440000";
    let connection = rusqlite::Connection::open(state.join("runlab.sqlite3")).expect("database");
    connection
        .execute(
            "INSERT INTO runs(
                run_id, accepted_at, initial_image_name, metadata_json, input_json,
                input_identity_json
             ) VALUES (?1, '2026-08-28T00:00:00Z', NULL,
                       '{\"description\":null,\"labels\":{}}', '{}', '{}')",
            [run_id],
        )
        .expect("insert accepted Run");

    let first = run_with_state(&state, &["run", "cancel", run_id]);
    assert_success(&first);
    let first = json_output(&first);
    assert_eq!(first["run_id"], run_id);
    assert_eq!(first["lifecycle"], "accepted");
    assert_eq!(first["cancellation_requested"], true);
    let requested_at = first["cancellation_requested_at"]
        .as_str()
        .expect("request time")
        .to_owned();

    let repeated = run_with_state(&state, &["run", "cancel", run_id]);
    assert_success(&repeated);
    assert_eq!(
        json_output(&repeated)["cancellation_requested_at"],
        requested_at
    );
    let record = run_with_state(&state, &["run", "get", run_id]);
    assert_success(&record);
    assert_eq!(
        json_output(&record)["cancellation_requested_at"],
        requested_at
    );

    connection
        .execute(
            "UPDATE runs SET terminal_at = '2026-08-28T00:00:01Z', completion_json = '{}'
             WHERE run_id = ?1",
            [run_id],
        )
        .expect("complete Run");
    let terminal = run_with_state(&state, &["run", "cancel", run_id]);
    assert_success(&terminal);
    let terminal = json_output(&terminal);
    assert_eq!(terminal["lifecycle"], "terminal");
    assert_eq!(terminal["cancellation_requested_at"], requested_at);
    assert_eq!(terminal["terminal_at"], "2026-08-28T00:00:01Z");
}

#[test]
fn exec_help_exposes_execution_without_persistent_run_arguments() {
    let exec_help = run(&["exec", "--help"]);
    assert_success(&exec_help);
    let stdout = text(&exec_help.stdout);
    for argument in [
        "--image",
        "--runtime-config",
        "--stdin",
        "--secret-env",
        "--secret-file",
        "--execution-timeout-ms",
        "--network",
    ] {
        assert!(
            stdout.contains(argument),
            "missing exec argument: {argument}"
        );
    }
    for persistent in ["--id", "--description", "--label"] {
        assert!(
            !stdout.contains(persistent),
            "persistent Run argument leaked into exec: {persistent}"
        );
    }
    assert!(stdout.contains("run_id:null"));
    assert!(stdout.contains("not a dry run"));
    assert!(stdout.contains("complete bounded RunOutput or EngineError JSON"));
}

#[test]
fn schema_and_query_help_expose_the_bounded_public_surface() {
    let schema = run(&["schema", "--help"]);
    assert_success(&schema);
    let stdout = text(&schema.stdout);
    assert!(stdout.contains("list"));
    assert!(stdout.contains("get"));

    let query = run(&["query", "run", "--help"]);
    assert_success(&query);
    let stdout = text(&query.stdout);
    for argument in [
        "--file",
        "--stdin",
        "--limit",
        "--max-cell-bytes",
        "--max-output-bytes",
        "--timeout-seconds",
    ] {
        assert!(
            stdout.contains(argument),
            "missing query argument: {argument}"
        );
    }
    assert!(stdout.contains("Only the public Relations"));
    assert!(stdout.contains("schema list/get"));
}

#[test]
fn managed_vm_surface_is_narrow() {
    #[cfg(target_os = "macos")]
    {
        let vm_help = run(&["vm", "--help"]);
        assert_success(&vm_help);
        let stdout = text(&vm_help.stdout);
        assert!(!stdout.contains("--state"));
        for command in ["create", "start", "install", "stop", "status"] {
            assert!(stdout.contains(&format!("\n  {command} ")));
        }
        for deferred in ["delete", "exec", "shell"] {
            assert!(!stdout.contains(&format!("\n  {deferred} ")));
        }

        let install_help = run(&["vm", "install", "--help"]);
        assert_success(&install_help);
        let stdout = text(&install_help.stdout);
        assert!(!stdout.contains("--binary"));
        assert!(!stdout.contains("--runc"));

        let state = tempfile::tempdir().expect("temporary state");
        let invalid = run_with_state(state.path(), &["vm", "status"]);
        assert!(!invalid.status.success());
        assert!(invalid.stdout.is_empty());
        assert!(text(&invalid.stderr).contains("--state does not apply"));
    }

    #[cfg(target_os = "linux")]
    {
        let help = run(&["--help"]);
        assert_success(&help);
        assert!(!text(&help.stdout).contains("__managed-vm-handshake"));
        let handshake = run(&["__managed-vm-handshake"]);
        assert_success(&handshake);
        let value: Value = serde_json::from_slice(&handshake.stdout).expect("handshake JSON");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["transport_version"], 1);
        assert_eq!(value["runlab_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["os"], "linux");
        assert_eq!(value["architecture"], std::env::consts::ARCH);
        assert_eq!(value["capabilities"], json!(["native-engine", "state-cli"]));
    }
}

#[test]
fn image_and_filesystem_commands_import_discover_and_get_paths() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout(temporary.path());
    let (manifest_descriptor, manifest) = import_described_image(&state, &layout);

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
    assert_run_metadata(&state, run_id);
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

fn import_described_image(state: &Path, layout: &Path) -> (Value, String) {
    let imported = run_with_state(
        state,
        &[
            "image",
            "import",
            path(layout),
            "--name",
            "swebench/example:v1",
            "--description",
            "Python + uv task environment",
            "--label",
            "runtime=python",
            "--label",
            "command=a=b",
        ],
    );
    assert_success(&imported);
    let imported = json_output(&imported);
    assert_eq!(imported["name"], "swebench/example:v1");
    assert_eq!(
        imported["metadata"],
        json!({
            "description": "Python + uv task environment",
            "labels": {"command": "a=b", "runtime": "python"},
        })
    );
    let manifest_descriptor = imported["manifest"].clone();
    let manifest = manifest_descriptor["digest"]
        .as_str()
        .expect("manifest digest")
        .to_owned();

    let listed = run_with_state(state, &["image", "list"]);
    assert_success(&listed);
    let listed = json_output(&listed);
    assert_eq!(listed["images"][0]["name"], "swebench/example:v1");
    assert_eq!(listed["images"][0]["metadata"], imported["metadata"]);
    assert!(listed["next_after"].is_null());

    let by_name = run_with_state(state, &["image", "get", "swebench/example:v1"]);
    assert_success(&by_name);
    let by_name = json_output(&by_name);
    assert_eq!(by_name["manifest"]["digest"], manifest);
    assert_eq!(by_name["metadata"], imported["metadata"]);

    let by_digest = run_with_state(state, &["image", "get", &manifest]);
    assert_success(&by_digest);
    let by_digest = json_output(&by_digest);
    assert_eq!(by_digest["platform"]["os"], "linux");
    assert!(by_digest["metadata"].is_null());
    (manifest_descriptor, manifest)
}

fn assert_run_metadata(state: &Path, run_id: &str) {
    let run_record = run_with_state(state, &["run", "get", run_id]);
    assert_success(&run_record);
    assert_eq!(
        json_output(&run_record)["metadata"],
        json!({
            "description": "SWE-bench replay",
            "labels": {"suite": "swe-bench", "task": "example"},
        })
    );
    let run_list = run_with_state(state, &["run", "list"]);
    assert_success(&run_list);
    assert_eq!(
        json_output(&run_list)["runs"][0]["metadata"]["labels"]["task"],
        "example"
    );
    let list = json_output(&run_list);
    assert_eq!(list["runs"][0]["initial_image_name"], "swebench/example:v1");
    assert_eq!(list["runs"][0]["accepted_at"], "2026-08-27T00:00:00Z");
}

#[test]
fn schema_and_query_expose_only_bounded_public_run_facts() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let layout = create_layout(temporary.path());
    let (manifest, _) = import_described_image(&state, &layout);
    insert_terminal_run(&state, "550e8400-e29b-41d4-a716-446655440000", &manifest);

    let listed = run_with_state(&state, &["schema", "list"]);
    assert_success(&listed);
    assert_eq!(json_output(&listed)["objects"][0]["name"], "runs");
    assert_eq!(json_output(&listed)["objects"][0]["columns"], json!([]));

    let schema = run_with_state(&state, &["schema", "get", "runs", "--compact"]);
    assert_success(&schema);
    assert!(
        json_output(&schema)["objects"][0]["columns"]
            .as_array()
            .is_some_and(|columns| columns.iter().any(|column| column["name"] == "labels"))
    );

    let query = run_with_state(
        &state,
        &[
            "query",
            "run",
            "SELECT run_id, initial_image_name, json_extract(labels, '$.task') AS task, primary_exit_code FROM runs",
        ],
    );
    assert_success(&query);
    let query = json_output(&query);
    assert_eq!(query["complete"], true);
    assert_eq!(query["returned"], 1);
    assert_eq!(
        query["rows"][0]["initial_image_name"],
        "swebench/example:v1"
    );
    assert_eq!(query["rows"][0]["task"], "example");

    let performance = run_with_state(
        &state,
        &[
            "query",
            "run",
            "SELECT primary_started_at, primary_ended_at, primary_duration_ms, accepted_to_primary_start_ms, primary_end_to_terminal_ms, primary_stdout_bytes, primary_stderr_bytes, primary_final_image_digest FROM runs",
        ],
    );
    assert_success(&performance);
    let row = json_output(&performance)["rows"][0].clone();
    assert_eq!(row["primary_started_at"], "2026-08-27T00:00:00.373456789Z");
    assert_eq!(row["primary_ended_at"], "2026-08-27T00:00:00.873456789Z");
    assert_eq!(row["primary_duration_ms"], 500.0);
    assert_eq!(row["accepted_to_primary_start_ms"], 250.0);
    assert_eq!(row["primary_end_to_terminal_ms"], 250.0);
    assert_eq!(row["primary_stdout_bytes"], 5);
    assert_eq!(row["primary_stderr_bytes"], 0);
    assert_eq!(row["primary_final_image_digest"], manifest["digest"]);

    for sql in [
        "SELECT * FROM main.runs",
        "SELECT * FROM sqlite_schema",
        "DELETE FROM runs",
    ] {
        let rejected = run_with_state(&state, &["query", "run", sql]);
        assert!(!rejected.status.success(), "SQL was allowed: {sql}");
        assert!(rejected.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one filesystem contract fixture keeps ordered Layer semantics visible together"
)]
fn filesystem_directory_get_applies_layers_whiteouts_and_symlinks() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let lower = filesystem_layer(|builder| {
        append_file(builder, "workspace/keep.txt", b"keep");
        append_file(builder, "workspace/remove.txt", b"remove");
        append_file(builder, "workspace/sub/old.txt", b"old");
        append_file(builder, "workspace/versioned.txt", b"lower");
        append_symlink(builder, "workspace/latest", "keep.txt");
        append_hard_link(
            builder,
            "workspace/versioned-link.txt",
            "workspace/versioned.txt",
        );
    });
    let upper = filesystem_layer(|builder| {
        append_file(builder, "workspace/.wh.remove.txt", b"");
        append_file(builder, "workspace/sub/.wh..wh..opq", b"");
        append_file(builder, "workspace/sub/new.txt", b"new");
        append_file(builder, "workspace/added.txt", b"added");
        append_file(builder, "workspace/versioned.txt", b"upper");
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
    assert_eq!(
        fs::read(output.join("versioned-link.txt")).expect("hard link"),
        b"lower"
    );
    assert_eq!(
        fs::read(output.join("versioned.txt")).expect("overwritten file"),
        b"upper"
    );
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

    let hard_link_output = temporary.path().join("versioned-link");
    let hard_link = run_with_state(
        &state,
        &[
            "filesystem",
            "get",
            "--image",
            "layered",
            "/workspace/versioned-link.txt",
            "--output",
            path(&hard_link_output),
        ],
    );
    assert_success(&hard_link);
    assert_eq!(json_output(&hard_link)["kind"], "file");
    assert_eq!(
        fs::read(hard_link_output).expect("direct hard link"),
        b"lower"
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

#[test]
fn filesystem_changes_reports_sorted_paginated_final_differences() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    let lower = filesystem_layer(|builder| {
        append_file(builder, "workspace/unchanged.txt", b"same");
        append_file(builder, "workspace/modified.txt", b"before");
        append_file(builder, "workspace/deleted.txt", b"deleted");
        append_file(builder, "workspace/sub/old.txt", b"old");
    });
    let upper = filesystem_layer(|builder| {
        append_file(builder, "workspace/modified.txt", b"after");
        append_file(builder, "workspace/added.txt", b"added");
        append_file(builder, "workspace/.wh.deleted.txt", b"");
        append_file(builder, "workspace/sub/.wh..wh..opq", b"");
        append_file(builder, "workspace/sub/new.txt", b"new");
    });
    let initial_layout = create_layout_with_layers(
        &temporary.path().join("initial"),
        std::slice::from_ref(&lower),
    );
    let final_layout = create_layout_with_layers(&temporary.path().join("final"), &[lower, upper]);
    let initial_import = run_with_state(
        &state,
        &[
            "image",
            "import",
            path(&initial_layout),
            "--name",
            "initial",
        ],
    );
    assert_success(&initial_import);
    let initial_manifest = json_output(&initial_import)["manifest"].clone();
    let final_import = run_with_state(
        &state,
        &["image", "import", path(&final_layout), "--name", "final"],
    );
    assert_success(&final_import);
    let final_manifest = json_output(&final_import)["manifest"].clone();
    let run_id = "d11ce004-0000-4000-8000-000000000004";
    insert_terminal_run_with_final(&state, run_id, &initial_manifest, &final_manifest);

    let first = run_with_state(
        &state,
        &["filesystem", "changes", "--run", run_id, "--limit", "2"],
    );
    assert_success(&first);
    let first = json_output(&first);
    assert_eq!(first["changes"][0]["path"], "/workspace/added.txt");
    assert_eq!(first["changes"][0]["change"], "added");
    assert_eq!(first["changes"][0]["node_type"], "file");
    assert_eq!(first["changes"][0]["size"], 5);
    assert_eq!(first["changes"][1]["path"], "/workspace/deleted.txt");
    assert_eq!(first["changes"][1]["change"], "deleted");
    assert_eq!(first["next_after"], "/workspace/deleted.txt");

    let second = run_with_state(
        &state,
        &[
            "filesystem",
            "changes",
            "--run",
            run_id,
            "--limit",
            "10",
            "--after",
            first["next_after"].as_str().expect("next path"),
        ],
    );
    assert_success(&second);
    let second = json_output(&second);
    assert_eq!(
        second["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .map(|change| change["path"].as_str().expect("path"))
            .collect::<Vec<_>>(),
        [
            "/workspace/modified.txt",
            "/workspace/sub",
            "/workspace/sub/new.txt"
        ]
    );
    assert_eq!(second["changes"][0]["change"], "modified");
    assert_eq!(second["changes"][1]["change"], "modified");
    assert_eq!(second["changes"][1]["node_type"], "directory");
    assert_eq!(second["changes"][1]["subtree"], true);
    assert_eq!(second["changes"][2]["change"], "added");
    assert!(second["next_after"].is_null());
}

fn insert_terminal_run(state: &Path, run_id: &str, manifest: &Value) {
    insert_terminal_run_with_final(state, run_id, manifest, manifest);
}

fn insert_terminal_run_with_final(
    state: &Path,
    run_id: &str,
    initial_manifest: &Value,
    final_manifest: &Value,
) {
    let connection =
        rusqlite::Connection::open(state.join("runlab.sqlite3")).expect("Run database");
    let completion = json!({
        "kind": "engine_returned",
        "result": {
            "kind": "output",
            "output": {
                "execution": {
                    "timed_out": false,
                    "cancelled": false
                },
                "programs": {
                    "primary": {
                        "start": {
                            "status": "succeeded",
                            "facts": {"started_at": "2026-08-27T00:00:00.373456789Z"}
                        },
                        "process": {
                            "kind": "exited",
                            "code": 0,
                            "ended_at": "2026-08-27T00:00:00.873456789Z"
                        },
                        "stdout": {
                            "status": "succeeded",
                            "facts": {"bytes": {"encoding": "base64", "value": "aGVsbG8="}}
                        },
                        "stderr": {
                            "status": "succeeded",
                            "facts": {"bytes": {"encoding": "base64", "value": ""}}
                        },
                        "final_environment": {
                            "availability": "available",
                            "value": final_manifest,
                        }
                    }
                }
            }
        }
    });
    connection
        .execute(
            "INSERT INTO runs(
                run_id, accepted_at, initial_image_name, metadata_json, input_json,
                input_identity_json, terminal_at, completion_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                run_id,
                "2026-08-27T00:00:00.123456789Z",
                "swebench/example:v1",
                serde_json::to_string(&json!({
                    "description": "SWE-bench replay",
                    "labels": {"suite": "swe-bench", "task": "example"},
                }))
                .expect("metadata JSON"),
                serde_json::to_string(&json!({
                    "programs": {"primary": {"initial_environment": initial_manifest}}
                }))
                .expect("input JSON"),
                "{}",
                "2026-08-27T00:00:01.123456789Z",
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
fn existing_state_is_migrated_with_empty_metadata() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let state = temporary.path().join("state");
    fs::create_dir_all(&state).expect("state directory");
    let connection = rusqlite::Connection::open(state.join("runlab.sqlite3")).expect("database");
    connection
        .execute_batch(
            "CREATE TABLE catalog (
                 name TEXT PRIMARY KEY,
                 descriptor_json TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE runs (
                 run_id TEXT PRIMARY KEY,
                 accepted_at TEXT NOT NULL,
                 input_json TEXT NOT NULL,
                 input_identity_json TEXT NOT NULL,
                 terminal_at TEXT,
                 completion_json TEXT
             ) STRICT;
             INSERT INTO catalog VALUES ('legacy', '{}', '2026-08-27T00:00:00Z');
             INSERT INTO runs VALUES (
                 '550e8400-e29b-41d4-a716-446655440000',
                 '2026-08-27T00:00:00Z', '{}', '{}', NULL, NULL
             );",
        )
        .expect("legacy schema");
    drop(connection);

    let images = run_with_state(&state, &["image", "list"]);
    assert_success(&images);
    assert_eq!(
        json_output(&images)["images"][0]["metadata"],
        json!({"description": null, "labels": {}})
    );

    let runs = run_with_state(&state, &["run", "list"]);
    assert_success(&runs);
    assert_eq!(
        json_output(&runs)["runs"][0]["metadata"],
        json!({"description": null, "labels": {}})
    );
    let cancelled = run_with_state(
        &state,
        &["run", "cancel", "550e8400-e29b-41d4-a716-446655440000"],
    );
    assert_success(&cancelled);
    assert_eq!(json_output(&cancelled)["cancellation_requested"], true);
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
    let missing_error: Value = serde_json::from_slice(&missing.stderr).expect("structured error");
    assert_eq!(missing_error["kind"], "runlab.error");
    assert_eq!(missing_error["category"], "not_found");
    assert_eq!(missing_error["stage"], "run_lookup");
    assert_eq!(missing_error["accepted"], false);
    assert_eq!(missing_error["run_created"], false);
    assert_eq!(missing_error["retryable"], false);
    assert!(
        missing_error["message"]
            .as_str()
            .is_some_and(|value| value.contains("Run does not exist"))
    );

    let bad_id = run_with_state(&state, &["run", "get", "not-a-run-id"]);
    assert!(!bad_id.status.success());
    assert!(bad_id.stdout.is_empty());
    let bad_id_error: Value = serde_json::from_slice(&bad_id.stderr).expect("argument error");
    assert_eq!(bad_id_error["category"], "invalid_input");
    assert_eq!(bad_id_error["stage"], "arguments");
    assert!(
        bad_id_error["message"]
            .as_str()
            .is_some_and(|value| value.contains("UUID v4"))
    );

    let malformed_label = run_with_state(
        &state,
        &[
            "image",
            "import",
            "missing.oci",
            "--name",
            "example",
            "--label",
            "missing-equals",
        ],
    );
    assert!(!malformed_label.status.success());
    assert!(malformed_label.stdout.is_empty());
    let label_error: Value =
        serde_json::from_slice(&malformed_label.stderr).expect("argument error");
    assert_eq!(label_error["category"], "invalid_input");
    assert_eq!(label_error["stage"], "arguments");
    assert!(
        label_error["message"]
            .as_str()
            .is_some_and(|value| value.contains("KEY=VALUE"))
    );
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

fn append_hard_link(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, target: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Link);
    header.set_link_name(target).expect("hard link target");
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new([]))
        .expect("append hard link");
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
