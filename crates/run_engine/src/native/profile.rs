use std::fs;
use std::path::Path;

use run_protocol::{EngineError, InputPath, ProgramId, ProgramInput};

use super::container_path::safe_container_path;
use crate::oci::VerifiedImage;

pub(super) fn validate_runtime(id: &ProgramId, program: &ProgramInput) -> Result<(), EngineError> {
    let base = program_path(id).child("runtime_config");
    let value = program.runtime_config().as_json();
    if value.pointer("/linux/cgroupsPath").is_some() {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("cgroupsPath"),
            "NativeEngine requires its unique runtime id to select an Engine-owned cgroup; caller-selected cgroupsPath has external or concurrent ownership",
        ));
    }
    if value
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .any(|value| value.as_array().is_none_or(|hooks| !hooks.is_empty()))
        })
    {
        return Err(EngineError::unsupported(
            base.clone().child("hooks"),
            "NativeEngine does not execute host hooks without a containment model for processes they may leave behind",
        ));
    }
    let namespaces = value
        .pointer("/linux/namespaces")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            EngineError::unsupported(
                base.clone().child("linux").child("namespaces"),
                "Network::Isolated requires an explicit new network namespace",
            )
        })?;
    require_new_namespace(
        &base,
        namespaces,
        "network",
        "Network::Isolated requires a new network namespace",
        "Network::Isolated cannot join an existing network namespace",
    )?;
    require_new_namespace(
        &base,
        namespaces,
        "mount",
        "NativeEngine requires a new mount namespace for its private rootfs",
        "NativeEngine cannot place its private rootfs in an existing mount namespace",
    )?;
    if let Some(propagation) = value.pointer("/linux/rootfsPropagation")
        && !matches!(
            propagation.as_str(),
            Some("private" | "rprivate" | "slave" | "rslave")
        )
    {
        return Err(EngineError::unsupported(
            base.child("linux").child("rootfsPropagation"),
            "NativeEngine requires rootfs propagation that cannot propagate mounts back to the host",
        ));
    }
    Ok(())
}

fn require_new_namespace(
    base: &InputPath,
    namespaces: &[serde_json::Value],
    kind: &str,
    missing: &str,
    existing: &str,
) -> Result<(), EngineError> {
    let mut found = None;
    for (index, namespace) in namespaces.iter().enumerate() {
        if namespace.get("type").and_then(serde_json::Value::as_str) != Some(kind) {
            continue;
        }
        if found.replace(index).is_some() {
            return Err(EngineError::invalid(
                base.clone()
                    .child("linux")
                    .child("namespaces")
                    .index(index)
                    .child("type"),
                format!("{kind} namespace is duplicated"),
            ));
        }
    }
    let Some(index) = found else {
        return Err(EngineError::unsupported(
            base.clone().child("linux").child("namespaces"),
            missing,
        ));
    };
    if namespaces[index]
        .get("path")
        .is_some_and(|path| !path.is_null())
    {
        return Err(EngineError::unsupported(
            base.clone()
                .child("linux")
                .child("namespaces")
                .index(index)
                .child("path"),
            existing,
        ));
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
            if namespace.get("type").and_then(serde_json::Value::as_str) == Some("network") {
                continue;
            }
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
    Ok(())
}

fn validate_host_path(raw: &str, path: InputPath) -> Result<(), EngineError> {
    if !Path::new(raw).is_absolute() {
        return Err(EngineError::invalid(
            path,
            "explicit host resource path must be absolute",
        ));
    }
    fs::metadata(raw).map_err(|error| {
        EngineError::input_unavailable(path.clone(), format!("cannot inspect {raw}: {error}"))
    })?;
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
