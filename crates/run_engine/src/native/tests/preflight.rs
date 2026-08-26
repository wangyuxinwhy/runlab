use std::collections::BTreeMap;
use std::num::NonZeroU64;

use run_protocol::{Network, ProgramId, ProgramInput, RunInput, RuntimeConfig};
use serde_json::json;

use super::fixtures::*;
use crate::native::prepare::{MAX_EXECUTION_TIMEOUT, MAX_PROGRAMS};
use crate::native::profile::{validate_platform, validate_runtime};
use crate::oci::inspect_image;
use crate::{CancellationToken, RunEngine};

#[test]
fn capability_limits_fail_before_host_or_content_probe() {
    let engine = test_engine();
    let programs = (0..=MAX_PROGRAMS)
        .map(|index| (ProgramId::new(format!("p{index}")), test_program()))
        .chain(std::iter::once((ProgramId::primary(), test_program())))
        .collect();
    let input = RunInput::new(programs, None, Network::Isolated).expect("input");
    let error = engine
        .run(input, CancellationToken::new())
        .expect_err("Program cap");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs")
    );

    let input = RunInput::new(
        BTreeMap::from([(ProgramId::primary(), test_program())]),
        NonZeroU64::new(
            u64::try_from(MAX_EXECUTION_TIMEOUT.as_millis()).expect("milliseconds") + 1,
        ),
        Network::Isolated,
    )
    .expect("input");
    let error = engine
        .run(input, CancellationToken::new())
        .expect_err("timeout cap");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("execution_timeout_ms")
    );
}

#[test]
fn later_invalid_program_fails_before_any_host_or_content_probe() {
    let mut invalid_value = test_program().runtime_config().as_json().clone();
    invalid_value
        .as_object_mut()
        .expect("runtime object")
        .insert(
            "hooks".to_owned(),
            json!({"prestart": [{"path": "/bin/true"}]}),
        );
    let invalid_runtime =
        RuntimeConfig::parse(serde_json::to_vec(&invalid_value).expect("runtime config bytes"))
            .expect("structurally valid runtime config");
    let invalid = ProgramInput::new(test_image(), invalid_runtime, Vec::new()).expect("program");
    let input = RunInput::new(
        BTreeMap::from([
            (ProgramId::new("dependency-a"), test_program()),
            (ProgramId::new("dependency-z"), invalid),
            (ProgramId::primary(), test_program()),
        ]),
        None,
        Network::Isolated,
    )
    .expect("input");

    let error = test_engine()
        .run(input, CancellationToken::new())
        .expect_err("later unsupported Program");
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs[\"dependency-z\"].runtime_config.hooks")
    );
}

#[test]
fn isolated_profile_requires_one_new_network_namespace() {
    let id = ProgramId::primary();
    validate_runtime(&id, &test_program()).expect("new private network namespace");
    let runtime = RuntimeConfig::parse(
            br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"terminal":false,"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0},"noNewPrivileges":true,"capabilities":{"bounding":[],"effective":[],"inheritable":[],"permitted":[],"ambient":[]}},"linux":{"namespaces":[{"type":"pid"},{"type":"network","path":"/proc/1/ns/net"},{"type":"ipc"},{"type":"uts"},{"type":"mount"},{"type":"cgroup"}]}}"#.to_vec(),
        )
        .expect("runtime");
    let program = ProgramInput::new(test_image(), runtime, Vec::new()).expect("program");
    let error = validate_runtime(&id, &program).expect_err("existing namespace");
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
fn isolated_profile_rejects_cross_boundary_runtime_features_at_exact_paths() {
    let cases = [
        (
            "hooks",
            json!({"hooks": {"prestart": [{"path": "/bin/true"}]}}),
            "hooks",
        ),
        (
            "bind",
            json!({"mounts": [{"destination": "/host", "type": "bind", "source": "/"}]}),
            "mounts[0]",
        ),
        (
            "namespace",
            json!({"linux": {"namespaces": [{"type": "network"}, {"type": "pid", "path": "/proc/1/ns/pid"}]}}),
            "linux.namespaces[1].path",
        ),
        (
            "capability",
            json!({"process": {"capabilities": {
                "bounding": [], "effective": ["CAP_NET_ADMIN"], "inheritable": [],
                "permitted": [], "ambient": []
            }}}),
            "process.capabilities.effective",
        ),
        (
            "privilege gain",
            json!({"process": {"noNewPrivileges": false}}),
            "process.noNewPrivileges",
        ),
        (
            "device",
            json!({"linux": {"devices": [{"path": "/dev/kmsg", "type": "c", "major": 1, "minor": 11}], "namespaces": [{"type": "network"}]}}),
            "linux.devices",
        ),
        (
            "seccomp listener",
            json!({"linux": {"seccomp": {"defaultAction": "SCMP_ACT_ALLOW", "listenerPath": "/run/notify.sock"}, "namespaces": [{"type": "network"}]}}),
            "linux.seccomp.listenerPath",
        ),
        (
            "rootfs propagation",
            json!({"linux": {"rootfsPropagation": "shared", "namespaces": [{"type": "network"}]}}),
            "linux.rootfsPropagation",
        ),
        (
            "caller cgroup",
            json!({"linux": {"cgroupsPath": "/shared", "namespaces": [{"type": "network"}]}}),
            "linux.cgroupsPath",
        ),
    ];
    for (label, addition, suffix) in cases {
        let mut value = json!({
            "ociVersion": "1.3.0",
            "root": {"path": "rootfs"},
            "process": {
                "terminal": false,
                "args": ["/bin/true"],
                "cwd": "/",
                "user": {"uid": 0, "gid": 0},
                "noNewPrivileges": true,
                "capabilities": {
                    "bounding": [], "effective": [], "inheritable": [],
                    "permitted": [], "ambient": []
                }
            },
            "linux": {"namespaces": [
                {"type": "pid"}, {"type": "network"}, {"type": "ipc"},
                {"type": "uts"}, {"type": "mount"}, {"type": "cgroup"}
            ]}
        });
        merge_json(&mut value, &addition);
        let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime JSON"))
            .expect("runtime config");
        let program = ProgramInput::new(test_image(), runtime, Vec::new()).expect("program");
        let error = validate_runtime(&ProgramId::primary(), &program)
            .expect_err("cross-boundary feature unexpectedly accepted");
        assert!(
            error
                .path()
                .expect("unsupported path")
                .to_string()
                .ends_with(suffix),
            "{label}: {error}"
        );
    }
}

fn merge_json(target: &mut serde_json::Value, addition: &serde_json::Value) {
    let target = target.as_object_mut().expect("target object");
    for (key, value) in addition.as_object().expect("addition object") {
        if let (Some(existing), Some(fields)) = (target.get_mut(key), value.as_object())
            && let Some(existing) = existing.as_object_mut()
        {
            for (field, value) in fields {
                existing.insert(field.clone(), value.clone());
            }
            continue;
        }
        target.insert(key.clone(), value.clone());
    }
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
