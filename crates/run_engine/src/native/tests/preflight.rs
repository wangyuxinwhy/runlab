use std::collections::BTreeMap;
use std::num::NonZeroU64;

use run_protocol::{
    Network, ProgramId, ProgramInput, RunControls, RunInput, RuntimeConfig, SecretValue, Secrets,
};
use serde_json::json;

use super::fixtures::*;
use crate::native::prepare::{MAX_EXECUTION_TIMEOUT, MAX_PROGRAMS};
use crate::native::profile::{
    validate_host_resources, validate_platform, validate_runtime, validate_secrets,
};
use crate::oci::inspect_image;
use crate::{CancellationToken, RunEngine};

#[test]
fn capability_limits_fail_before_host_or_content_probe() {
    let engine = test_engine();
    let programs = (0..=MAX_PROGRAMS)
        .map(|index| (ProgramId::new(format!("p{index}")), test_program()))
        .chain(std::iter::once((ProgramId::primary(), test_program())))
        .collect();
    let input =
        RunInput::new(programs, RunControls::new(None, Network::Isolated, true)).expect("input");
    let error = engine
        .run(input, CancellationToken::new())
        .expect_err("Program cap");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs")
    );

    let input = RunInput::new(
        BTreeMap::from([(ProgramId::primary(), test_program())]),
        RunControls::new(
            NonZeroU64::new(
                u64::try_from(MAX_EXECUTION_TIMEOUT.as_millis()).expect("milliseconds") + 1,
            ),
            Network::Isolated,
            true,
        ),
    )
    .expect("input");
    let error = engine
        .run(input, CancellationToken::new())
        .expect_err("timeout cap");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("controls.execution_timeout_ms")
    );
}

#[test]
fn isolated_profile_requires_one_new_network_namespace() {
    let id = ProgramId::primary();
    validate_runtime(&id, &test_program(), Network::Isolated)
        .expect("new private network namespace");
    let runtime = RuntimeConfig::parse(
            br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"terminal":false,"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0},"noNewPrivileges":true,"capabilities":{"bounding":[],"effective":[],"inheritable":[],"permitted":[],"ambient":[]}},"linux":{"namespaces":[{"type":"pid"},{"type":"network","path":"/proc/1/ns/net"},{"type":"ipc"},{"type":"uts"},{"type":"mount"},{"type":"cgroup"}]}}"#.to_vec(),
        )
        .expect("runtime");
    let program =
        ProgramInput::new(test_image(), runtime, Vec::new(), Secrets::empty()).expect("program");
    let error = validate_runtime(&id, &program, Network::Isolated).expect_err("existing namespace");
    assert!(
        error
            .path()
            .expect("path")
            .to_string()
            .ends_with("linux.namespaces[1].path"),
        "{error:?}"
    );
}

#[test]
fn native_profile_requires_one_new_mount_namespace() {
    let id = ProgramId::primary();
    let mut missing = test_program().runtime_config().as_json().clone();
    missing
        .pointer_mut("/linux/namespaces")
        .and_then(serde_json::Value::as_array_mut)
        .expect("namespaces")
        .retain(|namespace| {
            namespace.get("type").and_then(serde_json::Value::as_str) != Some("mount")
        });
    let program = program_with_runtime(&missing);
    let error =
        validate_runtime(&id, &program, Network::Isolated).expect_err("missing mount namespace");
    assert!(
        error
            .path()
            .expect("path")
            .to_string()
            .ends_with("linux.namespaces"),
        "{error}"
    );

    let mut existing = test_program().runtime_config().as_json().clone();
    existing
        .pointer_mut("/linux/namespaces/4")
        .and_then(serde_json::Value::as_object_mut)
        .expect("mount namespace")
        .insert("path".to_owned(), json!("/proc/1/ns/mnt"));
    let program = program_with_runtime(&existing);
    let error =
        validate_runtime(&id, &program, Network::Isolated).expect_err("existing mount namespace");
    assert!(
        error
            .path()
            .expect("path")
            .to_string()
            .ends_with("linux.namespaces[4].path"),
        "{error}"
    );

    let mut duplicate = test_program().runtime_config().as_json().clone();
    duplicate
        .pointer_mut("/linux/namespaces")
        .and_then(serde_json::Value::as_array_mut)
        .expect("namespaces")
        .push(json!({"type": "mount"}));
    let program = program_with_runtime(&duplicate);
    let error =
        validate_runtime(&id, &program, Network::Isolated).expect_err("duplicate mount namespace");
    assert!(
        error
            .path()
            .expect("path")
            .to_string()
            .ends_with("linux.namespaces[6].type"),
        "{error}"
    );
}

#[test]
fn native_profile_rejects_shared_rootfs_propagation() {
    let id = ProgramId::primary();
    for propagation in ["shared", "rshared", "unbindable", "runbindable"] {
        let mut value = test_program().runtime_config().as_json().clone();
        value
            .pointer_mut("/linux")
            .and_then(serde_json::Value::as_object_mut)
            .expect("linux")
            .insert("rootfsPropagation".to_owned(), json!(propagation));
        let program = program_with_runtime(&value);
        let error = validate_runtime(&id, &program, Network::Isolated)
            .expect_err("outgoing mount propagation");
        assert!(
            error
                .path()
                .expect("path")
                .to_string()
                .ends_with("linux.rootfsPropagation"),
            "{propagation}: {error}"
        );
    }

    for propagation in ["private", "rprivate", "slave", "rslave"] {
        let mut value = test_program().runtime_config().as_json().clone();
        value
            .pointer_mut("/linux")
            .and_then(serde_json::Value::as_object_mut)
            .expect("linux")
            .insert("rootfsPropagation".to_owned(), json!(propagation));
        validate_runtime(&id, &program_with_runtime(&value), Network::Isolated)
            .expect("non-shared rootfs propagation");
    }
}

#[test]
fn native_profile_delegates_oci_runtime_semantics_to_runc() {
    let mut value = test_program().runtime_config().as_json().clone();
    value
        .pointer_mut("/process")
        .and_then(serde_json::Value::as_object_mut)
        .expect("process")
        .insert("noNewPrivileges".to_owned(), json!(false));
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime config bytes"))
        .expect("structurally valid runtime config");
    let program =
        ProgramInput::new(test_image(), runtime, Vec::new(), Secrets::empty()).expect("program");

    validate_runtime(&ProgramId::primary(), &program, Network::Isolated)
        .expect("runc, not NativeEngine, owns these OCI field semantics");
}

#[test]
fn host_hooks_are_rejected_without_a_containment_model() {
    let mut value = test_program().runtime_config().as_json().clone();
    value.as_object_mut().expect("runtime object").insert(
        "hooks".to_owned(),
        json!({"prestart": [{"path": "/bin/true"}]}),
    );
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime config bytes"))
        .expect("structurally valid runtime config");
    let program =
        ProgramInput::new(test_image(), runtime, Vec::new(), Secrets::empty()).expect("program");

    let error = validate_runtime(&ProgramId::primary(), &program, Network::Isolated)
        .expect_err("host hooks");
    assert!(
        error
            .path()
            .expect("unsupported path")
            .to_string()
            .ends_with("runtime_config.hooks"),
        "{error}"
    );
}

#[test]
fn missing_bind_source_is_rejected_before_execution() {
    let workspace = tempfile::tempdir().expect("host resource fixture");
    let source = workspace.path().join("missing");
    let mut value = test_program().runtime_config().as_json().clone();
    value.as_object_mut().expect("runtime object").insert(
        "mounts".to_owned(),
        json!([{
            "destination": "/input",
            "source": source,
            "type": "bind",
            "options": ["bind"]
        }]),
    );
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime config bytes"))
        .expect("structurally valid runtime config");
    let program =
        ProgramInput::new(test_image(), runtime, Vec::new(), Secrets::empty()).expect("program");

    let error =
        validate_host_resources(&ProgramId::primary(), &program).expect_err("missing bind source");
    assert!(
        error
            .path()
            .expect("input path")
            .to_string()
            .ends_with("mounts[0].source"),
        "{error}"
    );
}

#[test]
fn secrets_cannot_ambiguously_replace_runtime_environment_or_mounts() {
    let mut value = test_program().runtime_config().as_json().clone();
    value["process"]["env"] = json!(["TOKEN=runtime"]);
    value.as_object_mut().expect("runtime object").insert(
        "mounts".to_owned(),
        json!([{"destination": "/run/credential", "type": "tmpfs", "source": "tmpfs"}]),
    );
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime config bytes"))
        .expect("structurally valid runtime config");

    let environment_conflict = ProgramInput::new(
        test_image(),
        runtime.clone(),
        Vec::new(),
        Secrets::new(
            BTreeMap::from([("TOKEN".to_owned(), SecretValue::new(b"secret".to_vec()))]),
            BTreeMap::new(),
        )
        .expect("Secrets"),
    )
    .expect("program");
    let error = validate_secrets(&ProgramId::primary(), &environment_conflict)
        .expect_err("environment collision");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs[\"primary\"].secrets.env[\"TOKEN\"]")
    );

    let file_conflict = ProgramInput::new(
        test_image(),
        runtime,
        Vec::new(),
        Secrets::new(
            BTreeMap::new(),
            BTreeMap::from([(
                "/run/credential".to_owned(),
                SecretValue::new(b"secret".to_vec()),
            )]),
        )
        .expect("Secrets"),
    )
    .expect("program");
    let error =
        validate_secrets(&ProgramId::primary(), &file_conflict).expect_err("mount collision");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs[\"primary\"].secrets.files[\"/run/credential\"]")
    );
}

#[test]
fn caller_selected_cgroup_is_rejected_at_the_owned_boundary() {
    let mut value = test_program().runtime_config().as_json().clone();
    value
        .pointer_mut("/linux")
        .and_then(serde_json::Value::as_object_mut)
        .expect("linux")
        .insert("cgroupsPath".to_owned(), json!("/shared"));
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime config bytes"))
        .expect("structurally valid runtime config");
    let program =
        ProgramInput::new(test_image(), runtime, Vec::new(), Secrets::empty()).expect("program");

    let error = validate_runtime(&ProgramId::primary(), &program, Network::Isolated)
        .expect_err("owned cgroup");
    assert!(
        error
            .path()
            .expect("unsupported path")
            .to_string()
            .ends_with("linux.cgroupsPath"),
        "{error}"
    );
}

#[test]
fn image_platform_requirements_are_rejected_at_exact_paths() {
    for (field, value, suffix) in [
        ("variant", json!("v9"), "platform.variant"),
        ("os.version", json!("test-kernel"), "platform.os.version"),
        (
            "os.features",
            json!(["test-feature"]),
            "platform.os.features",
        ),
    ] {
        let store = MemoryStore::default();
        let descriptor = image_with_platform_field(&store, field, value);
        let image = inspect_image(&store, &descriptor).expect("verified image");
        let Err(error) = validate_platform(&ProgramId::primary(), &image) else {
            panic!("unproved platform requirement {field}");
        };
        assert!(
            error
                .path()
                .expect("platform path")
                .to_string()
                .ends_with(suffix),
            "{field}: {error}"
        );
    }
    if std::env::consts::ARCH == "aarch64" {
        let store = MemoryStore::default();
        let descriptor = image_with_platform_field(&store, "variant", json!("v8"));
        let image = inspect_image(&store, &descriptor).expect("verified arm64/v8 image");
        validate_platform(&ProgramId::primary(), &image)
            .expect("aarch64 execution proves the OCI arm64/v8 baseline");
    }
}

fn program_with_runtime(value: &serde_json::Value) -> ProgramInput {
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime config bytes"))
        .expect("structurally valid runtime config");
    ProgramInput::new(test_image(), runtime, Vec::new(), Secrets::empty()).expect("program")
}
