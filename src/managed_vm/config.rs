use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(super) const SHARES_ROOT: &str = "/mnt/runlab-shares";
pub(super) const SHARES_FINGERPRINT_ENV: &str = "RUNLAB_SHARES_FINGERPRINT";
const MAX_SHARES: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmShareDocument {
    pub(crate) schema_version: u32,
    pub(crate) shares: Vec<VmShare>,
}

impl Default for VmShareDocument {
    fn default() -> Self {
        Self {
            schema_version: 1,
            shares: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmShare {
    pub(crate) name: String,
    pub(crate) host_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ResolvedVmShare {
    pub(crate) name: String,
    pub(crate) host_path: String,
    pub(crate) guest_path: String,
    pub(crate) r#type: &'static str,
    pub(crate) read_only: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct LimaConfig {
    #[serde(default)]
    pub(super) plain: Option<bool>,
    #[serde(default, rename = "mountType")]
    pub(super) mount_type: Option<String>,
    #[serde(default, rename = "mountInotify")]
    pub(super) mount_inotify: Option<bool>,
    #[serde(default)]
    pub(super) mounts: Vec<LimaMount>,
    #[serde(default)]
    pub(super) containerd: Option<LimaContainerd>,
    #[serde(default, rename = "portForwards")]
    pub(super) port_forwards: Vec<Value>,
    #[serde(default)]
    pub(super) networks: Vec<Value>,
    #[serde(default, rename = "hostResolver")]
    pub(super) host_resolver: Option<LimaHostResolver>,
    #[serde(default, rename = "propagateProxyEnv")]
    pub(super) propagate_proxy_env: Option<bool>,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) ssh: Option<Value>,
    pub(super) images: Vec<super::host::VmImage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct LimaMount {
    #[serde(default)]
    pub(super) location: String,
    #[serde(default, rename = "mountPoint")]
    pub(super) mount_point: String,
    #[serde(default)]
    pub(super) writable: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct LimaContainerd {
    #[serde(default)]
    pub(super) system: Option<bool>,
    #[serde(default)]
    pub(super) user: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct LimaHostResolver {
    #[serde(default)]
    pub(super) enabled: Option<bool>,
    #[serde(default)]
    pub(super) ipv6: Option<bool>,
}

pub(crate) fn parse_document(bytes: &[u8]) -> Result<VmShareDocument> {
    serde_json::from_slice(bytes).context("VM share configuration is not valid JSON")
}

pub(super) fn normalize_document(
    document: VmShareDocument,
) -> Result<(VmShareDocument, Vec<String>)> {
    ensure!(
        document.schema_version == 1,
        "unsupported VM share configuration schema_version: {}",
        document.schema_version
    );
    ensure!(
        document.shares.len() <= MAX_SHARES,
        "VM share configuration exceeds {MAX_SHARES} shares"
    );
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut shares = Vec::with_capacity(document.shares.len());
    for share in document.shares {
        validate_name(&share.name)?;
        ensure!(
            names.insert(share.name.clone()),
            "VM share name is duplicated: {}",
            share.name
        );
        let requested = Path::new(&share.host_path);
        ensure!(
            requested.is_absolute(),
            "VM share host_path must be absolute: {}",
            share.host_path
        );
        let canonical = requested.canonicalize().with_context(|| {
            format!(
                "VM share host_path does not exist or cannot be resolved: {}",
                share.host_path
            )
        })?;
        ensure!(
            canonical.is_dir(),
            "VM share host_path must be a directory: {}",
            canonical.display()
        );
        ensure!(
            paths.insert(canonical.clone()),
            "VM share host_path is duplicated: {}",
            canonical.display()
        );
        if path_appears_case_insensitive(&canonical) {
            warnings.push(format!(
                "share {} is on a case-insensitive filesystem; Linux names that differ only by case cannot remain distinct",
                share.name
            ));
        }
        let host_path = canonical
            .to_str()
            .context("VM share host_path is not valid UTF-8")?
            .to_owned();
        shares.push(VmShare {
            name: share.name,
            host_path,
        });
    }
    shares.sort();
    Ok((
        VmShareDocument {
            schema_version: 1,
            shares,
        },
        warnings,
    ))
}

pub(super) fn resolved_shares(document: &VmShareDocument) -> Vec<ResolvedVmShare> {
    document
        .shares
        .iter()
        .map(|share| ResolvedVmShare {
            name: share.name.clone(),
            host_path: share.host_path.clone(),
            guest_path: guest_path(&share.name),
            r#type: "virtiofs",
            read_only: true,
        })
        .collect()
}

pub(super) fn fingerprint(document: &VmShareDocument) -> String {
    let bytes = serde_json::to_vec(document).expect("VM share document serialization cannot fail");
    let digest = Sha256::digest(bytes);
    let encoded = digest.iter().fold(String::new(), |mut value, byte| {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to a String cannot fail");
        value
    });
    format!("sha256:{encoded}")
}

pub(super) fn profile_value(document: &VmShareDocument) -> Value {
    let mounts = document
        .shares
        .iter()
        .map(|share| {
            serde_json::json!({
                "location": share.host_path,
                "mountPoint": guest_path(&share.name),
                "writable": false,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "plain": false,
        "mountType": "virtiofs",
        "mountInotify": false,
        "mounts": mounts,
        "containerd": {"system": false, "user": false},
        "portForwards": [],
        "networks": [],
        "hostResolver": {"enabled": true, "ipv6": false},
        "propagateProxyEnv": false,
        "env": {SHARES_FINGERPRINT_ENV: fingerprint(document)},
        "ssh": {
            "loadDotSSHPubKeys": false,
            "forwardAgent": false,
            "forwardX11": false,
            "forwardX11Trusted": false,
        },
    })
}

pub(super) fn edit_expression(document: &VmShareDocument) -> Result<String> {
    let profile = profile_value(document);
    let assignments = [
        "plain",
        "mountType",
        "mountInotify",
        "mounts",
        "containerd",
        "portForwards",
        "networks",
        "hostResolver",
        "propagateProxyEnv",
        "env",
        "ssh",
    ]
    .into_iter()
    .map(|field| {
        let encoded = serde_json::to_string(&profile[field])?;
        Ok(format!(".{field} = {encoded}"))
    })
    .collect::<Result<Vec<_>>>()?;
    Ok(assignments.join(" | "))
}

pub(super) fn configured_document(config: &LimaConfig) -> Result<VmShareDocument> {
    let mut shares = Vec::with_capacity(config.mounts.len());
    for mount in &config.mounts {
        ensure!(
            !mount.writable,
            "configured VM share is writable: {}",
            mount.mount_point
        );
        let name = share_name_from_guest_path(&mount.mount_point)?;
        shares.push(VmShare {
            name,
            host_path: mount.location.clone(),
        });
    }
    shares.sort();
    let document = VmShareDocument {
        schema_version: 1,
        shares,
    };
    let (normalized, _) = normalize_document(document.clone())?;
    ensure!(
        document == normalized,
        "configured VM share declaration does not use normalized canonical Host paths"
    );
    let expected = fingerprint(&normalized);
    ensure!(
        config.env.len() == 1 && config.env.get(SHARES_FINGERPRINT_ENV) == Some(&expected),
        "VM share declaration fingerprint does not match the effective mounts"
    );
    Ok(normalized)
}

pub(super) fn profile_problems(
    config: &LimaConfig,
    expected: Option<&VmShareDocument>,
) -> Vec<String> {
    let mut problems = Vec::new();
    if config.plain != Some(false) {
        problems.push("Lima plain mode must be disabled".to_owned());
    }
    if config.mount_type.as_deref() != Some("virtiofs") {
        problems.push("Lima mount type must be virtiofs".to_owned());
    }
    if config.mount_inotify != Some(false) {
        problems.push("Lima mount inotify must be disabled".to_owned());
    }
    if !matches!(
        config.containerd.as_ref(),
        Some(LimaContainerd {
            system: Some(false),
            user: Some(false),
        })
    ) {
        problems.push("Lima containerd must be disabled".to_owned());
    }
    if !config.port_forwards.is_empty() {
        problems.push("Lima port forwards must be empty".to_owned());
    }
    if !config.networks.is_empty() {
        problems.push("Lima additional networks must be empty".to_owned());
    }
    if !matches!(
        config.host_resolver.as_ref(),
        Some(LimaHostResolver {
            enabled: Some(true),
            ipv6: Some(false),
        })
    ) {
        problems.push("Lima host resolver profile does not match RunLab".to_owned());
    }
    if config.propagate_proxy_env != Some(false) {
        problems.push("Lima proxy environment propagation must be disabled".to_owned());
    }
    if [
        "loadDotSSHPubKeys",
        "forwardAgent",
        "forwardX11",
        "forwardX11Trusted",
    ]
    .into_iter()
    .any(|field| {
        config
            .ssh
            .as_ref()
            .and_then(|ssh| ssh.get(field))
            .and_then(Value::as_bool)
            != Some(false)
    }) {
        problems.push("Lima SSH forwarding and Host key loading must be disabled".to_owned());
    }
    match configured_document(config) {
        Ok(actual) if expected.is_some_and(|expected| actual != *expected) => {
            problems.push("VM shares do not match the requested configuration".to_owned());
        }
        Ok(_) => {}
        Err(error) => problems.push(error.to_string()),
    }
    problems
}

pub(super) fn validate_runtime_mounts(
    runtime_config: &Path,
    shares: &[ResolvedVmShare],
) -> Result<()> {
    let bytes = fs::read(runtime_config).with_context(|| {
        format!(
            "failed to read Runtime Configuration {}",
            runtime_config.display()
        )
    })?;
    let value: Value =
        serde_json::from_slice(&bytes).context("Runtime Configuration is not valid JSON")?;
    let Some(mounts) = value.get("mounts").and_then(Value::as_array) else {
        return Ok(());
    };
    for mount in mounts {
        if super::transport::is_managed_resolver_mount(mount) {
            continue;
        }
        if !is_bind_mount(mount) {
            continue;
        }
        let source = mount
            .get("source")
            .and_then(Value::as_str)
            .context("OCI bind mount source must be a string")?;
        ensure!(
            Path::new(source).is_absolute(),
            "OCI bind mount source must be absolute: {source}"
        );
        let options = mount
            .get("options")
            .and_then(Value::as_array)
            .context("OCI bind mount options must be an array")?;
        let read_only = options.iter().any(|value| value.as_str() == Some("ro"));
        let writable = options.iter().any(|value| value.as_str() == Some("rw"));
        ensure!(
            read_only && !writable,
            "macOS Managed VM share bind mounts must contain `ro` and must not contain `rw`: {source}"
        );
        validate_shared_source(source, shares)?;
    }
    Ok(())
}

fn validate_shared_source(source: &str, shares: &[ResolvedVmShare]) -> Result<()> {
    let source_path = Path::new(source);
    for share in shares {
        let guest_root = Path::new(&share.guest_path);
        let Ok(relative) = source_path.strip_prefix(guest_root) else {
            continue;
        };
        ensure!(
            !relative
                .components()
                .any(|component| matches!(component, Component::ParentDir)),
            "VM share bind source escapes its declared share: {source}"
        );
        let host_root = Path::new(&share.host_path)
            .canonicalize()
            .with_context(|| format!("VM share is unavailable on macOS: {}", share.name))?;
        let host_source = host_root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("VM share bind source is unavailable on macOS: {source}"))?;
        ensure!(
            host_source.starts_with(&host_root),
            "VM share bind source resolves outside its declared share: {source}"
        );
        ensure!(
            host_source.is_file() || host_source.is_dir(),
            "VM share bind source is not a regular file or directory: {source}"
        );
        return Ok(());
    }
    bail!(
        "OCI bind mount source is not inside a declared VM share: {source}; declare it with `runlab vm config apply --document FILE` or import the data as an OCI Image"
    )
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    ensure!(
        valid,
        "VM share name must be 1-63 lowercase ASCII letters, digits, or interior hyphens: {name}"
    );
    Ok(())
}

fn is_bind_mount(mount: &Value) -> bool {
    mount.get("type").and_then(Value::as_str) == Some("bind")
        || mount
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|option| matches!(option.as_str(), Some("bind" | "rbind")))
            })
}

fn guest_path(name: &str) -> String {
    format!("{SHARES_ROOT}/{name}")
}

fn share_name_from_guest_path(path: &str) -> Result<String> {
    let relative = Path::new(path)
        .strip_prefix(SHARES_ROOT)
        .with_context(|| format!("VM mount is outside the reserved share root: {path}"))?;
    let mut components = relative.components();
    let name = match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => name
            .to_str()
            .context("VM share name is not valid UTF-8")?
            .to_owned(),
        _ => bail!("VM mount point is not a direct child of {SHARES_ROOT}: {path}"),
    };
    validate_name(&name)?;
    Ok(name)
}

#[cfg(target_os = "macos")]
fn path_appears_case_insensitive(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    for ancestor in path.ancestors() {
        let Some(name) = ancestor.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let mut alternate = name.as_bytes().to_vec();
        let Some(index) = alternate.iter().position(u8::is_ascii_alphabetic) else {
            continue;
        };
        alternate[index] = if alternate[index].is_ascii_lowercase() {
            alternate[index].to_ascii_uppercase()
        } else {
            alternate[index].to_ascii_lowercase()
        };
        let Ok(alternate) = String::from_utf8(alternate) else {
            continue;
        };
        let candidate = ancestor.with_file_name(alternate);
        if let (Ok(original), Ok(candidate)) = (fs::metadata(ancestor), fs::metadata(candidate))
            && original.dev() == candidate.dev()
            && original.ino() == candidate.ino()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_round_trips_declared_shares() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (document, _) = normalize_document(VmShareDocument {
            schema_version: 1,
            shares: vec![VmShare {
                name: "trace-corpus".to_owned(),
                host_path: temporary.path().to_string_lossy().into_owned(),
            }],
        })
        .expect("document");
        let value = profile_value(&document);
        let config: LimaConfig = serde_json::from_value(serde_json::json!({
            "images": [],
            "plain": value["plain"],
            "mountType": value["mountType"],
            "mountInotify": value["mountInotify"],
            "mounts": value["mounts"],
            "containerd": value["containerd"],
            "portForwards": value["portForwards"],
            "networks": value["networks"],
            "hostResolver": value["hostResolver"],
            "propagateProxyEnv": value["propagateProxyEnv"],
            "env": value["env"],
            "ssh": value["ssh"],
        }))
        .expect("Lima config");

        assert_eq!(configured_document(&config).expect("configured"), document);
        assert!(profile_problems(&config, Some(&document)).is_empty());
    }

    #[test]
    fn profile_rejects_a_mount_without_matching_declaration_fingerprint() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let host_path = temporary.path().canonicalize().expect("canonical path");
        let config: LimaConfig = serde_json::from_value(serde_json::json!({
            "images": [],
            "plain": false,
            "mountType": "virtiofs",
            "mounts": [{
                "location": host_path,
                "mountPoint": "/mnt/runlab-shares/example",
                "writable": false
            }],
            "hostResolver": {"enabled": true, "ipv6": false}
        }))
        .expect("Lima config");

        assert!(
            profile_problems(&config, None)
                .iter()
                .any(|problem| { problem.contains("fingerprint") })
        );
    }

    #[test]
    fn profile_rejects_an_unresolved_effective_host_path_even_with_a_matching_fingerprint() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let missing = temporary.path().join("missing");
        let document = VmShareDocument {
            schema_version: 1,
            shares: vec![VmShare {
                name: "example".to_owned(),
                host_path: missing.to_string_lossy().into_owned(),
            }],
        };
        let config: LimaConfig = serde_json::from_value(serde_json::json!({
            "images": [],
            "plain": false,
            "mountType": "virtiofs",
            "mountInotify": false,
            "mounts": [{
                "location": missing,
                "mountPoint": "/mnt/runlab-shares/example",
                "writable": false
            }],
            "containerd": {"system": false, "user": false},
            "portForwards": [],
            "networks": [],
            "hostResolver": {"enabled": true, "ipv6": false},
            "propagateProxyEnv": false,
            "env": {SHARES_FINGERPRINT_ENV: fingerprint(&document)},
            "ssh": {
                "loadDotSSHPubKeys": false,
                "forwardAgent": false,
                "forwardX11": false,
                "forwardX11Trusted": false
            }
        }))
        .expect("Lima config");

        assert!(
            profile_problems(&config, None)
                .iter()
                .any(|problem| problem.contains("does not exist or cannot be resolved"))
        );
    }

    #[test]
    fn profile_rejects_missing_explicit_capability_pins() {
        let document = VmShareDocument::default();
        let config: LimaConfig = serde_json::from_value(serde_json::json!({
            "images": [],
            "mountType": "virtiofs",
            "mounts": [],
            "env": {SHARES_FINGERPRINT_ENV: fingerprint(&document)}
        }))
        .expect("Lima config");

        let problems = profile_problems(&config, Some(&document));
        for expected in ["plain", "inotify", "containerd", "resolver", "proxy", "SSH"] {
            assert!(
                problems.iter().any(|problem| problem.contains(expected)),
                "missing {expected} problem: {problems:?}"
            );
        }
    }

    #[test]
    fn runtime_mounts_must_resolve_inside_a_share() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        fs::write(temporary.path().join("data.db"), b"sqlite").expect("fixture");
        let runtime = temporary.path().join("config.json");
        fs::write(
            &runtime,
            serde_json::to_vec(&serde_json::json!({
                "mounts": [{
                    "type": "bind",
                    "source": "/mnt/runlab-shares/corpus/data.db",
                    "destination": "/data/input.db",
                    "options": ["rbind", "ro"]
                }]
            }))
            .expect("runtime JSON"),
        )
        .expect("runtime config");
        let shares = vec![ResolvedVmShare {
            name: "corpus".to_owned(),
            host_path: temporary.path().to_string_lossy().into_owned(),
            guest_path: "/mnt/runlab-shares/corpus".to_owned(),
            r#type: "virtiofs",
            read_only: true,
        }];

        validate_runtime_mounts(&runtime, &shares).expect("shared source");
    }

    #[test]
    fn bind_options_cannot_bypass_share_or_read_only_validation() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        fs::write(temporary.path().join("data.db"), b"sqlite").expect("fixture");
        let shares = vec![ResolvedVmShare {
            name: "corpus".to_owned(),
            host_path: temporary.path().to_string_lossy().into_owned(),
            guest_path: "/mnt/runlab-shares/corpus".to_owned(),
            r#type: "virtiofs",
            read_only: true,
        }];
        let runtime = temporary.path().join("config.json");
        fs::write(
            &runtime,
            serde_json::to_vec(&serde_json::json!({
                "mounts": [{
                    "type": "none",
                    "source": "/var/lib/runlab",
                    "destination": "/state",
                    "options": ["rbind", "ro"]
                }]
            }))
            .expect("runtime JSON"),
        )
        .expect("runtime config");
        let outside = validate_runtime_mounts(&runtime, &shares).expect_err("outside share");
        assert!(
            outside
                .to_string()
                .contains("not inside a declared VM share")
        );

        fs::write(
            &runtime,
            serde_json::to_vec(&serde_json::json!({
                "mounts": [{
                    "source": "/mnt/runlab-shares/corpus/data.db",
                    "destination": "/data/input.db",
                    "options": ["rbind", "ro", "rw"]
                }]
            }))
            .expect("runtime JSON"),
        )
        .expect("runtime config");
        let writable = validate_runtime_mounts(&runtime, &shares).expect_err("writable option");
        assert!(writable.to_string().contains("must not contain `rw`"));
    }

    #[test]
    fn shared_source_symlinks_cannot_escape_the_host_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let share_root = temporary.path().join("share");
        fs::create_dir(&share_root).expect("share root");
        fs::write(temporary.path().join("outside"), b"outside").expect("outside fixture");
        symlink("../outside", share_root.join("escape")).expect("escape symlink");
        let runtime = temporary.path().join("config.json");
        fs::write(
            &runtime,
            serde_json::to_vec(&serde_json::json!({
                "mounts": [{
                    "type": "bind",
                    "source": "/mnt/runlab-shares/corpus/escape",
                    "destination": "/data/input",
                    "options": ["rbind", "ro"]
                }]
            }))
            .expect("runtime JSON"),
        )
        .expect("runtime config");
        let shares = vec![ResolvedVmShare {
            name: "corpus".to_owned(),
            host_path: share_root.to_string_lossy().into_owned(),
            guest_path: "/mnt/runlab-shares/corpus".to_owned(),
            r#type: "virtiofs",
            read_only: true,
        }];

        let error = validate_runtime_mounts(&runtime, &shares).expect_err("symlink escape");
        assert!(error.to_string().contains("resolves outside"));
    }
}
