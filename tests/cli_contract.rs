use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::{Connection, params};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const RUN_ID: &str = "run-018f0c90-7b8a-7000-8000-000000000001";
const PRIMARY_STDOUT: &[u8] = b"primary stdout\n";
const PRIMARY_STDERR: &[u8] = b"primary stderr\n";
const SERVICE_STDOUT: &[u8] = b"service stdout\n";
const SERVICE_STDERR: &[u8] = b"service stderr\n";

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(arguments)
        .output()
        .expect("runlab process")
}

fn public_schema(name: &str) -> Value {
    let output = run(&["schema", "show", name]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).expect("public JSON Schema")
}

/// Reads the record version the installed binary declares, so a protocol bump
/// cannot leave this fixture silently describing an older record shape.
fn declared_schema_version(schema_name: &str) -> u64 {
    public_schema(schema_name)["properties"]["schema_version"]["const"]
        .as_u64()
        .expect("public schema declares its record version as a const")
}

fn assert_top_level_result_shape(result: &Value, schema_name: &str) {
    let schema = public_schema(schema_name);
    let actual = result
        .as_object()
        .expect("result object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let declared = schema["properties"]
        .as_object()
        .expect("schema properties")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, declared, "{schema_name} top-level fields");
    for required in schema["required"].as_array().expect("required fields") {
        assert!(
            actual.contains(required.as_str().expect("required field name")),
            "{schema_name} omitted required field {required}"
        );
    }
}

#[test]
fn help_exposes_oci_run_and_explicit_docker_compatibility_surfaces() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("image"));
    assert!(stdout.contains("runtime-config"));
    assert!(stdout.contains("run"));
    assert!(stdout.contains("docker"));
    assert!(stdout.contains("vm"));
    assert!(!stdout.contains("Base + Overlay + Task"));
    assert!(output.stderr.is_empty());

    let runtime_create = run(&["runtime-config", "create", "--help"]);
    assert!(runtime_create.status.success());
    assert!(runtime_create.stderr.is_empty());
    let runtime_create_help =
        String::from_utf8(runtime_create.stdout).expect("UTF-8 runtime-config create help");
    for expected in [
        "--network <NETWORK>",
        "[default: none]",
        "[possible values: none, egress]",
        "inherits a Run-owned namespace",
    ] {
        assert!(
            runtime_create_help.contains(expected),
            "missing {expected} in {runtime_create_help}"
        );
    }

    let invalid_network = run(&[
        "runtime-config",
        "create",
        "image:test",
        "--output",
        "/tmp/config.json",
        "--network",
        "private",
    ]);
    assert_eq!(invalid_network.status.code(), Some(2));
    assert!(invalid_network.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid_network.stderr).contains("invalid value 'private'"));
}

#[test]
fn vm_help_exposes_namespace_and_file_staging_without_accepting_host_state() {
    let help = run(&["vm", "exec", "--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let stdout = String::from_utf8(help.stdout).expect("UTF-8 help");
    for expected in [
        "--namespace <NAMESPACE>",
        "--input <HOST_FILE>",
        "--runtime-config-input <INDEX>",
        "--output <HOST_FILE>",
        "@input/N",
        "@output/N",
        "vm` rejects host state",
    ] {
        assert!(stdout.contains(expected), "missing {expected} in {stdout}");
    }

    let rejected = run(&["--state", "/tmp/host-state", "vm", "status"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("vm commands do not accept host --state")
    );

    for invalid in [
        vec![
            "vm",
            "exec",
            "--namespace",
            "../escape",
            "--",
            "schema",
            "list",
        ],
        vec!["vm", "exec", "--namespace", "safe", "--", "sh", "-c", "id"],
        vec![
            "vm",
            "exec",
            "--namespace",
            "safe",
            "--input",
            "/tmp/a",
            "--",
            "schema",
            "list",
        ],
    ] {
        let output = run(&invalid);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn docker_commands_remain_explicit_beside_oci_native_import() {
    let docker = run(&["docker", "image", "--help"]);
    assert!(docker.status.success());
    assert!(docker.stderr.is_empty());
    let docker_help = String::from_utf8(docker.stdout).expect("UTF-8 help");
    for command in ["import", "materialize", "checkout"] {
        assert!(docker_help.contains(command));
    }

    let image = run(&["image", "--help"]);
    assert!(image.status.success());
    assert!(image.stderr.is_empty());
    let image_help = String::from_utf8(image.stdout).expect("UTF-8 help");
    assert!(image_help.contains("import"));
    for command in ["materialize", "checkout"] {
        assert!(!image_help.contains(command));
        let legacy = run(&["image", command]);
        assert_eq!(legacy.status.code(), Some(2));
        assert!(legacy.stdout.is_empty());
        assert!(String::from_utf8_lossy(&legacy.stderr).contains("unrecognized subcommand"));
    }

    let import = run(&["image", "import", "--help"]);
    assert!(import.status.success());
    assert!(import.stderr.is_empty());
    let import_help = String::from_utf8(import.stdout).expect("UTF-8 help");
    for input in [
        "<SOURCE>",
        "--name <LOCAL_REFERENCE>",
        "--source-reference <SOURCE_REFERENCE>",
        "--manifest <MANIFEST>",
        "--platform <PLATFORM>",
        "--description <DESCRIPTION>",
    ] {
        assert!(
            import_help.contains(input),
            "missing {input} in {import_help}"
        );
    }
}

#[test]
fn image_pull_help_exposes_bounded_platform_and_catalog_inputs() {
    let output = run(&["image", "pull", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("<REMOTE_REFERENCE>"));
    assert!(stdout.contains("--platform <PLATFORM>"));
    assert!(stdout.contains("linux/amd64"));
    assert!(stdout.contains("linux/arm64"));
    assert!(stdout.contains("--name <LOCAL_REFERENCE>"));
    assert!(stdout.contains("--description <DESCRIPTION>"));
    assert!(output.stderr.is_empty());

    let invalid = run(&[
        "image",
        "pull",
        "registry.example/team/agent:latest",
        "--platform",
        "linux/386",
    ]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid value 'linux/386'"));
}

type SchemaExpectation = (&'static str, &'static str, &'static [&'static str]);

const PUBLIC_SCHEMA_EXPECTATIONS: &[SchemaExpectation] = &[
    (
        "accepted-run-record",
        "AcceptedRunRecord",
        &[
            "schema_version",
            "run_id",
            "lifecycle",
            "accepted_at",
            "requested_image_reference",
            "initial_image",
            "runtime_config",
            "controls",
            "managed_service",
        ],
    ),
    (
        "terminal-run-record",
        "TerminalRunRecord",
        &[
            "schema_version",
            "run_id",
            "lifecycle",
            "accepted_at",
            "terminal_at",
            "requested_image_reference",
            "initial_image",
            "runtime_config",
            "controls",
            "backend",
            "process",
            "stdout",
            "stderr",
            "final_image",
            "operation_errors",
            "managed_service",
        ],
    ),
    (
        "run-start-result",
        "RunStartResult",
        &[
            "schema_version",
            "run_id",
            "database",
            "process",
            "initial_image",
            "final_image",
            "stdout",
            "stderr",
            "operation_errors",
            "managed_service",
            "cleanup",
        ],
    ),
    (
        "run-list-result",
        "RunListResult",
        &["schema_version", "runs", "next_after"],
    ),
    (
        "run-diff-result",
        "RunDiffResult",
        &[
            "schema_version",
            "left_run_id",
            "right_run_id",
            "equal",
            "total_differences",
            "truncated",
            "differences",
        ],
    ),
    (
        "run-stream-get-result",
        "RunStreamGetResult",
        &["schema_version", "run_id", "participant", "field", "output"],
    ),
    (
        "run-reconcile-result",
        "RunReconcileResult",
        &[
            "schema_version",
            "run_id",
            "status",
            "terminalized",
            "actions",
            "resources_absent",
            "cleanup_errors",
        ],
    ),
    (
        "run-reconcile-batch-result",
        "RunReconcileBatchResult",
        &["schema_version", "dry_run", "items", "failed", "next_after"],
    ),
    (
        "run-verify-result",
        "RunVerifyResult",
        &[
            "schema_version",
            "run_id",
            "lifecycle",
            "valid",
            "image_roots",
            "verified_stored_bytes",
            "verified_stored_bytes_size",
            "verified_oci_blobs",
            "verified_oci_bytes",
        ],
    ),
    (
        "image-operation-result",
        "ImageOperationResult",
        &[
            "schema_version",
            "manifest",
            "platform",
            "config",
            "layers",
            "parent_manifest",
            "added_layers",
        ],
    ),
    (
        "image-inspect-result",
        "ImageInspectResult",
        &[
            "manifest",
            "config",
            "platform",
            "layers",
            "diff_ids",
            "parent_manifest",
            "added_layers",
        ],
    ),
    (
        "image-import-result",
        "ImageImportResult",
        &[
            "schema_version",
            "source_kind",
            "source_index",
            "selected_manifest",
            "platform",
            "verified_blobs",
            "verified_bytes",
            "local_reference",
        ],
    ),
    (
        "image-pull-result",
        "ImagePullResult",
        &[
            "schema_version",
            "remote_reference",
            "source_index",
            "selected_manifest",
            "platform",
            "downloaded_blobs",
            "downloaded_bytes",
            "local_reference",
        ],
    ),
    (
        "image-catalog-list-result",
        "ImageCatalogListResult",
        &["schema_version", "entries", "next_after"],
    ),
    (
        "image-catalog-show-result",
        "ImageCatalogShowResult",
        &["schema_version", "entry"],
    ),
    (
        "image-catalog-set-result",
        "ImageCatalogSetResult",
        &["schema_version", "changed", "previous", "entry"],
    ),
    (
        "image-catalog-remove-result",
        "ImageCatalogRemoveResult",
        &["schema_version", "reference", "removed", "previous"],
    ),
    (
        "image-diff-result",
        "ImageDiffResult",
        &["schema_version", "from", "to", "structure", "filesystem"],
    ),
    (
        "image-export-result",
        "ImageExportResult",
        &[
            "schema_version",
            "requested_reference",
            "manifest_digest",
            "output",
            "digest",
            "size",
            "format",
        ],
    ),
    (
        "image-file-get-result",
        "ImageFileGetResult",
        &[
            "schema_version",
            "requested_reference",
            "manifest_digest",
            "source",
            "output",
            "digest",
            "size",
        ],
    ),
    (
        "docker-image-materialize-result",
        "DockerImageMaterializeResult",
        &["schema_version", "manifest_digest", "docker_image"],
    ),
    (
        "docker-image-checkout-create-result",
        "DockerImageCheckoutCreateResult",
        &[
            "schema_version",
            "checkout_id",
            "parent_manifest",
            "exec_argv",
        ],
    ),
    (
        "runtime-config-create-result",
        "RuntimeConfigCreateResult",
        &[
            "schema_version",
            "requested_reference",
            "manifest_digest",
            "output",
            "size",
        ],
    ),
    (
        "runtime-config-check-result",
        "RuntimeConfigCheckResult",
        &["schema_version", "valid", "oci_version"],
    ),
    (
        "managed-service-check-result",
        "ManagedServiceCheckResult",
        &[
            "schema_version",
            "valid",
            "name",
            "requested_reference",
            "initial_image",
            "runtime_config",
            "readiness",
        ],
    ),
    (
        "state-verify-result",
        "StateVerifyResult",
        &[
            "schema_version",
            "valid",
            "catalog_entries",
            "runs",
            "accepted_runs",
            "image_roots",
            "rooted_manifests",
            "verified_stored_bytes",
            "verified_stored_bytes_size",
            "reachable_oci_blobs",
            "reachable_oci_bytes",
            "orphan_oci_blobs",
            "orphan_oci_bytes",
            "staging_entries",
            "recovery_entries",
        ],
    ),
    (
        "state-gc-plan",
        "StateGcPlan",
        &[
            "schema_version",
            "created_at",
            "roots_digest",
            "roots",
            "reachable_oci_blobs",
            "reachable_oci_bytes",
            "delete",
            "plan_digest",
        ],
    ),
    (
        "state-gc-plan-result",
        "StateGcPlanResult",
        &[
            "schema_version",
            "output",
            "plan_digest",
            "roots",
            "reachable_oci_blobs",
            "reachable_oci_bytes",
            "delete_oci_blobs",
            "delete_oci_bytes",
        ],
    ),
    (
        "state-gc-apply-result",
        "StateGcApplyResult",
        &[
            "schema_version",
            "plan_digest",
            "deleted_oci_blobs",
            "deleted_oci_bytes",
            "already_absent_oci_blobs",
            "already_absent_oci_bytes",
            "skipped_reachable_oci_blobs",
            "skipped_reachable_oci_bytes",
            "failed",
            "failures",
        ],
    ),
    (
        "vm-status",
        "VmStatus",
        &[
            "schema_version",
            "instance",
            "status",
            "lima_version",
            "vm_type",
            "architecture",
            "plain",
            "mounts",
            "image",
            "handshake",
            "handshake_error",
            "runc",
            "runc_error",
            "reference_profile",
            "reference_profile_error",
        ],
    ),
    (
        "vm-install-result",
        "VmInstallResult",
        &[
            "schema_version",
            "instance",
            "binary",
            "digest",
            "size",
            "handshake",
            "runc_binary",
            "runc_digest",
            "runc_size",
            "runc",
            "reference_profile",
        ],
    ),
    (
        "vm-operation-result",
        "VmOperationResult",
        &[
            "schema_version",
            "instance",
            "operation_id",
            "namespace",
            "detached",
            "runtime_config_inputs",
        ],
    ),
    (
        "vm-operation-status",
        "VmOperationStatus",
        &[
            "schema_version",
            "operation_id",
            "namespace",
            "state",
            "terminal",
            "exit_code",
            "result",
            "output_count",
            "runtime_config_inputs",
        ],
    ),
    (
        "vm-cancel-result",
        "VmCancelResult",
        &["schema_version", "operation_id", "signal_sent", "status"],
    ),
    (
        "vm-discard-result",
        "VmDiscardResult",
        &["schema_version", "operation_id", "removed"],
    ),
    (
        "schema-list-result",
        "SchemaListResult",
        &["schema_version", "schemas"],
    ),
];

#[test]
fn schema_catalog_is_complete_compact_and_state_independent() {
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(["schema", "list"])
        .env_remove("RUNLAB_STATE")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .output()
        .expect("runlab process");
    assert!(output.status.success());
    assert_eq!(output.stdout.split(|byte| *byte == b'\n').count(), 2);
    assert!(output.stderr.is_empty());
    let list: Value = serde_json::from_slice(&output.stdout).expect("schema list result");
    let expectations = PUBLIC_SCHEMA_EXPECTATIONS;
    let names = list["schemas"]
        .as_array()
        .expect("schema names")
        .iter()
        .map(|name| name.as_str().expect("schema name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        expectations
            .iter()
            .map(|(name, _, _)| *name)
            .collect::<Vec<_>>()
    );
    assert_top_level_result_shape(&list, "schema-list-result");
    for (name, title, fields) in expectations {
        let schema = public_schema(name);
        assert_eq!(schema["title"], *title);
        let declared = schema["properties"]
            .as_object()
            .expect("schema properties")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(declared, fields.iter().copied().collect(), "{name}");
    }
}

#[test]
fn runtime_check_is_pure_and_rejects_duplicate_keys() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let valid = directory.path().join("valid.json");
    let state = directory.path().join("state");
    fs::write(
        &valid,
        br#"{"ociVersion":"1.2.0","root":{"path":"rootfs"},"process":{"terminal":false,"user":{"uid":0,"gid":0},"args":["/bin/true"],"env":[],"cwd":"/","noNewPrivileges":true},"hostname":"runlab","linux":{"namespaces":[{"type":"pid"},{"type":"network"},{"type":"ipc"},{"type":"uts"},{"type":"mount"},{"type":"cgroup"}]},"hooks":{}}"#,
    )
    .expect("write config");
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args([
            "--state",
            state.to_str().expect("state path"),
            "runtime-config",
            "check",
            valid.to_str().expect("path"),
        ])
        .env_remove("RUNLAB_STATE")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .output()
        .expect("runlab process");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("check result");
    assert_eq!(
        result,
        serde_json::json!({
            "schema_version": 1,
            "valid": true,
            "oci_version": "1.2.0"
        })
    );
    assert_top_level_result_shape(&result, "runtime-config-check-result");
    assert!(!state.exists());

    let duplicate = directory.path().join("duplicate.json");
    fs::write(
        &duplicate,
        br#"{"ociVersion":"1.2.0","ociVersion":"1.2.0"}"#,
    )
    .expect("write duplicate config");
    let output = run(&["runtime-config", "check", duplicate.to_str().expect("path")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate JSON key"));
}

#[test]
fn invalid_runtime_fails_before_state_creation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = directory.path().join("config.json");
    let state = directory.path().join("state");
    fs::write(&config, br#"{"ociVersion":"1.2.0"}"#).expect("write config");
    let output = run(&[
        "--state",
        state.to_str().expect("path"),
        "run",
        "start",
        &format!("sha256:{}", "1".repeat(64)),
        "--runtime-config",
        config.to_str().expect("path"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("OCI Runtime root must be an object"));
    assert!(!state.exists());
}

#[test]
fn oversized_stream_limit_fails_before_state_creation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    let output = run(&[
        "--state",
        state.to_str().expect("path"),
        "run",
        "start",
        &format!("sha256:{}", "1".repeat(64)),
        "--runtime-config",
        "missing-config.json",
        "--stdout-limit-bytes",
        "16777217",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("stream limits must not exceed 16777216 bytes")
    );
    assert!(!state.exists());
}

#[test]
fn state_verify_does_not_initialize_missing_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("missing-state");
    let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args([
            "--state",
            state.to_str().expect("UTF-8 state"),
            "state",
            "verify",
        ])
        .output()
        .expect("state verify");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to inspect RunLab state"));
    assert!(!state.exists());
}

#[test]
fn ordinary_read_commands_do_not_initialize_missing_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let digest = format!("sha256:{}", "0".repeat(64));
    for (name, arguments) in [
        ("image inspect", vec!["image", "inspect", digest.as_str()]),
        ("run get", vec!["run", "get", RUN_ID]),
        ("run list", vec!["run", "list"]),
    ] {
        let state = directory.path().join(name.replace(' ', "-"));
        let output = Command::new(env!("CARGO_BIN_EXE_runlab"))
            .arg("--state")
            .arg(&state)
            .args(arguments)
            .output()
            .expect("runlab process");
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert!(output.stdout.is_empty(), "{name}");
        assert!(!state.exists(), "{name} created state");
    }
}

#[test]
fn run_verify_does_not_initialize_missing_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("missing-state");
    let output = run(&[
        "--state",
        state.to_str().expect("UTF-8 state"),
        "run",
        "verify",
        "run-018f0c90-7b8a-7000-8000-000000000001",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to inspect RunLab state"));
    assert!(!state.exists());
}

#[test]
fn reconcile_dry_run_does_not_initialize_missing_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("missing-state");
    let output = run(&[
        "--state",
        state.to_str().expect("UTF-8 state"),
        "run",
        "reconcile",
        "run-018f0c90-7b8a-7000-8000-000000000001",
        "--dry-run",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!state.exists());
}

#[test]
fn clap_usage_errors_exit_two_without_json() {
    let output = run(&["run", "start"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));
}

#[test]
fn reconcile_requires_one_identity_or_bounded_state_wide_mode() {
    let unknown = "run-018f0f4d-0000-7000-8000-000000000000";
    for arguments in [
        vec!["run", "reconcile"],
        vec!["run", "reconcile", unknown, "--all"],
        vec!["run", "reconcile", unknown, "--limit", "2"],
        vec!["run", "reconcile", "--after", unknown],
    ] {
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(2), "arguments: {arguments:?}");
        assert!(output.stdout.is_empty());
    }

    let help = run(&["run", "reconcile", "--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("--all"));
    assert!(stdout.contains("--limit <COUNT>"));
    assert!(stdout.contains("--after <AFTER>"));
    assert!(stdout.contains("--dry-run"));
}

#[test]
fn managed_service_is_explicit_and_rejected_by_docker_before_state_creation() {
    let help = run(&["run", "start", "--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--managed-service <FILE>"));

    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    let output = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "start",
        &format!("sha256:{}", "1".repeat(64)),
        "--backend",
        "docker",
        "--runtime-config",
        "does-not-exist.json",
        "--managed-service",
        "does-not-exist-service.json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--managed-service requires --backend native")
    );
    assert!(!state.exists());
}

#[test]
fn run_stream_get_selects_default_primary_and_explicit_participants() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    seed_terminal_run(&state, true);

    for (stream, participant, expected, output_name) in [
        ("stdout", None, PRIMARY_STDOUT, "default-primary.stdout"),
        (
            "stderr",
            Some("primary"),
            PRIMARY_STDERR,
            "explicit-primary.stderr",
        ),
        (
            "stdout",
            Some("managed-service"),
            SERVICE_STDOUT,
            "service.stdout",
        ),
        (
            "stderr",
            Some("managed-service"),
            SERVICE_STDERR,
            "service.stderr",
        ),
    ] {
        let output_path = directory.path().join(output_name);
        let mut arguments = vec![
            "--state",
            state.to_str().expect("state path"),
            "run",
            stream,
            "get",
            RUN_ID,
        ];
        if let Some(participant) = participant {
            arguments.extend(["--participant", participant]);
        }
        arguments.extend(["--output", output_path.to_str().expect("output path")]);

        let output = run(&arguments);

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(fs::read(&output_path).expect("captured stream"), expected);
        let result: Value = serde_json::from_slice(&output.stdout).expect("stream result");
        assert_top_level_result_shape(&result, "run-stream-get-result");
        assert_eq!(result["schema_version"], 2);
        assert_eq!(result["run_id"], RUN_ID);
        assert_eq!(result["field"], stream);
        assert_eq!(
            result["participant"],
            participant.unwrap_or("primary").replace('-', "_")
        );
        assert_eq!(
            Path::new(result["output"].as_str().expect("output path"))
                .canonicalize()
                .expect("result output path"),
            output_path.canonicalize().expect("fixture output path")
        );
    }
}

#[test]
fn run_stream_get_preserves_missing_managed_service_error() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    seed_terminal_run(&state, false);
    let output_path = directory.path().join("service.stdout");

    let output = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "stdout",
        "get",
        RUN_ID,
        "--participant",
        "managed-service",
        "--output",
        output_path.to_str().expect("output path"),
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Run field is unavailable"));
    assert!(stderr.contains("service_stdout"));
    assert!(!output_path.exists());
}

#[test]
fn run_stream_get_rejects_unknown_participant() {
    let output = run(&[
        "run",
        "stdout",
        "get",
        RUN_ID,
        "--participant",
        "sidecar",
        "--output",
        "unused",
    ]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid value 'sidecar'"));
    assert!(stderr.contains("primary"));
    assert!(stderr.contains("managed-service"));
}

#[test]
fn run_stream_get_help_documents_participant_selection() {
    for stream in ["stdout", "stderr"] {
        let output = run(&["run", stream, "get", "--help"]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert!(stdout.contains("--participant <PARTICIPANT>"));
        assert!(stdout.contains("Participant whose captured stream is read"));
        assert!(stdout.contains("primary"));
        assert!(stdout.contains("managed-service"));
        assert!(stdout.contains("default: primary"));
    }
}

#[test]
fn run_list_is_bounded_filterable_and_cursor_paginated() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    let connection = initialize_run_database(&state);
    let older = RUN_ID;
    let newer = "run-018f0c90-7b8a-7000-8000-000000000002";
    insert_terminal_record(&connection, older, &terminal_record(false));
    let mut newer_record = terminal_record(false);
    newer_record["run_id"] = Value::String(newer.to_owned());
    insert_terminal_record(&connection, newer, &newer_record);
    drop(connection);

    let first = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "list",
        "--lifecycle",
        "terminal",
        "--limit",
        "1",
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());
    let first: Value = serde_json::from_slice(&first.stdout).expect("Run list JSON");
    assert_top_level_result_shape(&first, "run-list-result");
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["runs"].as_array().expect("runs").len(), 1);
    assert_eq!(first["runs"][0]["run_id"], newer);
    assert_eq!(first["next_after"], newer);
    assert!(!first.to_string().contains("primary stdout"));

    let second = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "list",
        "--limit",
        "1",
        "--after",
        newer,
    ]);
    assert!(second.status.success());
    let second: Value = serde_json::from_slice(&second.stdout).expect("Run list JSON");
    assert_eq!(second["runs"][0]["run_id"], older);
    assert!(second["next_after"].is_null());

    let invalid = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "list",
        "--limit",
        "0",
    ]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("--limit must be between 1 and 100"));
}

#[test]
fn run_diff_reports_only_record_facts_without_stream_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let state = directory.path().join("state");
    let connection = initialize_run_database(&state);
    let left = RUN_ID;
    let right = "run-018f0c90-7b8a-7000-8000-000000000002";
    insert_terminal_record(&connection, left, &terminal_record(false));
    let mut right_record = terminal_record(false);
    right_record["run_id"] = Value::String(right.to_owned());
    right_record["accepted_at"] = Value::String("2026-08-22T00:00:00Z".to_owned());
    right_record["terminal_at"] = Value::String("2026-08-22T00:00:02Z".to_owned());
    right_record["requested_image_reference"] = Value::String("runlab/agent:latest".to_owned());
    right_record["controls"]["timeout_seconds"] = Value::from(90);
    right_record["stdout"] = stored_bytes(b"different stdout bytes");
    insert_terminal_record(&connection, right, &right_record);
    drop(connection);

    let output = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "diff",
        left,
        right,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 JSON");
    assert!(!stdout.contains("primary stdout"));
    assert!(!stdout.contains("different stdout bytes"));
    let result: Value = serde_json::from_str(&stdout).expect("Run diff JSON");
    assert_top_level_result_shape(&result, "run-diff-result");
    assert_eq!(result["left_run_id"], left);
    assert_eq!(result["right_run_id"], right);
    assert_eq!(result["equal"], false);
    let paths = result["differences"]
        .as_array()
        .expect("differences")
        .iter()
        .map(|difference| difference["path"].as_str().expect("path"))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/controls/timeout_seconds",
            "/requested_image_reference",
            "/stdout/digest",
            "/stdout/size"
        ]
    );
    assert!(
        !paths
            .iter()
            .any(|path| path.contains("run_id") || path.contains("_at"))
    );

    let equal = run(&[
        "--state",
        state.to_str().expect("state path"),
        "run",
        "diff",
        left,
        left,
    ]);
    assert!(equal.status.success());
    let equal: Value = serde_json::from_slice(&equal.stdout).expect("equal diff JSON");
    assert_eq!(equal["equal"], true);
    assert_eq!(equal["total_differences"], 0);
    assert_eq!(equal["differences"], serde_json::json!([]));
}

fn seed_terminal_run(state: &Path, with_service: bool) {
    let connection = initialize_run_database(state);
    let terminal_bytes =
        serde_json::to_vec(&terminal_record(with_service)).expect("terminal record");
    let runtime_config = b"{}\n";
    let service_runtime_config = b"{\"service\":true}\n";
    let service_initial_digest = with_service.then(|| digest_for_seed('2'));
    let service_config = with_service.then_some(service_runtime_config.as_slice());
    let service_stdout = with_service.then_some(SERVICE_STDOUT);
    let service_stderr = with_service.then_some(SERVICE_STDERR);
    connection
        .execute(
            r"
            INSERT INTO runs (
                run_id, lifecycle, accepted_at, terminal_at,
                initial_manifest_digest, service_initial_manifest_digest,
                accepted_record_json, terminal_record_json,
                runtime_config, service_runtime_config, stdin,
                stdout, stderr, service_stdout, service_stderr
            ) VALUES (
                ?1, 'terminal', ?2, ?3,
                ?4, ?5,
                ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12, ?13, ?14
            )
            ",
            params![
                RUN_ID,
                "2026-08-21T00:00:00Z",
                "2026-08-21T00:00:02Z",
                digest_for_seed('1'),
                service_initial_digest,
                b"{}".as_slice(),
                terminal_bytes,
                runtime_config.as_slice(),
                service_config,
                b"".as_slice(),
                PRIMARY_STDOUT,
                PRIMARY_STDERR,
                service_stdout,
                service_stderr,
            ],
        )
        .expect("terminal Run fixture");
}

fn insert_terminal_record(connection: &Connection, run_id: &str, record: &Value) {
    let terminal_bytes = serde_json::to_vec(record).expect("terminal record");
    connection
        .execute(
            r"
            INSERT INTO runs (
                run_id, lifecycle, accepted_at, terminal_at,
                initial_manifest_digest, accepted_record_json, terminal_record_json,
                runtime_config, stdin, stdout, stderr
            ) VALUES (
                ?1, 'terminal', ?2, ?3,
                ?4, ?5, ?6,
                ?7, ?8, ?9, ?10
            )
            ",
            params![
                run_id,
                record["accepted_at"].as_str().expect("accepted_at"),
                record["terminal_at"].as_str().expect("terminal_at"),
                record["initial_image"]["digest"]
                    .as_str()
                    .expect("Initial Manifest digest"),
                b"{}".as_slice(),
                terminal_bytes,
                b"{}".as_slice(),
                b"".as_slice(),
                PRIMARY_STDOUT,
                PRIMARY_STDERR,
            ],
        )
        .expect("insert terminal Run fixture");
}

fn initialize_run_database(state: &Path) -> Connection {
    fs::create_dir_all(state).expect("state directory");
    let database_path = state.join("runs.sqlite3");
    let connection = Connection::open(&database_path).expect("Run database fixture");
    connection
        .execute_batch(
            r"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO schema_metadata(key, value) VALUES ('storage_version', 6);
            CREATE TABLE runs (
                run_id TEXT PRIMARY KEY,
                lifecycle TEXT NOT NULL,
                accepted_at TEXT NOT NULL,
                terminal_at TEXT,
                initial_manifest_digest TEXT NOT NULL,
                final_manifest_digest TEXT,
                service_initial_manifest_digest TEXT,
                service_final_manifest_digest TEXT,
                accepted_record_json BLOB NOT NULL,
                terminal_record_json BLOB,
                runtime_config BLOB NOT NULL,
                service_runtime_config BLOB,
                stdin BLOB NOT NULL,
                stdout BLOB,
                stderr BLOB,
                service_stdout BLOB,
                service_stderr BLOB
            );
            ",
        )
        .expect("Run database schema");
    connection
}

fn terminal_record(with_service: bool) -> Value {
    let runtime_config = b"{}\n";
    let service_runtime_config = b"{\"service\":true}\n";
    let primary_manifest = descriptor('1');
    let service_manifest = descriptor('2');
    let service = with_service.then(|| {
        serde_json::json!({
            "name": "postgres",
            "requested_image_reference": null,
            "initial_image": service_manifest,
            "runtime_config": stored_bytes(service_runtime_config),
            "readiness_condition": {
                "port": 5432,
                "timeout_seconds": 30
            },
            "readiness": {
                "outcome": "ready",
                "observed_at": "2026-08-21T00:00:01Z",
                "attempts": 1
            },
            "process": process_facts(),
            "stdout": stored_bytes(SERVICE_STDOUT),
            "stderr": stored_bytes(SERVICE_STDERR),
            "final_image": {"availability": "not_applicable"},
            "operation_errors": []
        })
    });
    serde_json::json!({
        "schema_version": declared_schema_version("terminal-run-record"),
        "run_id": RUN_ID,
        "lifecycle": "terminal",
        "accepted_at": "2026-08-21T00:00:00Z",
        "terminal_at": "2026-08-21T00:00:02Z",
        "requested_image_reference": null,
        "initial_image": primary_manifest,
        "runtime_config": stored_bytes(runtime_config),
        "controls": {
            "stdin": stored_bytes(b""),
            "timeout_seconds": 60,
            "stdout_limit_bytes": 1024,
            "stderr_limit_bytes": 1024,
            "network": "none"
        },
        "backend": {
            "name": "native_linux",
            "version": "fixture",
            "platform": {"os": "linux", "architecture": "amd64"},
            "network": "none",
            "run_network": with_service.then(|| serde_json::json!({
                "namespace_device": 4,
                "namespace_inode": 5,
                "realization": {"kind": "loopback_only"}
            })),
            "details": {
                "kind": "native_linux",
                "runtime_name": "runc",
                "runtime_version": "1.3.6",
                "runtime_commit": "fixture",
                "runtime_spec": "1.2.0",
                "runtime_digest": "sha256:629ecefe6e91e307e72dd9bce8f4e9234f6bc2403bc8387253c7e4f0c4c4e6e0",
                "runtime_size": 12,
                "kernel_release": "fixture",
                "runtime_invocation": {"kind": "direct"},
                "runtime_config": {"kind": "accepted"},
                "filesystem": {"kind": "overlay_fs", "profile": "fixture"}
            }
        },
        "process": process_facts(),
        "stdout": stored_bytes(PRIMARY_STDOUT),
        "stderr": stored_bytes(PRIMARY_STDERR),
        "final_image": {"availability": "not_applicable"},
        "operation_errors": [],
        "managed_service": service
    })
}

fn process_facts() -> Value {
    serde_json::json!({
        "availability": "available",
        "facts": {
            "terminal_outcome": "process_exited",
            "exit_code": 0,
            "started_at": "2026-08-21T00:00:00Z",
            "ended_at": "2026-08-21T00:00:01Z",
            "oom_killed": false,
            "backend_error": null
        }
    })
}

fn descriptor(seed: char) -> Value {
    serde_json::json!({
        "digest": digest_for_seed(seed),
        "size": 1,
        "media_type": "application/vnd.oci.image.manifest.v1+json"
    })
}

fn digest_for_seed(seed: char) -> String {
    format!("sha256:{}", seed.to_string().repeat(64))
}

fn stored_bytes(bytes: &[u8]) -> Value {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("encode digest");
    }
    serde_json::json!({
        "availability": "available",
        "digest": format!("sha256:{hex}"),
        "size": bytes.len()
    })
}
