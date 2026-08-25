use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use flate2::Compression;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tar::{Builder, EntryType, Header};
use tempfile::TempDir;

const IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const IMAGE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    index: Vec<u8>,
    manifest: Blob,
    config: Blob,
    layer: Blob,
}

#[derive(Clone)]
struct Blob {
    digest: String,
    bytes: Vec<u8>,
    media_type: &'static str,
}

#[test]
fn imports_read_only_layout_and_archive_without_docker() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write(source.path(), "amd64", b"hello from OCI\n");
    make_read_only(&fixture.root);
    let sentinel = DockerSentinel::new();

    let layout_state = TempDir::new().expect("layout state");
    let output = runlab(&sentinel, layout_state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--source-reference",
            "source:test",
            "--platform",
            "linux/amd64",
            "--name",
            "local/agent:test",
            "--description",
            "fixture image",
        ])
        .output()
        .expect("run image import");
    let result = success_json(&output);
    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["source_kind"], "oci_layout");
    assert_eq!(
        result["selected_manifest"]["digest"],
        fixture.manifest.digest
    );
    assert_eq!(
        result["platform"],
        json!({"os": "linux", "architecture": "amd64"})
    );
    assert_eq!(result["verified_blobs"], 3);
    assert_eq!(result["local_reference"], "local/agent:test");
    assert_exact_graph(layout_state.path(), &fixture);
    assert_catalog_and_file(&sentinel, layout_state.path(), &fixture);

    let archive_directory = TempDir::new().expect("archive directory");
    let archive = archive_directory.path().join("image.tar");
    fixture.write_archive(&archive, ArchiveMutation::None);
    let archive_state = TempDir::new().expect("archive state");
    let output = runlab(&sentinel, archive_state.path())
        .args([
            "image",
            "import",
            archive.to_str().expect("UTF-8 archive"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/archive:test",
        ])
        .output()
        .expect("run archive import");
    let result = success_json(&output);
    assert_eq!(result["source_kind"], "oci_archive");
    assert_eq!(
        result["selected_manifest"]["digest"],
        fixture.manifest.digest
    );
    assert_exact_graph(archive_state.path(), &fixture);
    assert!(!sentinel.called());
}

#[test]
fn runtime_config_authoring_matches_the_selected_run_network() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write_with_config(
        source.path(),
        "amd64",
        b"runtime config authoring\n",
        &json!({"Entrypoint": ["/bin/true"]}),
    );
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    import_named_fixture(&sentinel, state.path(), &fixture, "local/runtime:test");

    let default_path = state.path().join("default.json");
    let explicit_none_path = state.path().join("none.json");
    let egress_path = state.path().join("egress.json");
    for (network, output_path) in [
        (None, &default_path),
        (Some("none"), &explicit_none_path),
        (Some("egress"), &egress_path),
    ] {
        let mut command = runlab(&sentinel, state.path());
        command.args([
            "runtime-config",
            "create",
            "local/runtime:test",
            "--output",
            output_path.to_str().expect("UTF-8 output path"),
        ]);
        if let Some(network) = network {
            command.args(["--network", network]);
        }
        let result = success_json(&command.output().expect("runtime-config create"));
        assert_eq!(result["schema_version"], 1);
        assert_eq!(result["manifest_digest"], fixture.manifest.digest);
        assert_eq!(
            result["output"],
            output_path
                .canonicalize()
                .expect("canonical output path")
                .to_str()
                .expect("UTF-8 canonical output path")
        );
        assert_eq!(
            result["size"],
            fs::metadata(output_path).expect("output metadata").len()
        );
    }

    assert_eq!(
        fs::read(&default_path).expect("default config"),
        fs::read(&explicit_none_path).expect("explicit none config")
    );
    assert_eq!(
        runtime_namespace_types(&default_path),
        ["pid", "network", "ipc", "uts", "mount", "cgroup"]
    );
    assert_eq!(
        runtime_namespace_types(&egress_path),
        ["pid", "ipc", "uts", "mount", "cgroup"]
    );
    assert!(!sentinel.called());
}

#[test]
fn catalog_remove_is_idempotent_and_retains_oci_content() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write(source.path(), "amd64", b"retained content\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();

    let imported = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/retained:test",
        ])
        .output()
        .expect("image import");
    success_json(&imported);

    let removed = runlab(&sentinel, state.path())
        .args(["image", "catalog", "remove", "local/retained:test"])
        .output()
        .expect("Catalog remove");
    let removed = success_json(&removed);
    assert_eq!(
        removed,
        json!({
            "schema_version": 1,
            "reference": "local/retained:test",
            "removed": true,
            "previous": {
                "name": "local/retained",
                "tag": "test",
                "reference": "local/retained:test",
                "manifest": {
                    "media_type": IMAGE_MANIFEST,
                    "digest": fixture.manifest.digest,
                    "size": fixture.manifest.bytes.len()
                },
                "platform": {"os": "linux", "architecture": "amd64"},
                "description": null,
                "source": format!("oci-layout@{}", digest(&fixture.index)),
                "maintainer": "local"
            }
        })
    );

    let removed_again = runlab(&sentinel, state.path())
        .args(["image", "catalog", "remove", "local/retained:test"])
        .output()
        .expect("repeated Catalog remove");
    let removed_again = success_json(&removed_again);
    assert_eq!(removed_again["removed"], false);
    assert!(removed_again["previous"].is_null());

    let missing = runlab(&sentinel, state.path())
        .args(["image", "catalog", "show", "local/retained:test"])
        .output()
        .expect("Catalog show after remove");
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("local OCI reference is unknown: local/retained:test")
    );

    let inspected = runlab(&sentinel, state.path())
        .args(["image", "inspect", &fixture.manifest.digest])
        .output()
        .expect("inspect retained Manifest");
    let inspected = success_json(&inspected);
    assert_eq!(inspected["manifest"]["digest"], fixture.manifest.digest);
    assert_exact_graph(state.path(), &fixture);
    assert!(!sentinel.called());
}

#[test]
fn gc_plan_is_verified_replayable_and_removes_only_orphans() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write(source.path(), "amd64", b"garbage collect me\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();

    import_named_fixture(&sentinel, state.path(), &fixture, "local/gc:test");
    let removed = runlab(&sentinel, state.path())
        .args(["image", "catalog", "remove", "local/gc:test"])
        .output()
        .expect("Catalog remove");
    success_json(&removed);

    let verified = runlab(&sentinel, state.path())
        .args(["state", "verify"])
        .output()
        .expect("state verify");
    let verified = success_json(&verified);
    assert_eq!(verified["valid"], true);
    assert_eq!(verified["catalog_entries"], 0);
    assert_eq!(verified["runs"], 0);
    assert_eq!(verified["reachable_oci_blobs"], 0);
    assert_eq!(verified["orphan_oci_blobs"], 3);
    assert_eq!(verified["staging_entries"], 0);

    let plans = TempDir::new().expect("plan directory");
    let plan = plans.path().join("gc-plan.json");
    let planned = runlab(&sentinel, state.path())
        .args([
            "state",
            "gc",
            "plan",
            "--output",
            plan.to_str().expect("UTF-8 plan path"),
        ])
        .output()
        .expect("GC plan");
    let planned = success_json(&planned);
    assert_eq!(planned["delete_oci_blobs"], 3);
    assert_eq!(planned["roots"], 0);
    let plan_value: Value =
        serde_json::from_slice(&fs::read(&plan).expect("GC plan bytes")).expect("GC plan JSON");
    assert_eq!(
        plan_value["delete"].as_array().expect("delete set").len(),
        3
    );

    let mut tampered = plan_value;
    tampered["delete"][0]["size"] = json!(1);
    let tampered_path = plans.path().join("tampered-plan.json");
    fs::write(
        &tampered_path,
        serde_json::to_vec(&tampered).expect("tampered plan JSON"),
    )
    .expect("tampered plan");
    let rejected = runlab(&sentinel, state.path())
        .args([
            "state",
            "gc",
            "apply",
            tampered_path.to_str().expect("UTF-8 plan path"),
        ])
        .output()
        .expect("tampered GC apply");
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("plan digest mismatch"));
    assert_exact_graph(state.path(), &fixture);

    let applied = runlab(&sentinel, state.path())
        .args([
            "state",
            "gc",
            "apply",
            plan.to_str().expect("UTF-8 plan path"),
        ])
        .output()
        .expect("GC apply");
    let applied = success_json(&applied);
    assert_eq!(applied["deleted_oci_blobs"], 3);
    assert_eq!(applied["already_absent_oci_blobs"], 0);
    assert_eq!(applied["skipped_reachable_oci_blobs"], 0);
    assert_eq!(applied["failed"], 0);

    let reapplied = runlab(&sentinel, state.path())
        .args([
            "state",
            "gc",
            "apply",
            plan.to_str().expect("UTF-8 plan path"),
        ])
        .output()
        .expect("repeated GC apply");
    let reapplied = success_json(&reapplied);
    assert_eq!(reapplied["deleted_oci_blobs"], 0);
    assert_eq!(reapplied["already_absent_oci_blobs"], 3);
    assert_eq!(reapplied["failed"], 0);
    assert!(!sentinel.called());
}

#[test]
fn gc_plan_refuses_unreconciled_recovery_entries() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write(source.path(), "amd64", b"retained for recovery\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    import_named_fixture(&sentinel, state.path(), &fixture, "local/recovery:test");

    fs::create_dir_all(state.path().join("recovery/native/unresolved")).expect("recovery entry");
    let plans = TempDir::new().expect("plan directory");
    let plan = plans.path().join("gc-plan.json");
    let rejected = runlab(&sentinel, state.path())
        .args([
            "state",
            "gc",
            "plan",
            "--output",
            plan.to_str().expect("UTF-8 plan path"),
        ])
        .output()
        .expect("GC plan");
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("reconciled: 1 entries"));
    assert!(!plan.exists());
    assert_exact_graph(state.path(), &fixture);
    assert!(!sentinel.called());
}

#[test]
fn gc_apply_refuses_recovery_created_after_the_plan() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write(source.path(), "amd64", b"apply-time recovery guard\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    import_named_fixture(&sentinel, state.path(), &fixture, "local/apply-guard:test");
    success_json(
        &runlab(&sentinel, state.path())
            .args(["image", "catalog", "remove", "local/apply-guard:test"])
            .output()
            .expect("Catalog remove"),
    );

    let plans = TempDir::new().expect("plan directory");
    let plan = plans.path().join("gc-plan.json");
    success_json(
        &runlab(&sentinel, state.path())
            .args([
                "state",
                "gc",
                "plan",
                "--output",
                plan.to_str().expect("UTF-8 plan path"),
            ])
            .output()
            .expect("GC plan"),
    );
    fs::create_dir_all(state.path().join("recovery/native/unresolved")).expect("recovery entry");

    let rejected = runlab(&sentinel, state.path())
        .args([
            "state",
            "gc",
            "apply",
            plan.to_str().expect("UTF-8 plan path"),
        ])
        .output()
        .expect("GC apply");
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("reconciled: 1 entries"));
    assert_exact_graph(state.path(), &fixture);
    assert!(!sentinel.called());
}

#[test]
fn stale_gc_plan_never_deletes_content_that_became_reachable() {
    let source = TempDir::new().expect("source directory");
    let fixture = Fixture::write(source.path(), "amd64", b"reachable later\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let imported = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/stale:test",
        ])
        .output()
        .expect("image import");
    success_json(&imported);
    success_json(
        &runlab(&sentinel, state.path())
            .args(["image", "catalog", "remove", "local/stale:test"])
            .output()
            .expect("Catalog remove"),
    );

    let plans = TempDir::new().expect("plan directory");
    let plan = plans.path().join("stale-plan.json");
    success_json(
        &runlab(&sentinel, state.path())
            .args([
                "state",
                "gc",
                "plan",
                "--output",
                plan.to_str().expect("UTF-8 plan path"),
            ])
            .output()
            .expect("GC plan"),
    );
    success_json(
        &runlab(&sentinel, state.path())
            .args([
                "image",
                "catalog",
                "set",
                "local/reachable:test",
                &fixture.manifest.digest,
            ])
            .output()
            .expect("Catalog set"),
    );

    let applied = success_json(
        &runlab(&sentinel, state.path())
            .args([
                "state",
                "gc",
                "apply",
                plan.to_str().expect("UTF-8 plan path"),
            ])
            .output()
            .expect("GC apply"),
    );
    assert_eq!(applied["deleted_oci_blobs"], 0);
    assert_eq!(applied["skipped_reachable_oci_blobs"], 3);
    assert_exact_graph(state.path(), &fixture);
    assert!(!sentinel.called());
}

#[test]
fn stale_gc_plan_does_not_expand_when_more_content_becomes_unreachable() {
    let first_source = TempDir::new().expect("first source");
    let first = Fixture::write(first_source.path(), "amd64", b"planned orphan\n");
    let second_source = TempDir::new().expect("second source");
    let second = Fixture::write(second_source.path(), "amd64", b"later orphan\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    for (fixture, name) in [
        (&first, "local/first:latest"),
        (&second, "local/second:latest"),
    ] {
        success_json(
            &runlab(&sentinel, state.path())
                .args([
                    "image",
                    "import",
                    fixture.root.to_str().expect("UTF-8 source"),
                    "--platform",
                    "linux/amd64",
                    "--name",
                    name,
                ])
                .output()
                .expect("image import"),
        );
    }
    success_json(
        &runlab(&sentinel, state.path())
            .args(["image", "catalog", "remove", "local/first:latest"])
            .output()
            .expect("first Catalog remove"),
    );
    let plans = TempDir::new().expect("plan directory");
    let plan = plans.path().join("bounded-plan.json");
    let planned = success_json(
        &runlab(&sentinel, state.path())
            .args([
                "state",
                "gc",
                "plan",
                "--output",
                plan.to_str().expect("UTF-8 plan path"),
            ])
            .output()
            .expect("GC plan"),
    );
    assert_eq!(planned["delete_oci_blobs"], 3);
    success_json(
        &runlab(&sentinel, state.path())
            .args(["image", "catalog", "remove", "local/second:latest"])
            .output()
            .expect("second Catalog remove"),
    );

    let applied = success_json(
        &runlab(&sentinel, state.path())
            .args([
                "state",
                "gc",
                "apply",
                plan.to_str().expect("UTF-8 plan path"),
            ])
            .output()
            .expect("GC apply"),
    );
    assert_eq!(applied["deleted_oci_blobs"], 3);
    assert_exact_graph(state.path(), &second);
    let verified = success_json(
        &runlab(&sentinel, state.path())
            .args(["state", "verify"])
            .output()
            .expect("state verify"),
    );
    assert_eq!(verified["orphan_oci_blobs"], 3);
    assert!(!sentinel.called());
}

#[test]
fn catalog_set_moves_a_reference_and_preserves_omitted_description() {
    let first_source = TempDir::new().expect("first source");
    let first = Fixture::write(first_source.path(), "amd64", b"first\n");
    let second_source = TempDir::new().expect("second source");
    let second = Fixture::write(second_source.path(), "amd64", b"second\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();

    for (fixture, name, description) in [
        (&first, "local/target:test", "first description"),
        (&second, "local/source:test", "second description"),
    ] {
        let imported = runlab(&sentinel, state.path())
            .args([
                "image",
                "import",
                fixture.root.to_str().expect("UTF-8 source"),
                "--platform",
                "linux/amd64",
                "--name",
                name,
                "--description",
                description,
            ])
            .output()
            .expect("image import");
        success_json(&imported);
    }

    let moved = runlab(&sentinel, state.path())
        .args([
            "image",
            "catalog",
            "set",
            "local/target:test",
            &second.manifest.digest,
            "--description",
            "moved description",
        ])
        .output()
        .expect("Catalog set");
    let moved = success_json(&moved);
    assert_eq!(moved["changed"], true);
    assert_eq!(
        moved["previous"]["manifest"]["digest"],
        first.manifest.digest
    );
    assert_eq!(moved["entry"]["manifest"]["digest"], second.manifest.digest);
    assert_eq!(moved["entry"]["description"], "moved description");

    let unchanged = runlab(&sentinel, state.path())
        .args([
            "image",
            "catalog",
            "set",
            "local/target:test",
            &second.manifest.digest,
        ])
        .output()
        .expect("idempotent Catalog set");
    let unchanged = success_json(&unchanged);
    assert_eq!(unchanged["changed"], false);
    assert_eq!(unchanged["entry"]["description"], "moved description");

    let cleared = runlab(&sentinel, state.path())
        .args([
            "image",
            "catalog",
            "set",
            "local/target:test",
            &second.manifest.digest,
            "--clear-description",
        ])
        .output()
        .expect("clear Catalog description");
    let cleared = success_json(&cleared);
    assert_eq!(cleared["changed"], true);
    assert!(cleared["entry"]["description"].is_null());
    assert_exact_graph(state.path(), &first);
    assert_exact_graph(state.path(), &second);
    assert!(!sentinel.called());
}

#[test]
fn image_inspect_rejects_semantically_invalid_layers() {
    let source = TempDir::new().expect("source directory");
    let fixture =
        Fixture::write_with_layer(source.path(), "amd64", &non_directory_ancestor_layer());
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();

    let imported = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/invalid:test",
        ])
        .output()
        .expect("invalid image import");
    assert_eq!(imported.status.code(), Some(1));
    assert!(imported.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&imported.stderr).contains("non-directory ancestor"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let inspected = runlab(&sentinel, state.path())
        .args(["image", "inspect", &fixture.manifest.digest])
        .output()
        .expect("inspect invalid image");
    assert_eq!(inspected.status.code(), Some(1));
    assert!(inspected.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&inspected.stderr).contains("non-directory ancestor"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    assert!(!sentinel.called());
}

#[test]
fn corruption_does_not_move_an_existing_catalog_reference() {
    let valid_source = TempDir::new().expect("valid source");
    let valid = Fixture::write(valid_source.path(), "amd64", b"valid\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let imported = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            valid.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/agent:latest",
        ])
        .output()
        .expect("initial import");
    success_json(&imported);
    let index_path = state.path().join("oci/index.json");
    let index_before = fs::read(&index_path).expect("Catalog index before failure");

    let corrupt_source = TempDir::new().expect("corrupt source");
    let corrupt = Fixture::write(corrupt_source.path(), "amd64", b"corrupt parent\n");
    fs::write(
        corrupt.blob_path(&corrupt.layer.digest),
        b"not the selected Layer",
    )
    .expect("corrupt Layer bytes");
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            corrupt.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/agent:latest",
        ])
        .output()
        .expect("corrupt import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("digest mismatch"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(index_path).expect("Catalog index after failure"),
        index_before
    );
    let shown = runlab(&sentinel, state.path())
        .args(["image", "catalog", "show", "local/agent:latest"])
        .output()
        .expect("Catalog show");
    let shown = success_json(&shown);
    assert_eq!(shown["entry"]["manifest"]["digest"], valid.manifest.digest);
    assert!(!sentinel.called());
}

#[test]
fn archive_rejects_duplicate_and_non_regular_members() {
    let source = TempDir::new().expect("source");
    let fixture = Fixture::write(source.path(), "amd64", b"archive safety\n");
    let sentinel = DockerSentinel::new();
    for mutation in [
        ArchiveMutation::DuplicateIndex,
        ArchiveMutation::Symlink,
        ArchiveMutation::UnsafeAbsolute,
        ArchiveMutation::TrailingData,
        ArchiveMutation::BadChecksum,
        ArchiveMutation::Truncated,
        ArchiveMutation::PaxSizeOverride,
    ] {
        let archive_directory = TempDir::new().expect("archive directory");
        let archive = archive_directory.path().join("bad.tar");
        fixture.write_archive(&archive, mutation);
        let state = TempDir::new().expect("state");
        let output = runlab(&sentinel, state.path())
            .args([
                "image",
                "import",
                archive.to_str().expect("UTF-8 archive"),
                "--platform",
                "linux/amd64",
                "--name",
                "local/bad:test",
            ])
            .output()
            .expect("bad archive import");
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("duplicate member")
                || stderr.contains("not a regular file")
                || stderr.contains("unsafe member path")
                || stderr.contains("nonzero trailing data")
                || stderr.contains("checksum mismatch")
                || stderr.contains("exceeds the source file")
                || stderr.contains("PAX size overrides"),
            "unexpected stderr: {stderr}"
        );
    }
    assert!(!sentinel.called());
}

#[test]
fn nested_index_and_exact_manifest_selection_are_supported() {
    let source = TempDir::new().expect("source");
    let mut fixture = Fixture::write(source.path(), "amd64", b"nested\n");
    fixture.wrap_in_nested_index();
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &fixture.manifest.digest,
            "--platform",
            "linux/amd64",
            "--name",
            "local/nested:test",
        ])
        .output()
        .expect("nested import");
    let result = success_json(&output);
    assert_eq!(
        result["selected_manifest"]["digest"],
        fixture.manifest.digest
    );
    assert!(!sentinel.called());
}

#[test]
fn ambiguous_platform_requires_an_exact_manifest() {
    let source = TempDir::new().expect("source");
    let mut first = Fixture::write(source.path(), "amd64", b"first\n");
    let other_source = TempDir::new().expect("other source");
    let second = Fixture::write(other_source.path(), "amd64", b"second\n");
    for blob in [&second.manifest, &second.config, &second.layer] {
        fs::copy(blob.path(&second.root), blob.path(&first.root)).expect("copy second graph");
    }
    first.replace_index(&json!({
        "schemaVersion": 2,
        "mediaType": IMAGE_INDEX,
        "manifests": [
            manifest_index_entry(&first.manifest, "amd64", "source:first"),
            manifest_index_entry(&second.manifest, "amd64", "source:second")
        ]
    }));
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            first.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/ambiguous:test",
        ])
        .output()
        .expect("ambiguous import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("multiple Image Manifests"));

    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            first.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &second.manifest.digest,
            "--platform",
            "linux/amd64",
            "--name",
            "local/selected:test",
        ])
        .output()
        .expect("exact import");
    let result = success_json(&output);
    assert_eq!(
        result["selected_manifest"]["digest"],
        second.manifest.digest
    );
    assert!(!sentinel.called());
}

#[test]
fn exact_manifest_isolated_from_an_unrelated_invalid_candidate() {
    let source = TempDir::new().expect("source");
    let mut selected = Fixture::write(source.path(), "amd64", b"selected\n");
    let unrelated_source = TempDir::new().expect("unrelated source");
    let unrelated = Fixture::write(unrelated_source.path(), "amd64", b"unrelated\n");
    for blob in [&unrelated.manifest, &unrelated.config, &unrelated.layer] {
        fs::copy(blob.path(&unrelated.root), blob.path(&selected.root))
            .expect("copy unrelated graph");
    }
    fs::write(
        unrelated.config.path(&selected.root),
        b"corrupt unrelated Config",
    )
    .expect("corrupt unrelated Config");
    selected.replace_index(&json!({
        "schemaVersion": 2,
        "mediaType": IMAGE_INDEX,
        "manifests": [
            manifest_index_entry(&selected.manifest, "amd64", "source:selected"),
            {
                "mediaType": IMAGE_MANIFEST,
                "digest": unrelated.manifest.digest,
                "size": unrelated.manifest.bytes.len(),
                "annotations": {"org.opencontainers.image.ref.name": "source:unrelated"}
            }
        ]
    }));
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            selected.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &selected.manifest.digest,
            "--platform",
            "linux/amd64",
            "--name",
            "local/isolated:test",
        ])
        .output()
        .expect("exact import");
    let result = success_json(&output);
    assert_eq!(
        result["selected_manifest"]["digest"],
        selected.manifest.digest
    );
    assert!(!sentinel.called());
}

#[test]
fn descriptor_and_config_platform_mismatch_is_rejected() {
    let source = TempDir::new().expect("source");
    let mut fixture = Fixture::write(source.path(), "amd64", b"platform mismatch\n");
    let mut index: Value = serde_json::from_slice(&fixture.index).expect("index JSON");
    index["manifests"][0]["platform"]["architecture"] = json!("arm64");
    fixture.replace_index(&index);
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &fixture.manifest.digest,
            "--platform",
            "linux/amd64",
            "--name",
            "local/mismatch:test",
        ])
        .output()
        .expect("platform mismatch import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("descriptor platform mismatch"));
    assert!(!sentinel.called());
}

#[test]
fn unsupported_declared_platform_is_not_reinterpreted_from_config() {
    let source = TempDir::new().expect("source");
    let mut fixture = Fixture::write(source.path(), "amd64", b"unsupported platform\n");
    let mut index: Value = serde_json::from_slice(&fixture.index).expect("index JSON");
    index["manifests"][0]["platform"]["os"] = json!("windows");
    fixture.replace_index(&index);
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &fixture.manifest.digest,
            "--platform",
            "linux/amd64",
            "--name",
            "local/windows:test",
        ])
        .output()
        .expect("unsupported declared platform import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("descriptor platform mismatch"));
    assert!(!sentinel.called());
}

#[test]
fn overlapping_source_and_state_are_rejected_without_catalog_mutation() {
    let state = TempDir::new().expect("state");
    let fixture = Fixture::write(&state.path().join("oci"), "amd64", b"overlap\n");
    let sentinel = DockerSentinel::new();
    let before = snapshot_tree(state.path());
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 destination Layout"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/overlap:test",
        ])
        .output()
        .expect("overlapping import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not overlap"));
    assert_eq!(snapshot_tree(state.path()), before);
    assert!(!sentinel.called());
}

#[test]
fn concurrent_importers_preserve_both_catalog_references() {
    let source = TempDir::new().expect("source");
    let fixture = Fixture::write(source.path(), "amd64", b"concurrent\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let mut first = runlab(&sentinel, state.path());
    first
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/first:test",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second = runlab(&sentinel, state.path());
    second
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/second:test",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let first = first.spawn().expect("first importer");
    let second = second.spawn().expect("second importer");
    success_json(&first.wait_with_output().expect("first result"));
    success_json(&second.wait_with_output().expect("second result"));
    let output = runlab(&sentinel, state.path())
        .args(["image", "catalog", "list", "--limit", "10"])
        .output()
        .expect("Catalog list");
    let result = success_json(&output);
    let references = result["entries"]
        .as_array()
        .expect("Catalog entries")
        .iter()
        .map(|entry| entry["reference"].as_str().expect("reference"))
        .collect::<Vec<_>>();
    assert_eq!(references, vec!["local/first:test", "local/second:test"]);
    assert!(!sentinel.called());
}

#[test]
fn selection_uses_config_platform_and_rejects_unrooted_manifest() {
    let source = TempDir::new().expect("source");
    let mut fixture = Fixture::write(source.path(), "amd64", b"platform\n");
    let mut index: Value = serde_json::from_slice(&fixture.index).expect("index JSON");
    index["manifests"][0]
        .as_object_mut()
        .expect("Manifest descriptor")
        .remove("platform");
    fixture.replace_index(&index);
    let sentinel = DockerSentinel::new();
    let state = TempDir::new().expect("state");
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/platform:test",
        ])
        .output()
        .expect("platform-less import");
    success_json(&output);

    let state = TempDir::new().expect("unrooted state");
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &format!("sha256:{}", "a".repeat(64)),
            "--platform",
            "linux/amd64",
            "--name",
            "local/unrooted:test",
        ])
        .output()
        .expect("unrooted Manifest import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not reachable"));
    assert!(!sentinel.called());
}

#[cfg(unix)]
#[test]
fn directory_source_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().expect("source");
    let fixture = Fixture::write(source.path(), "amd64", b"symlink source\n");
    let link_directory = TempDir::new().expect("link directory");
    let link = link_directory.path().join("source");
    symlink(&fixture.root, &link).expect("source symlink");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            link.to_str().expect("UTF-8 link"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/symlink:test",
        ])
        .output()
        .expect("symlink source import");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not be a symlink"));
    assert!(!sentinel.called());
}

#[cfg(target_os = "linux")]
#[test]
fn directory_source_fifo_is_rejected_without_blocking() {
    use rustix::fs::{CWD, Mode, mkfifoat};

    let source = TempDir::new().expect("source");
    let fixture = Fixture::write(source.path(), "amd64", b"FIFO source\n");
    fs::remove_file(fixture.root.join("index.json")).expect("remove index");
    mkfifoat(
        CWD,
        fixture.root.join("index.json"),
        Mode::RUSR | Mode::WUSR,
    )
    .expect("index FIFO");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let mut command = runlab(&sentinel, state.path());
    command
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            "local/fifo:test",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("FIFO import");
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll FIFO import") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill blocked FIFO import");
            child.wait().expect("reap blocked FIFO import");
            panic!("OCI import blocked while opening a FIFO source member");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("FIFO stdout")
        .read_to_end(&mut stdout)
        .expect("read FIFO stdout");
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("FIFO stderr")
        .read_to_end(&mut stderr)
        .expect("read FIFO stderr");
    assert_eq!(status.code(), Some(1));
    assert!(stdout.is_empty());
    assert!(String::from_utf8_lossy(&stderr).contains("not a regular file"));
    assert!(!sentinel.called());
}

#[test]
fn manifest_and_source_reference_are_mutually_exclusive() {
    let source = TempDir::new().expect("source");
    let fixture = Fixture::write(source.path(), "amd64", b"selector\n");
    let state = TempDir::new().expect("state");
    let sentinel = DockerSentinel::new();
    let output = runlab(&sentinel, state.path())
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--manifest",
            &fixture.manifest.digest,
            "--source-reference",
            "source:test",
            "--name",
            "local/test",
        ])
        .output()
        .expect("invalid selector combination");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    assert!(!sentinel.called());
}

impl Fixture {
    fn write(root: &Path, architecture: &str, content: &[u8]) -> Self {
        Self::write_with_layer(root, architecture, &layer_tar(content))
    }

    fn write_with_config(
        root: &Path,
        architecture: &str,
        content: &[u8],
        container_config: &Value,
    ) -> Self {
        Self::write_with_layer_and_config(root, architecture, &layer_tar(content), container_config)
    }

    fn write_with_layer(root: &Path, architecture: &str, layer_tar: &[u8]) -> Self {
        Self::write_with_layer_and_config(root, architecture, layer_tar, &json!({}))
    }

    fn write_with_layer_and_config(
        root: &Path,
        architecture: &str,
        layer_tar: &[u8],
        container_config: &Value,
    ) -> Self {
        fs::create_dir_all(root.join("blobs/sha256")).expect("blob directory");
        fs::write(
            root.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .expect("layout marker");

        let layer = Blob::new(gzip(layer_tar), LAYER_GZIP);
        let config = Blob::new(
            serde_json::to_vec(&json!({
                "architecture": architecture,
                "os": "linux",
                "rootfs": {"type": "layers", "diff_ids": [digest(layer_tar)]},
                "config": container_config,
                "x-fixture": true
            }))
            .expect("Image Config JSON"),
            IMAGE_CONFIG,
        );
        let manifest = Blob::new(
            format!(
                "{{\n \"schemaVersion\":2,\n \"config\":{},\n \"layers\":[{}],\n \"annotations\":{{\"fixture\":\"preserved\"}}\n}}\n",
                config.descriptor_json(),
                layer.descriptor_json()
            )
            .into_bytes(),
            IMAGE_MANIFEST,
        );
        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": IMAGE_INDEX,
            "manifests": [{
                "mediaType": IMAGE_MANIFEST,
                "digest": manifest.digest,
                "size": manifest.bytes.len(),
                "platform": {"os": "linux", "architecture": architecture},
                "annotations": {"org.opencontainers.image.ref.name": "source:test"}
            }]
        }))
        .expect("index JSON");
        fs::write(root.join("index.json"), &index).expect("index");
        for blob in [&manifest, &config, &layer] {
            fs::write(blob.path(root), &blob.bytes).expect("blob");
        }
        Self {
            root: root.to_path_buf(),
            index,
            manifest,
            config,
            layer,
        }
    }

    fn blob_path(&self, digest: &str) -> PathBuf {
        self.root.join("blobs/sha256").join(&digest[7..])
    }

    fn replace_index(&mut self, value: &Value) {
        self.index = serde_json::to_vec(value).expect("index JSON");
        fs::write(self.root.join("index.json"), &self.index).expect("replace index");
    }

    fn wrap_in_nested_index(&mut self) {
        let nested = Blob::new(
            serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": IMAGE_INDEX,
                "manifests": [manifest_index_entry(
                    &self.manifest,
                    "amd64",
                    "nested:test"
                )]
            }))
            .expect("nested index JSON"),
            IMAGE_INDEX,
        );
        fs::write(nested.path(&self.root), &nested.bytes).expect("nested index blob");
        self.replace_index(&json!({
            "schemaVersion": 2,
            "mediaType": IMAGE_INDEX,
            "manifests": [{
                "mediaType": IMAGE_INDEX,
                "digest": nested.digest,
                "size": nested.bytes.len(),
                "annotations": {"org.opencontainers.image.ref.name": "source:test"}
            }]
        }));
    }

    fn write_archive(&self, path: &Path, mutation: ArchiveMutation) {
        let file = File::create(path).expect("archive file");
        let mut archive = Builder::new(file);
        append(
            &mut archive,
            "oci-layout",
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        );
        append(&mut archive, "index.json", &self.index);
        if mutation == ArchiveMutation::DuplicateIndex {
            append(&mut archive, "./index.json", &self.index);
        }
        if mutation == ArchiveMutation::PaxSizeOverride {
            let payload = b"10 size=1\n";
            let mut header = header(
                u64::try_from(payload.len()).expect("PAX payload size"),
                EntryType::XHeader,
            );
            archive
                .append_data(&mut header, "PaxHeaders/size", payload.as_slice())
                .expect("PAX size override");
        }
        for blob in [&self.manifest, &self.config, &self.layer] {
            append(
                &mut archive,
                &format!("blobs/sha256/{}", &blob.digest[7..]),
                &blob.bytes,
            );
        }
        if mutation == ArchiveMutation::Symlink {
            let mut header = header(0, EntryType::Symlink);
            header.set_link_name("index.json").expect("symlink target");
            header.set_cksum();
            archive
                .append_data(&mut header, "extra-link", Cursor::new(Vec::<u8>::new()))
                .expect("symlink member");
        }
        if mutation == ArchiveMutation::UnsafeAbsolute {
            let bytes = b"unsafe";
            let mut header = header(
                u64::try_from(bytes.len()).expect("unsafe member size"),
                EntryType::Regular,
            );
            header
                .set_path_absolute("/unsafe")
                .expect("absolute member path");
            header.set_cksum();
            archive
                .append(&header, bytes.as_slice())
                .expect("unsafe member");
        }
        archive.finish().expect("finish archive");
        archive
            .into_inner()
            .expect("close archive")
            .sync_all()
            .expect("sync archive");
        if mutation == ArchiveMutation::TrailingData {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(path)
                .expect("open archive for trailing data");
            file.write_all(b"nonzero").expect("trailing data");
            file.sync_all().expect("sync trailing data");
        }
        if mutation == ArchiveMutation::BadChecksum {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .expect("open archive for checksum corruption");
            let mut byte = [0_u8; 1];
            file.read_exact(&mut byte).expect("read archive byte");
            byte[0] ^= 1;
            file.seek(SeekFrom::Start(0)).expect("rewind archive");
            file.write_all(&byte).expect("corrupt archive checksum");
            file.sync_all().expect("sync checksum corruption");
        }
        if mutation == ArchiveMutation::Truncated {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open archive for truncation")
                .set_len(700)
                .expect("truncate archive");
        }
    }
}

impl Blob {
    fn new(bytes: Vec<u8>, media_type: &'static str) -> Self {
        Self {
            digest: digest(&bytes),
            bytes,
            media_type,
        }
    }

    fn descriptor_json(&self) -> String {
        format!(
            "{{\"mediaType\":\"{}\",\"digest\":\"{}\",\"size\":{}}}",
            self.media_type,
            self.digest,
            self.bytes.len()
        )
    }

    fn path(&self, root: &Path) -> PathBuf {
        root.join("blobs/sha256").join(&self.digest[7..])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveMutation {
    None,
    DuplicateIndex,
    Symlink,
    UnsafeAbsolute,
    TrailingData,
    BadChecksum,
    Truncated,
    PaxSizeOverride,
}

struct DockerSentinel {
    directory: TempDir,
    marker: PathBuf,
}

impl DockerSentinel {
    fn new() -> Self {
        let directory = TempDir::new().expect("sentinel directory");
        let executable = directory.path().join("docker");
        let marker = directory.path().join("called");
        fs::write(
            &executable,
            format!("#!/bin/sh\n: > '{}'\nexit 97\n", marker.display()),
        )
        .expect("docker sentinel");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
                .expect("sentinel permissions");
        }
        Self { directory, marker }
    }

    fn called(&self) -> bool {
        self.marker.exists()
    }
}

fn import_named_fixture(
    sentinel: &DockerSentinel,
    state: &Path,
    fixture: &Fixture,
    reference: &str,
) {
    let imported = runlab(sentinel, state)
        .args([
            "image",
            "import",
            fixture.root.to_str().expect("UTF-8 source"),
            "--platform",
            "linux/amd64",
            "--name",
            reference,
        ])
        .output()
        .expect("image import");
    success_json(&imported);
}

fn runlab(sentinel: &DockerSentinel, state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_runlab"));
    command
        .arg("--state")
        .arg(state)
        .env("PATH", sentinel.directory.path());
    command
}

fn success_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "command failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = std::str::from_utf8(&output.stdout).expect("UTF-8 JSON");
    assert_eq!(text.lines().count(), 1);
    serde_json::from_str(text).expect("JSON result")
}

fn assert_exact_graph(state: &Path, fixture: &Fixture) {
    for blob in [&fixture.manifest, &fixture.config, &fixture.layer] {
        assert_eq!(
            fs::read(state.join("oci/blobs/sha256").join(&blob.digest[7..])).expect("stored blob"),
            blob.bytes
        );
    }
}

fn assert_catalog_and_file(sentinel: &DockerSentinel, state: &Path, fixture: &Fixture) {
    let show = runlab(sentinel, state)
        .args(["image", "catalog", "show", "local/agent:test"])
        .output()
        .expect("Catalog show");
    let show = success_json(&show);
    assert_eq!(show["entry"]["manifest"]["digest"], fixture.manifest.digest);
    assert_eq!(show["entry"]["description"], "fixture image");

    let output_directory = TempDir::new().expect("output directory");
    let destination = output_directory.path().join("hello");
    let get = runlab(sentinel, state)
        .args([
            "image",
            "file",
            "get",
            "local/agent:test",
            "/hello",
            "--output",
            destination.to_str().expect("UTF-8 output"),
        ])
        .output()
        .expect("file get");
    success_json(&get);
    assert_eq!(
        fs::read(destination).expect("extracted bytes"),
        b"hello from OCI\n"
    );
}

fn layer_tar(content: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut archive = Builder::new(&mut bytes);
        let mut entry = header(
            u64::try_from(content.len()).expect("content size"),
            EntryType::Regular,
        );
        archive
            .append_data(&mut entry, "hello", content)
            .expect("Layer entry");
        archive.finish().expect("finish Layer");
    }
    bytes
}

fn non_directory_ancestor_layer() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut archive = Builder::new(&mut bytes);
        let mut parent = header(0, EntryType::Regular);
        archive
            .append_data(&mut parent, "parent", std::io::empty())
            .expect("parent entry");
        let content = b"child";
        let mut child = header(
            u64::try_from(content.len()).expect("child size"),
            EntryType::Regular,
        );
        archive
            .append_data(&mut child, "parent/child", content.as_slice())
            .expect("child entry");
        archive.finish().expect("finish invalid Layer");
    }
    bytes
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;

    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(Vec::new(), Compression::new(6));
    encoder.write_all(bytes).expect("gzip bytes");
    encoder.finish().expect("finish gzip")
}

fn append(builder: &mut Builder<File>, path: &str, bytes: &[u8]) {
    let mut header = header(
        u64::try_from(bytes.len()).expect("member size"),
        EntryType::Regular,
    );
    builder
        .append_data(&mut header, path, bytes)
        .expect("archive member");
}

fn runtime_namespace_types(path: &Path) -> Vec<String> {
    let config: Value = serde_json::from_slice(&fs::read(path).expect("runtime config bytes"))
        .expect("runtime config JSON");
    config["linux"]["namespaces"]
        .as_array()
        .expect("runtime namespaces")
        .iter()
        .map(|namespace| {
            namespace["type"]
                .as_str()
                .expect("runtime namespace type")
                .to_owned()
        })
        .collect()
}

fn header(size: u64, entry_type: EntryType) -> Header {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(entry_type);
    header.set_cksum();
    header
}

fn digest(bytes: &[u8]) -> String {
    let bytes = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("write digest");
    }
    format!("sha256:{hex}")
}

fn manifest_index_entry(manifest: &Blob, architecture: &str, reference: &str) -> Value {
    json!({
        "mediaType": IMAGE_MANIFEST,
        "digest": manifest.digest,
        "size": manifest.bytes.len(),
        "platform": {"os": "linux", "architecture": architecture},
        "annotations": {"org.opencontainers.image.ref.name": reference}
    })
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (bool, u32, Option<Vec<u8>>)> {
    fn walk(
        root: &Path,
        path: &Path,
        entries: &mut BTreeMap<PathBuf, (bool, u32, Option<Vec<u8>>)>,
    ) {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;

        let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
        #[cfg(unix)]
        let mode = metadata.permissions().mode();
        #[cfg(not(unix))]
        let mode = u32::from(metadata.permissions().readonly());
        let relative = path.strip_prefix(root).expect("snapshot relative path");
        let is_directory = metadata.is_dir();
        let bytes = metadata
            .is_file()
            .then(|| fs::read(path).expect("snapshot file"));
        entries.insert(relative.to_path_buf(), (is_directory, mode, bytes));
        if is_directory {
            for child in fs::read_dir(path).expect("snapshot directory") {
                walk(root, &child.expect("snapshot child").path(), entries);
            }
        }
    }

    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn make_read_only(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        for entry in fs::read_dir(root.join("blobs/sha256")).expect("blob entries") {
            fs::set_permissions(
                entry.expect("blob entry").path(),
                fs::Permissions::from_mode(0o444),
            )
            .expect("blob permissions");
        }
        for file in [root.join("oci-layout"), root.join("index.json")] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o444))
                .expect("metadata permissions");
        }
        for directory in [
            root.join("blobs/sha256"),
            root.join("blobs"),
            root.to_path_buf(),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o555))
                .expect("directory permissions");
        }
    }
}
