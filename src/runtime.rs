//! OCI Runtime `config.json`: structural validation, canonical bytes, and
//! authoring.
//!
//! One parse path, not two: a `RuntimeConfig` exists only after the document has
//! passed unique-key checking, the typed OCI view, and the `RunLab` profile a
//! backend requires. The exact bytes are retained, because the Run Record
//! identifies the config by digest.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Number, Value, json};

#[cfg(target_os = "linux")]
use crate::core::NetworkControl;
use crate::integrity::canonical_json;

const SUPPORTED_OCI_VERSION: &str = "1.2.0";
const REQUIRED_NAMESPACES: [&str; 6] = ["pid", "network", "ipc", "uts", "mount", "cgroup"];
const MANAGED_NAMESPACES: [&str; 5] = ["pid", "ipc", "uts", "mount", "cgroup"];
const MAX_NATIVE_FILE_MOUNTS: usize = 8;
const LINUX_HOSTNAME_MAX_BYTES: usize = 64;
const NATIVE_FILE_MOUNT_OPTIONS: [&str; 5] = ["bind", "ro", "nosuid", "nodev", "noexec"];
const STANDARD_MOUNT_DESTINATIONS: [&str; 5] =
    ["/proc", "/dev", "/dev/pts", "/dev/shm", "/dev/mqueue"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeFileMount {
    source: PathBuf,
    destination: PathBuf,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootlessMapping {
    pub(crate) host_uid: u32,
    pub(crate) host_gid: u32,
}

impl NativeFileMount {
    #[cfg(any(test, target_os = "linux"))]
    #[must_use]
    pub(crate) fn source(&self) -> &Path {
        &self.source
    }

    #[cfg(any(test, target_os = "linux"))]
    #[must_use]
    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn for_test(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    value: Value,
    oci_version: String,
}

impl RuntimeConfig {
    pub fn load(bytes: &[u8]) -> Result<Self> {
        Self::from_value(Self::load_unique(bytes)?)
    }

    pub(crate) fn load_rewriting_mount_sources(
        bytes: &[u8],
        mut rewrite: impl FnMut(&str) -> Result<Option<PathBuf>>,
    ) -> Result<Self> {
        let mut value = Self::load_unique(bytes)?;
        if let Some(mounts) = value.get_mut("mounts").and_then(Value::as_array_mut) {
            for mount in mounts {
                let Some(source) = mount
                    .get_mut("source")
                    .and_then(|source| source.as_str().map(ToOwned::to_owned))
                else {
                    continue;
                };
                if let Some(replacement) = rewrite(&source)? {
                    mount["source"] = Value::String(
                        replacement
                            .to_str()
                            .context("rewritten OCI mount source is not valid Unicode")?
                            .to_owned(),
                    );
                }
            }
        }
        Self::from_value(value)
    }

    pub(crate) fn native_file_mount_count(&self) -> Result<usize> {
        Ok(self.native_file_mounts()?.len())
    }

    fn load_unique(bytes: &[u8]) -> Result<Value> {
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let UniqueValue(value) = UniqueValue::deserialize(&mut deserializer)
            .context("OCI Runtime config.json is invalid JSON")?;
        deserializer
            .end()
            .context("OCI Runtime config.json has trailing data")?;
        Ok(value)
    }

    pub fn from_image_config(image: &Value) -> Result<Self> {
        let image = image
            .as_object()
            .context("OCI Image config must be an object")?;
        let config = match image.get("config") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(config)) => config.clone(),
            Some(_) => bail!("OCI Image config.config must be an object"),
        };
        let mut arguments = string_array(config.get("Entrypoint"), "Config.Entrypoint")?;
        arguments.extend(string_array(config.get("Cmd"), "Config.Cmd")?);
        if arguments.is_empty() {
            bail!("OCI Image has no Entrypoint or Cmd; provide an OCI Runtime config.json");
        }
        let environment = string_array(config.get("Env"), "Config.Env")?;
        let cwd = match config.get("WorkingDir") {
            None | Some(Value::Null) => "/",
            Some(Value::String(value)) if value.is_empty() => "/",
            Some(Value::String(value)) => value,
            Some(_) => bail!("OCI Image Config.WorkingDir must be a string"),
        };
        let user = image_user(config.get("User"))?;
        let value = json!({
            "ociVersion": SUPPORTED_OCI_VERSION,
            "root": {"path": "rootfs", "readonly": false},
            "process": {
                "terminal": false,
                "user": user,
                "args": arguments,
                "env": environment,
                "cwd": cwd,
                "noNewPrivileges": true
            },
            "hostname": "runlab",
            "mounts": [
                {
                    "destination": "/proc",
                    "type": "proc",
                    "source": "proc",
                    "options": ["nosuid", "noexec", "nodev"]
                },
                {
                    "destination": "/dev",
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
                },
                {
                    "destination": "/dev/pts",
                    "type": "devpts",
                    "source": "devpts",
                    "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]
                },
                {
                    "destination": "/dev/shm",
                    "type": "tmpfs",
                    "source": "shm",
                    "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
                },
                {
                    "destination": "/dev/mqueue",
                    "type": "mqueue",
                    "source": "mqueue",
                    "options": ["nosuid", "noexec", "nodev"]
                }
            ],
            "linux": {
                "namespaces": REQUIRED_NAMESPACES.map(|kind| json!({"type": kind}))
            }
        });
        Self::from_value(value)
    }

    pub fn encoded(&self) -> Result<Vec<u8>> {
        let mut bytes = canonical_json(&self.value)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    #[must_use]
    pub(crate) fn value(&self) -> &Value {
        &self.value
    }

    #[must_use]
    pub fn oci_version(&self) -> &str {
        &self.oci_version
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn validate_native_profile(&self) -> Result<()> {
        self.validate_native_profile_with_namespaces(&REQUIRED_NAMESPACES, "private")
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn validate_native_run_profile(&self, network: NetworkControl) -> Result<()> {
        match network {
            NetworkControl::None => self.validate_native_profile(),
            NetworkControl::Egress => {
                self.validate_native_profile_with_namespaces(&MANAGED_NAMESPACES, "run-network")
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn validate_native_rootless_profile(&self, network: NetworkControl) -> Result<()> {
        if network != NetworkControl::None {
            bail!("rootless native execution only supports network=none");
        }
        let process = self.value["process"]
            .as_object()
            .context("OCI Runtime process must be an object")?;
        let user = process
            .get("user")
            .and_then(Value::as_object)
            .context("OCI Runtime process.user must be an object")?;
        if user.get("uid").and_then(Value::as_u64) != Some(0)
            || user.get("gid").and_then(Value::as_u64) != Some(0)
        {
            bail!("rootless native execution only supports process.user uid=0 and gid=0");
        }
        if user
            .get("additionalGids")
            .and_then(Value::as_array)
            .is_some_and(|groups| !groups.is_empty())
        {
            bail!("rootless native execution does not support process.user.additionalGids");
        }
        let linux = self.value["linux"]
            .as_object()
            .context("OCI Runtime linux must be an object")?;
        if linux.get("resources").is_some_and(|value| !value.is_null()) {
            bail!("rootless native execution does not support linux.resources");
        }
        self.validate_native_profile()?;
        if !self.native_file_mounts()?.is_empty() {
            bail!("rootless native execution does not support read-only host mounts");
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn realize_rootless(&self, mapping: RootlessMapping) -> Result<Self> {
        let mut value = self.value.clone();
        let linux = value["linux"]
            .as_object_mut()
            .context("OCI Runtime linux must be an object")?;
        let namespaces = linux
            .get_mut("namespaces")
            .and_then(Value::as_array_mut)
            .context("OCI Runtime linux.namespaces must be an array")?;
        namespaces.push(json!({"type": "user"}));
        linux.insert(
            "uidMappings".to_owned(),
            json!([{"containerID": 0, "hostID": mapping.host_uid, "size": 1}]),
        );
        linux.insert(
            "gidMappings".to_owned(),
            json!([{"containerID": 0, "hostID": mapping.host_gid, "size": 1}]),
        );
        let mounts = value
            .get_mut("mounts")
            .and_then(Value::as_array_mut)
            .context("rootless native execution requires OCI mounts")?;
        let devpts = mounts
            .iter_mut()
            .find(|mount| mount.get("destination").and_then(Value::as_str) == Some("/dev/pts"))
            .and_then(Value::as_object_mut)
            .context("rootless native execution requires the standard /dev/pts mount")?;
        let options = devpts
            .get_mut("options")
            .and_then(Value::as_array_mut)
            .context("rootless /dev/pts mount requires options")?;
        options.retain(|option| option.as_str() != Some("gid=5"));
        Self::from_value(value)
    }

    pub(crate) fn validate_native_managed_profile(&self) -> Result<()> {
        self.validate_native_profile_with_namespaces(&MANAGED_NAMESPACES, "managed-service")
    }

    pub(crate) fn native_file_mounts(&self) -> Result<Vec<NativeFileMount>> {
        validate_native_mounts(self.value.get("mounts"))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn validate_native_resolver_destination(&self) -> Result<()> {
        if self
            .native_file_mounts()?
            .iter()
            .any(|mount| mount.destination() == Path::new("/etc/resolv.conf"))
        {
            bail!(
                "network=egress owns /etc/resolv.conf; the OCI Runtime config must not mount that destination"
            );
        }
        Ok(())
    }

    fn validate_native_profile_with_namespaces(
        &self,
        required_namespaces: &[&str],
        profile: &str,
    ) -> Result<()> {
        let document = self
            .value
            .as_object()
            .context("OCI Runtime config.json must be an object")?;
        reject_native_fields(
            document,
            &[
                "ociVersion",
                "root",
                "process",
                "hostname",
                "linux",
                "annotations",
                "mounts",
            ],
            "config.json",
        )?;
        let root = self.value["root"]
            .as_object()
            .context("OCI Runtime root must be an object")?;
        reject_native_fields(root, &["path", "readonly"], "root")?;
        if root.get("readonly").and_then(Value::as_bool) != Some(false) {
            bail!("the native execution profile requires root.readonly=false");
        }
        let process = self.value["process"]
            .as_object()
            .context("OCI Runtime process must be an object")?;
        reject_native_fields(
            process,
            &["terminal", "user", "args", "env", "cwd", "noNewPrivileges"],
            "process",
        )?;
        if process.get("terminal").and_then(Value::as_bool) != Some(false) {
            bail!("the native execution profile does not support process.terminal=true");
        }
        if process.get("noNewPrivileges").and_then(Value::as_bool) != Some(true) {
            bail!("the native execution profile requires process.noNewPrivileges=true");
        }
        self.native_file_mounts()?;
        require_native_standard_mounts(self.value.get("mounts"))?;
        if self
            .value
            .get("hooks")
            .is_some_and(|value| !value.is_null())
        {
            bail!("the native execution profile does not accept OCI hooks");
        }
        let linux = self.value["linux"]
            .as_object()
            .context("OCI Runtime linux must be an object")?;
        reject_native_fields(linux, &["namespaces", "devices", "resources"], "linux")?;
        if linux
            .get("devices")
            .and_then(Value::as_array)
            .is_some_and(|devices| !devices.is_empty())
        {
            bail!("the native execution profile does not accept Linux devices");
        }
        validate_native_resources(linux.get("resources"))?;
        let namespaces = linux
            .get("namespaces")
            .and_then(Value::as_array)
            .context("the native execution profile requires linux.namespaces")?;
        let mut actual = BTreeSet::new();
        for namespace in namespaces {
            let namespace = namespace
                .as_object()
                .context("OCI Runtime namespace must be an object")?;
            reject_native_fields(namespace, &["type"], "linux.namespaces[]")?;
            if namespace.get("path").is_some() {
                bail!("the native execution profile does not join existing namespaces");
            }
            let kind = namespace
                .get("type")
                .and_then(Value::as_str)
                .context("OCI Runtime namespace requires type")?;
            actual.insert(kind);
        }
        let required = required_namespaces.iter().copied().collect::<BTreeSet<_>>();
        if actual != required {
            bail!(
                "the native {profile} execution profile requires exactly these namespaces: {}",
                required_namespaces.join(", ")
            );
        }
        let user = process
            .get("user")
            .and_then(Value::as_object)
            .context("OCI Runtime process.user must be an object")?;
        reject_native_fields(user, &["uid", "gid", "additionalGids"], "process.user")?;
        Ok(())
    }

    fn from_value(mut value: Value) -> Result<Self> {
        structural_validate(&value)?;
        expand_defaults(&mut value)?;
        let runtime = runtime_profile(&value)?;
        validate_runtime(&runtime)?;
        Ok(Self {
            value,
            oci_version: runtime.oci_version,
        })
    }
}

fn validate_native_mounts(value: Option<&Value>) -> Result<Vec<NativeFileMount>> {
    let Some(mounts) = value else {
        return Ok(Vec::new());
    };
    let mounts = mounts
        .as_array()
        .context("OCI Runtime mounts must be an array")?;
    let mut destinations = BTreeSet::new();
    let mut file_mounts = Vec::new();
    for mount in mounts {
        let mount = mount
            .as_object()
            .context("OCI Runtime mount must be an object")?;
        reject_native_fields(
            mount,
            &["destination", "type", "source", "options"],
            "mounts[]",
        )?;
        let destination = mount
            .get("destination")
            .and_then(Value::as_str)
            .context("OCI Runtime mount requires destination")?;
        if !destinations.insert(destination) {
            bail!("the native execution profile rejects duplicate mount destination {destination}");
        }
        let standard: (&str, &str, &[&str]) = match destination {
            "/proc" => ("proc", "proc", &["nosuid", "noexec", "nodev"]),
            "/dev" => (
                "tmpfs",
                "tmpfs",
                &["nosuid", "strictatime", "mode=755", "size=65536k"],
            ),
            "/dev/pts" => (
                "devpts",
                "devpts",
                &[
                    "nosuid",
                    "noexec",
                    "newinstance",
                    "ptmxmode=0666",
                    "mode=0620",
                    "gid=5",
                ],
            ),
            "/dev/shm" => (
                "tmpfs",
                "shm",
                &["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
            ),
            "/dev/mqueue" => ("mqueue", "mqueue", &["nosuid", "noexec", "nodev"]),
            _ => {
                file_mounts.push(validate_native_file_mount(mount, destination)?);
                if file_mounts.len() > MAX_NATIVE_FILE_MOUNTS {
                    bail!(
                        "the native execution profile accepts at most {MAX_NATIVE_FILE_MOUNTS} read-only file mounts"
                    );
                }
                continue;
            }
        };
        let (required_type, required_source, required_options) = standard;
        if mount.get("type").and_then(Value::as_str) != Some(required_type)
            || mount.get("source").and_then(Value::as_str) != Some(required_source)
        {
            bail!("the native execution profile requires the standard mount at {destination}");
        }
        let options = mount
            .get("options")
            .and_then(Value::as_array)
            .context("native OCI mounts require options")?
            .iter()
            .map(|option| {
                option
                    .as_str()
                    .context("OCI Runtime mount options must be strings")
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let required_options = required_options.iter().copied().collect::<BTreeSet<_>>();
        if options != required_options {
            bail!("the native execution profile requires exact options at {destination}");
        }
    }
    Ok(file_mounts)
}

fn validate_native_resources(value: Option<&Value>) -> Result<()> {
    let Some(resources) = value else {
        return Ok(());
    };
    let resources = resources
        .as_object()
        .context("OCI Runtime linux.resources must be an object")?;
    reject_native_fields(resources, &["memory"], "linux.resources")?;
    let memory = resources
        .get("memory")
        .and_then(Value::as_object)
        .context("the native execution profile requires linux.resources.memory")?;
    reject_native_fields(memory, &["limit", "swap"], "linux.resources.memory")?;
    let limit = memory
        .get("limit")
        .and_then(Value::as_i64)
        .context("the native execution profile requires a positive memory limit")?;
    let swap = memory
        .get("swap")
        .and_then(Value::as_i64)
        .context("the native execution profile requires an explicit memory swap limit")?;
    if limit <= 0 {
        bail!("the native execution profile requires a positive memory limit");
    }
    if swap != limit {
        bail!(
            "the native execution profile requires memory.swap equal to memory.limit to disable swap"
        );
    }
    Ok(())
}

fn require_native_standard_mounts(value: Option<&Value>) -> Result<()> {
    let mounts = value
        .and_then(Value::as_array)
        .context("the native execution profile requires OCI mounts")?;
    let destinations = mounts
        .iter()
        .filter_map(|mount| mount.get("destination").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if let Some(destination) = STANDARD_MOUNT_DESTINATIONS
        .iter()
        .find(|destination| !destinations.contains(**destination))
    {
        bail!("the native execution profile requires the standard mount at {destination}");
    }
    Ok(())
}

fn validate_native_file_mount(
    mount: &Map<String, Value>,
    destination: &str,
) -> Result<NativeFileMount> {
    let source = mount
        .get("source")
        .and_then(Value::as_str)
        .context("native read-only file mounts require source")?;
    if mount.get("type").and_then(Value::as_str) != Some("bind") {
        bail!("the native execution profile requires type=bind at {destination}");
    }
    let options = mount
        .get("options")
        .and_then(Value::as_array)
        .context("native read-only file mounts require options")?;
    let exact_options = options
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .context("OCI Runtime mount options must be strings")?;
    if exact_options != NATIVE_FILE_MOUNT_OPTIONS {
        bail!(
            "the native execution profile requires exact ordered options [bind, ro, nosuid, nodev, noexec] at {destination}"
        );
    }
    let source = PathBuf::from(source);
    let destination = PathBuf::from(destination);
    validate_absolute_mount_path(&source, "source")?;
    validate_absolute_mount_path(&destination, "destination")?;
    Ok(NativeFileMount {
        source,
        destination,
    })
}

fn validate_absolute_mount_path(path: &Path, field: &str) -> Result<()> {
    let text = path
        .to_str()
        .context("native read-only file mount path is not valid UTF-8")?;
    if text.len() <= 1
        || !text.starts_with('/')
        || text.ends_with('/')
        || text.as_bytes().contains(&0)
        || text[1..]
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        bail!("native read-only file mount {field} must be an absolute normalized path");
    }
    Ok(())
}

fn reject_native_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    context: &str,
) -> Result<()> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        bail!("the native execution profile does not support {context}.{field}");
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProfile {
    oci_version: String,
    root: RuntimeRoot,
    process: RuntimeProcess,
    hostname: String,
    linux: RuntimeLinux,
    #[serde(default, rename = "annotations")]
    _annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RuntimeRoot {
    path: String,
    #[serde(rename = "readonly")]
    _readonly: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProcess {
    #[serde(rename = "terminal")]
    _terminal: bool,
    #[serde(rename = "user")]
    _user: RuntimeUser,
    args: Vec<String>,
    env: Vec<String>,
    cwd: String,
    #[serde(rename = "noNewPrivileges")]
    _no_new_privileges: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct RuntimeUser {
    #[serde(rename = "uid")]
    _uid: u32,
    #[serde(rename = "gid")]
    _gid: u32,
    #[serde(default)]
    #[serde(rename = "additionalGids")]
    _additional_gids: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct RuntimeLinux {
    #[serde(default)]
    namespaces: Vec<RuntimeNamespace>,
}

#[derive(Debug, Clone, Deserialize)]
struct RuntimeNamespace {
    #[serde(rename = "type")]
    kind: String,
}

fn structural_validate(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .context("OCI Runtime config.json must be a JSON object")?;
    if !matches!(object.get("ociVersion"), Some(Value::String(_))) {
        bail!("OCI Runtime config.json requires a string ociVersion");
    }
    if !matches!(object.get("root"), Some(Value::Object(_))) {
        bail!("OCI Runtime root must be an object");
    }
    if !matches!(object.get("process"), Some(Value::Object(_))) {
        bail!("OCI Runtime process must be an object");
    }
    if !matches!(object.get("linux"), Some(Value::Object(_))) {
        bail!("OCI Runtime linux must be an object");
    }
    Ok(())
}

fn expand_defaults(value: &mut Value) -> Result<()> {
    let object = value
        .as_object_mut()
        .context("OCI Runtime config.json must be a JSON object")?;
    object
        .entry("hostname")
        .or_insert_with(|| Value::String("runlab".to_owned()));
    let root = object
        .get_mut("root")
        .and_then(Value::as_object_mut)
        .context("OCI Runtime root must be an object")?;
    root.entry("readonly").or_insert(Value::Bool(false));
    let process = object
        .get_mut("process")
        .and_then(Value::as_object_mut)
        .context("OCI Runtime process must be an object")?;
    process.entry("terminal").or_insert(Value::Bool(false));
    process
        .entry("env")
        .or_insert_with(|| Value::Array(Vec::new()));
    process
        .entry("noNewPrivileges")
        .or_insert(Value::Bool(false));
    object
        .entry("annotations")
        .or_insert_with(|| Value::Object(Map::new()));
    Ok(())
}

fn runtime_profile(value: &Value) -> Result<RuntimeProfile> {
    serde_json::from_value::<oci_spec::runtime::Spec>(value.clone())
        .context("OCI Runtime config.json has an invalid standard field")?;
    serde_json::from_value(value.clone())
        .context("OCI Runtime config.json has an invalid standard field")
}

fn validate_runtime(runtime: &RuntimeProfile) -> Result<()> {
    if runtime.oci_version != SUPPORTED_OCI_VERSION {
        bail!(
            "RunLab supports OCI Runtime version {SUPPORTED_OCI_VERSION}, received {}",
            runtime.oci_version
        );
    }
    if runtime.root.path != "rootfs" {
        bail!("OCI Runtime root.path must be the portable bundle path \"rootfs\"");
    }
    if runtime.process.args.is_empty()
        || runtime
            .process
            .args
            .iter()
            .any(|argument| argument.is_empty() || argument.contains('\0'))
    {
        bail!("OCI Runtime process.args must be a non-empty string array without NUL");
    }
    if !runtime.process.cwd.starts_with('/') || runtime.process.cwd.contains('\0') {
        bail!("OCI Runtime process.cwd must be an absolute Linux path");
    }
    validate_environment(&runtime.process.env)?;
    if runtime.hostname.is_empty()
        || runtime.hostname.contains('\0')
        || runtime.hostname.len() > LINUX_HOSTNAME_MAX_BYTES
    {
        bail!(
            "OCI Runtime hostname must be a non-empty string without NUL and no more than {LINUX_HOSTNAME_MAX_BYTES} bytes"
        );
    }
    let mut namespaces = BTreeSet::new();
    for namespace in &runtime.linux.namespaces {
        if !namespaces.insert(namespace.kind.as_str()) {
            bail!(
                "OCI Runtime linux.namespaces contains duplicate type: {}",
                namespace.kind
            );
        }
    }
    Ok(())
}

fn validate_environment(environment: &[String]) -> Result<()> {
    let mut names = BTreeSet::new();
    for item in environment {
        if item.contains('\0') {
            bail!("OCI Runtime process.env entries cannot contain NUL");
        }
        let Some((name, _)) = item.split_once('=') else {
            bail!("OCI Runtime process.env must contain NAME=VALUE strings");
        };
        if name.is_empty() {
            bail!("OCI Runtime process.env variable names cannot be empty");
        }
        if !names.insert(name) {
            bail!("OCI Runtime process.env contains duplicate variable: {name}");
        }
    }
    Ok(())
}

fn image_user(value: Option<&Value>) -> Result<AuthoredUser> {
    let value = match value {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value,
        Some(_) => bail!("OCI Image Config.User must be a string"),
    };
    if matches!(value, "" | "0" | "0:0" | "root" | "root:root") {
        return Ok(AuthoredUser {
            uid: 0,
            gid: 0,
            additional_gids: Vec::new(),
        });
    }
    let Some((user, group)) = value.split_once(':') else {
        bail!(
            "the converter supports empty/root or numeric UID:GID Image User; provide --runtime-config for named or group-inferred users"
        );
    };
    let uid = user
        .parse::<u32>()
        .context("OCI Image Config.User UID is not numeric")?;
    let gid = group
        .parse::<u32>()
        .context("OCI Image Config.User GID is not numeric")?;
    Ok(AuthoredUser {
        uid,
        gid,
        additional_gids: Vec::new(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthoredUser {
    uid: u32,
    gid: u32,
    additional_gids: Vec<u32>,
}

fn string_array(value: Option<&Value>, field: &str) -> Result<Vec<String>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .with_context(|| format!("OCI Image {field} must be a string array"))
            })
            .collect(),
        Some(_) => bail!("OCI Image {field} must be a string array"),
    }
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("JSON number is not finite"))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueValue(value)) = sequence.next_element::<UniqueValue>()? {
            values.push(value);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key: {key}"
                )));
            }
            let UniqueValue(value) = map.next_value::<UniqueValue>()?;
            values.insert(key, value);
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_with_root(root: &Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "ociVersion": "1.2.0",
            "root": root,
            "process": {
                "args": ["/bin/true"],
                "cwd": "/",
                "user": {"uid": 0, "gid": 0}
            },
            "linux": {"namespaces": []}
        }))
        .expect("runtime config")
    }

    fn runtime_with_mounts(mounts: &Value) -> RuntimeConfig {
        RuntimeConfig::load(
            &serde_json::to_vec(&json!({
                "ociVersion": "1.2.0",
                "root": {"path": "rootfs", "readonly": false},
                "process": {
                    "terminal": false,
                    "args": ["/bin/true"],
                    "cwd": "/",
                    "user": {"uid": 0, "gid": 0},
                    "noNewPrivileges": true
                },
                "hostname": "runlab",
                "mounts": mounts,
                "linux": {"namespaces": []}
            }))
            .expect("runtime config JSON"),
        )
        .expect("runtime config")
    }

    #[test]
    fn image_defaults_become_explicit_runtime_config() {
        let config = RuntimeConfig::from_image_config(&json!({
            "config": {
                "User": "0:0",
                "Env": ["PATH=/usr/bin:/bin", "MODE=test"],
                "Entrypoint": ["/bin/sh"],
                "Cmd": ["-c", "exit 0"],
                "WorkingDir": "/workspace"
            }
        }))
        .expect("runtime config");
        assert_eq!(
            config.value()["process"]["args"],
            json!(["/bin/sh", "-c", "exit 0"])
        );
        assert_eq!(config.value()["process"]["user"]["uid"], 0);
        RuntimeConfig::load(&config.encoded().expect("encoded")).expect("round trip");
    }

    #[test]
    fn duplicate_json_keys_are_rejected() {
        let error = RuntimeConfig::load(br#"{"ociVersion":"1.2.0","ociVersion":"1.2.0"}"#)
            .expect_err("duplicate key");
        assert!(error.to_string().contains("invalid JSON"));
        assert!(format!("{error:#}").contains("duplicate JSON key"));
    }

    #[test]
    fn non_portable_bundle_root_paths_are_rejected() {
        for path in [
            "",
            ".",
            "./rootfs",
            "/rootfs",
            "../rootfs",
            "rootfs/",
            "rootfs/sub",
        ] {
            let bytes = runtime_with_root(&json!({"path": path}));
            let error = RuntimeConfig::load(&bytes).expect_err("invalid root path");
            assert!(
                error
                    .to_string()
                    .contains("portable bundle path \"rootfs\"")
            );
        }
    }

    #[test]
    fn hostname_uses_the_linux_byte_limit() {
        let mut value = serde_json::from_slice::<Value>(&runtime_with_root(&json!({
            "path": "rootfs"
        })))
        .expect("runtime JSON");

        value["hostname"] = json!("a".repeat(LINUX_HOSTNAME_MAX_BYTES));
        RuntimeConfig::from_value(value.clone()).expect("64-byte hostname");

        value["hostname"] = json!("a".repeat(LINUX_HOSTNAME_MAX_BYTES + 1));
        let error = RuntimeConfig::from_value(value.clone()).expect_err("65-byte hostname");
        assert!(error.to_string().contains("no more than 64 bytes"));

        value["hostname"] = json!("界".repeat(22));
        let error = RuntimeConfig::from_value(value).expect_err("66-byte hostname");
        assert!(error.to_string().contains("no more than 64 bytes"));
    }

    #[test]
    fn missing_or_invalid_bundle_root_is_rejected() {
        for root in [Value::Null, json!({}), json!({"path": 3})] {
            let bytes = runtime_with_root(&root);
            assert!(RuntimeConfig::load(&bytes).is_err());
        }
    }

    #[test]
    fn backend_unsupported_fields_are_preserved() {
        let value = br#"{
            "ociVersion":"1.2.0",
            "root":{"path":"rootfs"},
            "process":{"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0}},
            "linux":{"namespaces":[]},
            "hooks":{}
        }"#;
        let runtime = RuntimeConfig::load(value).expect("valid OCI Runtime config");
        assert_eq!(runtime.value()["hooks"], json!({}));
    }

    #[test]
    fn malformed_unsupported_standard_fields_are_invalid_oci() {
        let value = br#"{
            "ociVersion":"1.2.0",
            "root":{"path":"rootfs"},
            "process":{"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0}},
            "linux":{"namespaces":[]},
            "hooks":{"prestart":[{"path":42}]}
        }"#;
        let error = RuntimeConfig::load(value).expect_err("invalid hook path");
        assert!(error.to_string().contains("invalid standard field"));
    }

    #[test]
    fn native_file_mount_requires_the_canonical_read_only_profile() {
        let runtime = runtime_with_mounts(&json!([{
            "destination": "/run/credential",
            "type": "bind",
            "source": "/var/runlab-input/credential",
            "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
        }]));
        let mounts = runtime.native_file_mounts().expect("native file mount");
        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source(),
            Path::new("/var/runlab-input/credential")
        );
        assert_eq!(mounts[0].destination(), Path::new("/run/credential"));

        for invalid in [
            json!({
                "destination": "/run/credential",
                "type": "none",
                "source": "/var/runlab-input/credential",
                "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
            }),
            json!({
                "destination": "/run/credential",
                "type": "bind",
                "source": "/var/runlab-input/credential",
                "options": ["ro", "bind", "nosuid", "nodev", "noexec"]
            }),
            json!({
                "destination": "run/credential",
                "type": "bind",
                "source": "/var/runlab-input/credential",
                "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
            }),
            json!({
                "destination": "/run//credential",
                "type": "bind",
                "source": "/var/runlab-input/credential",
                "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
            }),
            json!({
                "destination": "/run/credential",
                "type": "bind",
                "source": "/var/runlab-input/../credential",
                "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
            }),
        ] {
            assert!(
                runtime_with_mounts(&json!([invalid]))
                    .native_file_mounts()
                    .is_err()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn egress_resolver_destination_has_single_backend_ownership() {
        let runtime = runtime_with_mounts(&json!([{
            "destination": "/etc/resolv.conf",
            "type": "bind",
            "source": "/var/runlab-input/resolv.conf",
            "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
        }]));
        let error = runtime
            .validate_native_resolver_destination()
            .expect_err("resolver destination collision");
        assert!(error.to_string().contains("network=egress owns"));
    }

    #[test]
    fn native_file_mount_limit_is_global_to_one_runtime_config() {
        let mounts = (0..=MAX_NATIVE_FILE_MOUNTS)
            .map(|index| {
                json!({
                    "destination": format!("/run/credential-{index}"),
                    "type": "bind",
                    "source": format!("/var/runlab-input/credential-{index}"),
                    "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
                })
            })
            .collect::<Vec<_>>();
        let error = runtime_with_mounts(&Value::Array(mounts))
            .native_file_mounts()
            .expect_err("too many file mounts");
        assert!(error.to_string().contains("at most 8"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_profile_requires_every_standard_mount() {
        let authored = RuntimeConfig::from_image_config(&json!({
            "config": {"Entrypoint": ["/bin/true"]}
        }))
        .expect("authored runtime config");
        for missing in STANDARD_MOUNT_DESTINATIONS {
            let mut value = authored.value().clone();
            value["mounts"]
                .as_array_mut()
                .expect("mount array")
                .retain(|mount| mount["destination"] != missing);
            let runtime = RuntimeConfig::from_value(value).expect("valid OCI Runtime config");
            let error = runtime
                .validate_native_profile()
                .expect_err("incomplete native mount profile");
            assert!(error.to_string().contains(missing));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_profile_accepts_only_a_no_swap_memory_limit() {
        let authored = RuntimeConfig::from_image_config(&json!({
            "config": {"Entrypoint": ["/bin/true"]}
        }))
        .expect("authored runtime config");
        let mut value = authored.value().clone();
        value["linux"]["resources"] = json!({
            "memory": {"limit": 67_108_864, "swap": 67_108_864}
        });
        RuntimeConfig::from_value(value.clone())
            .expect("valid OCI Runtime config")
            .validate_native_profile()
            .expect("supported memory profile");

        for memory in [
            json!({"limit": 0, "swap": 0}),
            json!({"limit": 67_108_864}),
            json!({"limit": 67_108_864, "swap": 134_217_728}),
            json!({"limit": 67_108_864, "swap": 67_108_864, "reservation": 1}),
        ] {
            value["linux"]["resources"] = json!({"memory": memory});
            let error = RuntimeConfig::from_value(value.clone())
                .expect("valid OCI Runtime config")
                .validate_native_profile()
                .expect_err("unsupported memory profile");
            assert!(error.to_string().contains("memory"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_network_namespace_profiles_are_distinct() {
        let single = RuntimeConfig::from_image_config(&json!({
            "config": {
                "Entrypoint": ["/bin/true"]
            }
        }))
        .expect("single-participant runtime config");
        single
            .validate_native_profile()
            .expect("single-participant profile");
        single
            .validate_native_run_profile(NetworkControl::None)
            .expect("network=none single-participant profile");
        let error = single
            .validate_native_run_profile(NetworkControl::Egress)
            .expect_err("network=egress must inherit a Run-owned namespace");
        assert!(error.to_string().contains("run-network"));
        let error = single
            .validate_native_managed_profile()
            .expect_err("managed profile must inherit the Run network namespace");
        assert!(error.to_string().contains("managed-service"));

        let mut managed = single.value().clone();
        managed["linux"]["namespaces"]
            .as_array_mut()
            .expect("namespace array")
            .retain(|namespace| namespace["type"] != "network");
        let managed = RuntimeConfig::load(
            &serde_json::to_vec(&managed).expect("managed runtime config JSON"),
        )
        .expect("managed runtime config");
        managed
            .validate_native_managed_profile()
            .expect("managed-service profile");
        managed
            .validate_native_run_profile(NetworkControl::Egress)
            .expect("network=egress Run-owned namespace profile");
        let error = managed
            .validate_native_run_profile(NetworkControl::None)
            .expect_err("network=none single participant creates its own namespace");
        assert!(error.to_string().contains("private"));
        let error = managed
            .validate_native_profile()
            .expect_err("single-participant profile must create a private network namespace");
        assert!(error.to_string().contains("private"));
    }

    #[test]
    fn vm_mount_source_rewrite_is_structural_and_canonical() {
        let authored = RuntimeConfig::from_image_config(&json!({
            "config": {"Entrypoint": ["/bin/true"]}
        }))
        .expect("authored config");
        let mut value = authored.value().clone();
        value["mounts"].as_array_mut().unwrap().push(json!({
            "destination": "/run/credential",
            "type": "bind",
            "source": "@input/1",
            "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
        }));
        let bytes = serde_json::to_vec(&value).unwrap();

        let rewritten = RuntimeConfig::load_rewriting_mount_sources(&bytes, |source| {
            Ok((source == "@input/1")
                .then(|| PathBuf::from("/var/lib/runlab/vm-inputs/op/source-1")))
        })
        .expect("rewritten config");

        let mounts = rewritten.native_file_mounts().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source(),
            Path::new("/var/lib/runlab/vm-inputs/op/source-1")
        );
        assert!(rewritten.encoded().unwrap().ends_with(b"\n"));
    }

    #[test]
    fn vm_mount_source_rewrite_preserves_duplicate_key_rejection() {
        let error = RuntimeConfig::load_rewriting_mount_sources(
            br#"{"ociVersion":"1.2.0","ociVersion":"1.2.0"}"#,
            |_| Ok(None),
        )
        .expect_err("duplicate key");
        assert!(format!("{error:#}").contains("duplicate JSON key"));
    }
}
