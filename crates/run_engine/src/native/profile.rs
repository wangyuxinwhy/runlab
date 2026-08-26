use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use run_protocol::{EngineError, InputPath, ProgramId, ProgramInput};

use super::container_path::safe_container_path;
use crate::oci::VerifiedImage;

pub(super) fn validate_runtime(id: &ProgramId, program: &ProgramInput) -> Result<(), EngineError> {
    let base = program_path(id).child("runtime_config");
    let value = program.runtime_config().as_json();
    if value
        .pointer("/root/path")
        .and_then(serde_json::Value::as_str)
        != Some("rootfs")
    {
        return Err(EngineError::unsupported(
            base.clone().child("root").child("path"),
            "NativeEngine materializes each private rootfs at the exact bundle path rootfs",
        ));
    }
    if value
        .pointer("/process/terminal")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(EngineError::unsupported(
            base.clone().child("process").child("terminal"),
            "NativeEngine implements independent byte streams and does not allocate a terminal",
        ));
    }
    if value.pointer("/linux/cgroupsPath").is_some() {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("cgroupsPath"),
            "NativeEngine requires its unique runtime id to select an Engine-owned cgroup; caller-selected cgroupsPath has external or concurrent ownership",
        ));
    }
    validate_isolated_host_boundaries(&base, value)?;
    let namespaces = value
        .pointer("/linux/namespaces")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            EngineError::unsupported(
                base.clone().child("linux").child("namespaces"),
                "isolated execution requires an explicit new network namespace",
            )
        })?;
    let required_namespaces = ["cgroup", "ipc", "mount", "network", "pid", "uts"];
    let mut observed_namespaces = BTreeSet::new();
    for (index, namespace) in namespaces.iter().enumerate() {
        if namespace.get("path").is_some_and(|path| !path.is_null()) {
            return Err(EngineError::unsupported(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("path"),
                "isolated NativeEngine execution requires newly created namespaces, not existing host namespaces",
            ));
        }
        let namespace_type = namespace
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !required_namespaces.contains(&namespace_type) {
            return Err(EngineError::unsupported(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("type"),
                "isolated NativeEngine execution supports only new pid, network, ipc, uts, mount, and cgroup namespaces",
            ));
        }
        if !observed_namespaces.insert(namespace_type) {
            return Err(EngineError::invalid(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("type"),
                "namespace type is duplicated",
            ));
        }
    }
    if !required_namespaces
        .iter()
        .all(|namespace| observed_namespaces.contains(namespace))
    {
        return Err(EngineError::unsupported(
            base.child("linux").child("namespaces"),
            "isolated execution requires new pid, network, ipc, uts, mount, and cgroup namespaces",
        ));
    }
    Ok(())
}

fn validate_isolated_host_boundaries(
    base: &InputPath,
    value: &serde_json::Value,
) -> Result<(), EngineError> {
    if value.pointer("/process/noNewPrivileges") != Some(&serde_json::Value::Bool(true)) {
        return Err(EngineError::unsupported(
            base.clone().child("process").child("noNewPrivileges"),
            "isolated rootful execution requires noNewPrivileges=true",
        ));
    }
    if value
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .any(|hooks| hooks.as_array().is_none_or(|hooks| !hooks.is_empty()))
        })
    {
        return Err(EngineError::unsupported(
            base.clone().child("hooks"),
            "isolated NativeEngine execution does not permit caller-controlled host hooks",
        ));
    }
    validate_isolated_mounts(base, value)?;
    let capabilities = value
        .pointer("/process/capabilities")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            EngineError::unsupported(
                base.clone().child("process").child("capabilities"),
                "isolated rootful execution requires all five capability sets to be explicitly empty",
            )
        })?;
    for set in [
        "bounding",
        "effective",
        "inheritable",
        "permitted",
        "ambient",
    ] {
        if capabilities
            .get(set)
            .and_then(serde_json::Value::as_array)
            .is_none_or(|entries| !entries.is_empty())
        {
            return Err(EngineError::unsupported(
                base.clone()
                    .child("process")
                    .child("capabilities")
                    .child(set),
                "isolated rootful execution requires this capability set to be explicitly empty",
            ));
        }
    }
    if value
        .pointer("/linux/devices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|devices| !devices.is_empty())
    {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("devices"),
            "isolated NativeEngine execution does not permit explicit host devices",
        ));
    }
    if value.pointer("/linux/seccomp/listenerPath").is_some() {
        return Err(EngineError::unsupported(
            base.clone()
                .child("linux")
                .child("seccomp")
                .child("listenerPath"),
            "isolated NativeEngine execution does not permit a host seccomp listener path",
        ));
    }
    if value.pointer("/linux/rootfsPropagation").is_some() {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("rootfsPropagation"),
            "isolated NativeEngine execution does not permit caller-selected rootfs propagation",
        ));
    }
    for field in ["uidMappings", "gidMappings", "sysctl", "intelRdt"] {
        if value.pointer(&format!("/linux/{field}")).is_some() {
            return Err(EngineError::unsupported(
                base.clone().child("linux").child(field),
                "isolated NativeEngine does not implement this additional host-kernel boundary",
            ));
        }
    }
    Ok(())
}

fn validate_isolated_mounts(
    base: &InputPath,
    value: &serde_json::Value,
) -> Result<(), EngineError> {
    for (index, mount) in value
        .get("mounts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mount_type = mount
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let bind = mount_type == "bind"
            || mount
                .get("options")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|options| {
                    options
                        .iter()
                        .any(|option| matches!(option.as_str(), Some("bind" | "rbind")))
                });
        if bind {
            return Err(EngineError::unsupported(
                base.clone().child("mounts").index(index),
                "isolated NativeEngine execution does not permit bind mounts across the host boundary",
            ));
        }
        if !matches!(mount_type, "proc" | "tmpfs" | "sysfs") {
            return Err(EngineError::unsupported(
                base.clone().child("mounts").index(index).child("type"),
                "isolated NativeEngine execution supports only proc, tmpfs, and sysfs mounts",
            ));
        }
        if mount_type == "sysfs"
            && !mount
                .get("options")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|options| options.iter().any(|option| option.as_str() == Some("ro")))
        {
            return Err(EngineError::unsupported(
                base.clone().child("mounts").index(index).child("options"),
                "isolated NativeEngine sysfs mounts must be read-only",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_host_resources(
    id: &ProgramId,
    program: &ProgramInput,
) -> Result<(), EngineError> {
    let base = program_path(id).child("runtime_config");
    let value = program.runtime_config().as_json();
    if let Some(mounts) = value.get("mounts").and_then(serde_json::Value::as_array) {
        for (index, mount) in mounts.iter().enumerate() {
            let destination_path = base
                .clone()
                .child("mounts")
                .index(index)
                .child("destination");
            let destination = mount
                .get("destination")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    EngineError::invalid(destination_path.clone(), "mount destination is required")
                })?;
            safe_container_path(destination).map_err(|error| {
                EngineError::invalid(
                    destination_path,
                    format!("invalid mount destination: {error:#}"),
                )
            })?;
            let bind = mount.get("type").and_then(serde_json::Value::as_str) == Some("bind")
                || mount
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|options| {
                        options
                            .iter()
                            .any(|option| matches!(option.as_str(), Some("bind" | "rbind")))
                    });
            if bind {
                let path = base.clone().child("mounts").index(index).child("source");
                let source = mount
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        EngineError::invalid(path.clone(), "bind mount source is required")
                    })?;
                validate_host_path(source, path)?;
            }
        }
    }
    if let Some(namespaces) = value
        .pointer("/linux/namespaces")
        .and_then(serde_json::Value::as_array)
    {
        for (index, namespace) in namespaces.iter().enumerate() {
            if let Some(path) = namespace.get("path").and_then(serde_json::Value::as_str) {
                validate_host_path(
                    path,
                    base.clone()
                        .child("linux")
                        .child("namespaces")
                        .index(index)
                        .child("path"),
                )?;
            }
        }
    }
    for phase in [
        "prestart",
        "createRuntime",
        "createContainer",
        "startContainer",
        "poststart",
        "poststop",
    ] {
        if let Some(hooks) = value
            .pointer(&format!("/hooks/{phase}"))
            .and_then(serde_json::Value::as_array)
        {
            for (index, hook) in hooks.iter().enumerate() {
                let path = base
                    .clone()
                    .child("hooks")
                    .child(phase)
                    .index(index)
                    .child("path");
                let executable = hook
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| EngineError::invalid(path.clone(), "hook path is required"))?;
                validate_hook_path(executable, path)?;
            }
        }
    }
    Ok(())
}

fn validate_host_path(raw: &str, path: InputPath) -> Result<(), EngineError> {
    let value = Path::new(raw);
    if !value.is_absolute() {
        return Err(EngineError::invalid(
            path,
            "explicit host resource path must be absolute",
        ));
    }
    fs::symlink_metadata(value).map_err(|error| {
        EngineError::input_unavailable(path, format!("cannot inspect host resource {raw}: {error}"))
    })?;
    Ok(())
}

fn validate_hook_path(raw: &str, path: InputPath) -> Result<(), EngineError> {
    validate_host_path(raw, path.clone())?;
    let metadata = fs::metadata(raw).map_err(|error| {
        EngineError::input_unavailable(
            path.clone(),
            format!("cannot inspect hook executable {raw}: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(EngineError::input_unavailable(
            path,
            format!("hook path {raw} is not an executable regular file"),
        ));
    }
    Ok(())
}

pub(super) fn validate_platform(id: &ProgramId, image: &VerifiedImage) -> Result<(), EngineError> {
    let platform_path = program_path(id)
        .child("initial_environment")
        .child("platform");
    let os = image.platform().os().to_string();
    if os != "linux" {
        return Err(EngineError::unsupported(
            platform_path.clone().child("os"),
            format!("image operating system {os} cannot execute on the Linux NativeEngine"),
        ));
    }
    let actual = image.platform().architecture().to_string();
    let expected = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    if actual != expected {
        return Err(EngineError::unsupported(
            platform_path.clone().child("architecture"),
            format!("image architecture {actual} cannot execute on host architecture {expected}"),
        ));
    }
    if let Some(variant) = image.platform().variant()
        && !(expected == "arm64" && variant == "v8")
    {
        return Err(EngineError::unsupported(
            platform_path.clone().child("variant"),
            format!(
                "NativeEngine cannot prove image CPU variant {variant}; the aarch64 build target proves only the arm64/v8 baseline"
            ),
        ));
    }
    let config: serde_json::Value = serde_json::from_slice(image.config().bytes())
        .expect("VerifiedImage retains already validated config JSON");
    for (field, reason) in [
        (
            "os.version",
            "NativeEngine has not proved an image OS-version contract against the host kernel",
        ),
        (
            "os.features",
            "NativeEngine has not proved image-required OS features against the host",
        ),
        (
            "features",
            "NativeEngine does not implement reserved OCI platform features",
        ),
    ] {
        if config.get(field).is_some() {
            return Err(EngineError::unsupported(
                platform_path.clone().child(field),
                reason,
            ));
        }
    }
    Ok(())
}

fn program_path(id: &ProgramId) -> InputPath {
    InputPath::field("programs").key(id.as_str())
}
