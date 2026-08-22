#![cfg(unix)]

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tar::{Builder, EntryType, Header};

const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

#[derive(Clone, Copy)]
enum FixtureEntry<'a> {
    File { path: &'a str, bytes: &'a [u8] },
    Hardlink { path: &'a str, target: &'a str },
}

struct FixtureImage {
    state: PathBuf,
    base_manifest: String,
    manifest: String,
    fake_bin: PathBuf,
    docker_sentinel: PathBuf,
}

struct FixtureLayers {
    lower_tar: Vec<u8>,
    middle_tar: Vec<u8>,
    middle_gzip: Vec<u8>,
    upper_tar: Vec<u8>,
    upper_zstd: Vec<u8>,
}

#[test]
fn file_get_reads_verified_layers_without_docker() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = install_fixture(directory.path());

    let recreated_path = directory.path().join("recreated");
    let recreated = get_file(&fixture, "/recreated", &recreated_path);
    assert_successful_file(&recreated, &recreated_path, b"new\0bytes");

    let lower = get_file(
        &fixture,
        "/visible-from-lower",
        &directory.path().join("lower"),
    );
    assert_successful_file(&lower, &directory.path().join("lower"), b"lower");

    let middle = get_file(&fixture, "/middle", &directory.path().join("middle"));
    assert_successful_file(&middle, &directory.path().join("middle"), b"gzip");

    let hardlink = get_file(
        &fixture,
        "/usr/bin/tool",
        &directory.path().join("hardlink"),
    );
    assert_successful_file(&hardlink, &directory.path().join("hardlink"), b"tool\xff");

    for source in ["/removed", "/opaque/old"] {
        let output = get_file(
            &fixture,
            source,
            &directory
                .path()
                .join(source.trim_start_matches('/').replace('/', "-")),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("image path does not exist"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert!(!fixture.docker_sentinel.exists());
}

#[test]
fn file_get_does_not_overwrite_or_publish_after_integrity_failure() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = install_fixture(directory.path());
    let existing = directory.path().join("existing");
    fs::write(&existing, b"sentinel").expect("existing output");

    let collision = get_file(&fixture, "/recreated", &existing);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(fs::read(&existing).expect("existing output"), b"sentinel");

    let dangling = directory.path().join("dangling");
    symlink("missing-target", &dangling).expect("dangling symlink");
    let collision = get_file(&fixture, "/recreated", &dangling);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(
        fs::read_link(&dangling).expect("dangling symlink"),
        Path::new("missing-target")
    );

    let manifest_path = blob_path(&fixture.state, &fixture.manifest);
    let mut bytes = fs::read(&manifest_path).expect("Manifest bytes");
    bytes[0] ^= 1;
    fs::write(&manifest_path, bytes).expect("corrupt Manifest");
    let absent = directory.path().join("absent");
    let corrupted = get_file(&fixture, "/recreated", &absent);
    assert_eq!(corrupted.status.code(), Some(1));
    assert!(corrupted.stdout.is_empty());
    assert!(!absent.exists());
    assert!(String::from_utf8_lossy(&corrupted.stderr).contains("digest verification"));
    assert!(!fixture.docker_sentinel.exists());
}

#[test]
fn file_get_rejects_descriptor_diffid_and_compression_failures_before_publish() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = install_fixture(directory.path());

    let wrong_size = rewrite_manifest(&fixture, |manifest, _| {
        let size = manifest["layers"][0]["size"].as_u64().expect("Layer size");
        manifest["layers"][0]["size"] = json!(size + 1);
    });
    assert_failed_without_output(
        &get_file_from_manifest(
            &fixture,
            &wrong_size,
            "/recreated",
            &directory.path().join("wrong-size"),
        ),
        &directory.path().join("wrong-size"),
        "size mismatch",
    );

    let wrong_diff_id = rewrite_manifest(&fixture, |manifest, oci| {
        let config_digest = manifest["config"]["digest"]
            .as_str()
            .expect("Config digest");
        let mut config: Value = serde_json::from_slice(
            &fs::read(blob_path_from_oci(oci, config_digest)).expect("Config bytes"),
        )
        .expect("Config JSON");
        config["rootfs"]["diff_ids"][2] = json!(format!("sha256:{}", "0".repeat(64)));
        manifest["config"] = write_blob(
            oci,
            &serde_json::to_vec(&config).expect("Config JSON"),
            OCI_CONFIG,
        );
    });
    assert_failed_without_output(
        &get_file_from_manifest(
            &fixture,
            &wrong_diff_id,
            "/recreated",
            &directory.path().join("wrong-diff-id"),
        ),
        &directory.path().join("wrong-diff-id"),
        "DiffID mismatch",
    );

    let truncated = rewrite_manifest(&fixture, |manifest, oci| {
        let digest = manifest["layers"][2]["digest"]
            .as_str()
            .expect("zstd digest");
        let mut bytes = fs::read(blob_path_from_oci(oci, digest)).expect("zstd bytes");
        bytes.truncate(bytes.len() - 4);
        manifest["layers"][2] = write_blob(oci, &bytes, OCI_LAYER_ZSTD);
    });
    assert_failed_without_output(
        &get_file_from_manifest(
            &fixture,
            &truncated,
            "/recreated",
            &directory.path().join("truncated"),
        ),
        &directory.path().join("truncated"),
        "DiffID",
    );
    assert!(!fixture.docker_sentinel.exists());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one workflow verifies Catalog resolution across every public Image consumer"
)]
fn catalog_references_drive_docker_free_image_reads() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = install_fixture(directory.path());
    let state = fixture.state.to_str().expect("state path");

    let list = run_fixture(&fixture, &["--state", state, "image", "catalog", "list"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list: Value = serde_json::from_slice(&list.stdout).expect("Catalog list JSON");
    assert_eq!(list["entries"][0]["reference"], "runlab/fixture:latest");
    assert_eq!(list["entries"][0]["name"], "runlab/fixture");
    assert_eq!(list["entries"][0]["tag"], "latest");
    assert_eq!(list["entries"][0]["description"], "layer fixture");
    assert_eq!(list["entries"][0]["manifest"]["digest"], fixture.manifest);
    assert!(list["next_after"].is_null());

    let show = run_fixture(
        &fixture,
        &[
            "--state",
            state,
            "image",
            "catalog",
            "show",
            "runlab/fixture",
        ],
    );
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show: Value = serde_json::from_slice(&show.stdout).expect("Catalog show JSON");
    assert_eq!(show["entry"]["reference"], "runlab/fixture:latest");
    assert_eq!(show["entry"]["manifest"]["digest"], fixture.manifest);

    let inspect = run_fixture(
        &fixture,
        &["--state", state, "image", "inspect", "runlab/fixture"],
    );
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let inspect: Value = serde_json::from_slice(&inspect.stdout).expect("Image inspect JSON");
    assert_eq!(inspect["manifest"]["digest"], fixture.manifest);

    let runtime_path = directory.path().join("runtime.json");
    let runtime = run_fixture(
        &fixture,
        &[
            "--state",
            state,
            "runtime-config",
            "create",
            "runlab/fixture",
            "--output",
            runtime_path.to_str().expect("runtime path"),
        ],
    );
    assert!(
        runtime.status.success(),
        "{}",
        String::from_utf8_lossy(&runtime.stderr)
    );
    let runtime: Value = serde_json::from_slice(&runtime.stdout).expect("Runtime result JSON");
    assert_eq!(runtime["requested_reference"], "runlab/fixture:latest");
    assert_eq!(runtime["manifest_digest"], fixture.manifest);
    assert!(runtime_path.is_file());

    let file_path = directory.path().join("from-reference");
    let file = get_file_from_manifest(&fixture, "runlab/fixture", "/recreated", &file_path);
    assert_successful_file(&file, &file_path, b"new\0bytes");
    let file: Value = serde_json::from_slice(&file.stdout).expect("File result JSON");
    assert_eq!(file["requested_reference"], "runlab/fixture:latest");
    assert_eq!(file["manifest_digest"], fixture.manifest);

    let missing = run_fixture(
        &fixture,
        &["--state", state, "image", "inspect", "runlab/missing"],
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("local OCI reference is unknown: runlab/missing:latest")
    );
    let missing_run = run_fixture(
        &fixture,
        &[
            "--state",
            state,
            "run",
            "start",
            "runlab/missing",
            "--runtime-config",
            runtime_path.to_str().expect("runtime path"),
        ],
    );
    assert_eq!(missing_run.status.code(), Some(1));
    assert!(missing_run.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&missing_run.stderr)
            .contains("local OCI reference is unknown: runlab/missing:latest")
    );
    assert!(!fixture.state.join("runs.sqlite3").exists());
    assert!(!fixture.docker_sentinel.exists());
}

#[test]
fn image_diff_is_docker_free_byte_safe_and_paginated() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = install_fixture(directory.path());
    let state = fixture.state.to_str().expect("state path");
    let output = run_fixture(
        &fixture,
        &[
            "--state",
            state,
            "image",
            "diff",
            &fixture.base_manifest,
            "runlab/fixture",
            "--limit",
            "2",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let first: Value = serde_json::from_slice(&output.stdout).expect("Image diff JSON");
    assert_eq!(first["from"]["manifest"]["digest"], fixture.base_manifest);
    assert_eq!(first["to"]["requested_reference"], "runlab/fixture:latest");
    assert_eq!(first["structure"]["common_layer_prefix"], 1);
    assert_eq!(
        first["structure"]["added_layers"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(first["filesystem"]["total_changes"].as_u64().unwrap() > 2);
    assert_eq!(
        first["filesystem"]["changes"].as_array().map(Vec::len),
        Some(2)
    );
    let cursor = first["filesystem"]["next_after_path_hex"]
        .as_str()
        .expect("pagination cursor");
    let second = run_fixture(
        &fixture,
        &[
            "--state",
            state,
            "image",
            "diff",
            &fixture.base_manifest,
            &fixture.manifest,
            "--limit",
            "100",
            "--after-path-hex",
            cursor,
        ],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: Value = serde_json::from_slice(&second.stdout).expect("Image diff page JSON");
    let changes = second["filesystem"]["changes"]
        .as_array()
        .expect("filesystem changes");
    assert!(
        changes
            .iter()
            .all(|change| change["path_hex"].as_str().unwrap() > cursor)
    );
    assert!(changes.iter().any(|change| change["path"] == "/recreated"));
    assert_fixture_export(&fixture, directory.path());
    assert!(!fixture.docker_sentinel.exists());
}

fn assert_fixture_export(fixture: &FixtureImage, directory: &Path) {
    let output_path = directory.join("rootfs.tar");
    let state = fixture.state.to_str().expect("state path");
    let output = run_fixture(
        fixture,
        &[
            "--state",
            state,
            "image",
            "export",
            "runlab/fixture",
            "--output",
            output_path.to_str().expect("export path"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("export JSON");
    assert_eq!(result["requested_reference"], "runlab/fixture:latest");
    assert_eq!(result["format"], "tar");
    let bytes = fs::read(output_path).expect("exported tar");
    assert_eq!(result["digest"], sha256(&bytes));
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut files = std::collections::BTreeMap::new();
    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        if entry.header().entry_type() == EntryType::Regular {
            let mut content = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut content).expect("entry content");
            files.insert(entry.path_bytes().into_owned(), content);
        }
    }
    assert_eq!(files.get(b"recreated".as_slice()).unwrap(), b"new\0bytes");
    assert!(!files.contains_key(b"removed".as_slice()));
    assert!(!files.contains_key(b"opaque/old".as_slice()));
}

fn install_fixture(root: &Path) -> FixtureImage {
    let state = root.join("state");
    let oci = state.join("oci");
    fs::create_dir_all(oci.join("blobs/sha256")).expect("OCI blob directory");
    fs::write(oci.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).expect("oci-layout");

    let fixture = fixture_layers();
    let lower_descriptor = write_blob(&oci, &fixture.lower_tar, OCI_LAYER_TAR);
    let middle_descriptor = write_blob(&oci, &fixture.middle_gzip, OCI_LAYER_GZIP);
    let upper_descriptor = write_blob(&oci, &fixture.upper_zstd, OCI_LAYER_ZSTD);
    let base_manifest = write_manifest(
        &oci,
        std::slice::from_ref(&lower_descriptor),
        &[fixture.lower_tar.as_slice()],
    );
    let manifest = install_manifest(
        &oci,
        &[lower_descriptor, middle_descriptor, upper_descriptor],
        [&fixture.lower_tar, &fixture.middle_tar, &fixture.upper_tar],
    );
    let (fake_bin, docker_sentinel) = install_fake_docker(root);

    FixtureImage {
        state,
        base_manifest,
        manifest,
        fake_bin,
        docker_sentinel,
    }
}

fn fixture_layers() -> FixtureLayers {
    let lower = tar_bytes(&[
        FixtureEntry::File {
            path: "visible-from-lower",
            bytes: b"lower",
        },
        FixtureEntry::File {
            path: "removed",
            bytes: b"removed",
        },
        FixtureEntry::File {
            path: "recreated",
            bytes: b"old",
        },
        FixtureEntry::File {
            path: "opaque/old",
            bytes: b"old",
        },
    ]);
    let middle_tar = tar_bytes(&[FixtureEntry::File {
        path: "middle",
        bytes: b"gzip",
    }]);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::new(6));
    gzip.write_all(&middle_tar).expect("gzip Layer");
    let middle = gzip.finish().expect("finish gzip Layer");
    let upper_tar = tar_bytes(&[
        FixtureEntry::File {
            path: "recreated",
            bytes: b"new\0bytes",
        },
        FixtureEntry::File {
            path: "opaque/new",
            bytes: b"new",
        },
        FixtureEntry::File {
            path: "bin/tool",
            bytes: b"tool\xff",
        },
        FixtureEntry::Hardlink {
            path: "usr/bin/tool",
            target: "bin/tool",
        },
        FixtureEntry::File {
            path: ".wh.recreated",
            bytes: b"",
        },
        FixtureEntry::File {
            path: ".wh.removed",
            bytes: b"",
        },
        FixtureEntry::File {
            path: "opaque/.wh..wh..opq",
            bytes: b"",
        },
    ]);
    let upper = zstd::stream::encode_all(upper_tar.as_slice(), 3).expect("zstd Layer");
    FixtureLayers {
        lower_tar: lower,
        middle_tar,
        middle_gzip: middle,
        upper_tar,
        upper_zstd: upper,
    }
}

fn install_manifest(oci: &Path, layers: &[Value; 3], uncompressed: [&[u8]; 3]) -> String {
    let manifest_digest = write_manifest(oci, layers, &uncompressed);
    let manifest_bytes =
        fs::read(blob_path_from_oci(oci, &manifest_digest)).expect("Manifest bytes");
    let manifest_descriptor = json!({
        "mediaType": OCI_MANIFEST,
        "digest": manifest_digest,
        "size": manifest_bytes.len(),
        "platform": {
            "os": "linux",
            "architecture": fixture_architecture()
        },
        "annotations": {
            "org.opencontainers.image.ref.name": "runlab/fixture:latest",
            "io.runlab.catalog.description": "layer fixture",
            "io.runlab.catalog.source": "fixture",
            "io.runlab.catalog.maintainer": "local"
        }
    });
    fs::write(
        oci.join("index.json"),
        serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "manifests": [manifest_descriptor]
        }))
        .expect("Index JSON"),
    )
    .expect("index.json");
    manifest_descriptor["digest"]
        .as_str()
        .expect("Manifest digest")
        .to_owned()
}

fn write_manifest(oci: &Path, layers: &[Value], uncompressed: &[&[u8]]) -> String {
    let config = serde_json::to_vec(&json!({
        "architecture": fixture_architecture(),
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": uncompressed.iter().map(|bytes| sha256(bytes)).collect::<Vec<_>>()
        },
        "config": {"Entrypoint": ["/bin/true"]}
    }))
    .expect("Config JSON");
    let config_descriptor = write_blob(oci, &config, OCI_CONFIG);
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": config_descriptor,
        "layers": layers
    }))
    .expect("Manifest JSON");
    let manifest_descriptor = write_blob(oci, &manifest, OCI_MANIFEST);
    manifest_descriptor["digest"]
        .as_str()
        .expect("Manifest digest")
        .to_owned()
}

fn install_fake_docker(root: &Path) -> (PathBuf, PathBuf) {
    let fake_bin = root.join("fake-bin");
    fs::create_dir(&fake_bin).expect("fake binary directory");
    let docker_sentinel = root.join("docker-called");
    let docker = fake_bin.join("docker");
    fs::write(
        &docker,
        b"#!/bin/sh\n: > \"$RUNLAB_FAKE_DOCKER_SENTINEL\"\nexit 99\n",
    )
    .expect("fake docker");
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("fake docker mode");
    (fake_bin, docker_sentinel)
}

fn tar_bytes(entries: &[FixtureEntry<'_>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut builder = Builder::new(&mut bytes);
        for entry in entries {
            match entry {
                FixtureEntry::File { path, bytes } => {
                    let mut header = fixture_header(
                        EntryType::Regular,
                        u64::try_from(bytes.len()).expect("entry size"),
                    );
                    builder
                        .append_data(&mut header, path, *bytes)
                        .expect("file entry");
                }
                FixtureEntry::Hardlink { path, target } => {
                    let mut header = fixture_header(EntryType::Link, 0);
                    header.set_path(path).expect("hardlink path");
                    header.set_link_name(target).expect("hardlink target");
                    header.set_cksum();
                    builder.append(&header, &[][..]).expect("hardlink entry");
                }
            }
        }
        builder.finish().expect("finish Layer tar");
    }
    bytes
}

fn fixture_header(entry_type: EntryType, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    header
}

fn write_blob(oci: &Path, bytes: &[u8], media_type: &str) -> Value {
    let digest = sha256(bytes);
    fs::write(blob_path_from_oci(oci, &digest), bytes).expect("OCI blob");
    json!({
        "mediaType": media_type,
        "digest": digest,
        "size": bytes.len()
    })
}

fn sha256(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut hex, "{byte:02x}").expect("write digest");
    }
    format!("sha256:{hex}")
}

fn fixture_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("unsupported test architecture: {other}"),
    }
}

fn blob_path(state: &Path, digest: &str) -> PathBuf {
    blob_path_from_oci(&state.join("oci"), digest)
}

fn blob_path_from_oci(oci: &Path, digest: &str) -> PathBuf {
    oci.join("blobs/sha256")
        .join(digest.strip_prefix("sha256:").expect("sha256 digest"))
}

fn get_file(fixture: &FixtureImage, source: &str, output: &Path) -> Output {
    get_file_from_manifest(fixture, &fixture.manifest, source, output)
}

fn get_file_from_manifest(
    fixture: &FixtureImage,
    manifest: &str,
    source: &str,
    output: &Path,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args([
            "--state",
            fixture.state.to_str().expect("state path"),
            "image",
            "file",
            "get",
            manifest,
            source,
            "--output",
            output.to_str().expect("output path"),
        ])
        .env("PATH", &fixture.fake_bin)
        .env("RUNLAB_FAKE_DOCKER_SENTINEL", &fixture.docker_sentinel)
        .output()
        .expect("runlab process")
}

fn run_fixture(fixture: &FixtureImage, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(arguments)
        .env("PATH", &fixture.fake_bin)
        .env("RUNLAB_FAKE_DOCKER_SENTINEL", &fixture.docker_sentinel)
        .output()
        .expect("runlab process")
}

fn rewrite_manifest(fixture: &FixtureImage, mutate: impl FnOnce(&mut Value, &Path)) -> String {
    let oci = fixture.state.join("oci");
    let mut manifest: Value = serde_json::from_slice(
        &fs::read(blob_path_from_oci(&oci, &fixture.manifest)).expect("Manifest bytes"),
    )
    .expect("Manifest JSON");
    mutate(&mut manifest, &oci);
    write_blob(
        &oci,
        &serde_json::to_vec(&manifest).expect("Manifest JSON"),
        OCI_MANIFEST,
    )["digest"]
        .as_str()
        .expect("Manifest digest")
        .to_owned()
}

fn assert_failed_without_output(output: &Output, path: &Path, expected_error: &str) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!path.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected_error),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_successful_file(output: &Output, path: &Path, expected: &[u8]) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.split(|byte| *byte == b'\n').count(), 2);
    let value: Value = serde_json::from_slice(&output.stdout).expect("compact JSON");
    assert_eq!(fs::read(path).expect("output bytes"), expected);
    assert_eq!(value["digest"], sha256(expected));
    assert_eq!(value["size"], expected.len());
}
