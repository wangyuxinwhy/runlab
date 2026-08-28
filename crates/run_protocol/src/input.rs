use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use oci_spec::runtime::Spec;
use serde_json::Value;

use crate::json::parse_unique;
use crate::{ImageDescriptor, InputError, InputPath};

/// Maximum raw standard input accepted for one Program.
pub const MAX_STDIN_BYTES: usize = 10 * 1024 * 1024;

const OCI_RUNTIME_VERSION: &str = "1.3.0";

/// Sensitive bytes supplied to one Program without placing them in its OCI
/// Runtime Configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretValue(Arc<[u8]>);

impl SecretValue {
    /// Owns the exact sensitive bytes.
    #[must_use]
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(Arc::from(value.into()))
    }

    /// Returns the exact bytes for faithful Engine delivery.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

/// Sensitive environment variables and regular files delivered to one
/// Program independently of its OCI Runtime Configuration.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct Secrets {
    env: BTreeMap<String, SecretValue>,
    files: BTreeMap<String, SecretValue>,
}

impl Secrets {
    /// Validates a complete set of Program secrets.
    ///
    /// Environment names use the portable shell identifier form. File keys
    /// are normalized absolute Linux container paths. Environment values must
    /// be UTF-8 without NUL because they are delivered through `execve`.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when a name, path, or environment value cannot
    /// be delivered faithfully.
    pub fn new(
        env: BTreeMap<String, SecretValue>,
        files: BTreeMap<String, SecretValue>,
    ) -> Result<Self, InputError> {
        for (name, value) in &env {
            let path = InputPath::field("secrets").child("env").key(name);
            if !valid_environment_name(name) {
                return Err(InputError::new(
                    path,
                    "secret environment name must match [A-Za-z_][A-Za-z0-9_]*",
                ));
            }
            if std::str::from_utf8(value.as_bytes()).is_err() {
                return Err(InputError::new(
                    path,
                    "secret environment value must be UTF-8",
                ));
            }
            if value.as_bytes().contains(&0) {
                return Err(InputError::new(
                    path,
                    "secret environment value cannot contain NUL",
                ));
            }
        }
        for container_path in files.keys() {
            if !valid_container_path(container_path) {
                return Err(InputError::new(
                    InputPath::field("secrets")
                        .child("files")
                        .key(container_path),
                    "secret file destination must be a normalized absolute container path other than /",
                ));
            }
        }
        Ok(Self { env, files })
    }

    /// Returns an empty Secret set.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns sensitive environment variables keyed by their exact Program
    /// environment names.
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, SecretValue> {
        &self.env
    }

    /// Returns sensitive regular-file contents keyed by absolute container
    /// destination.
    #[must_use]
    pub fn files(&self) -> &BTreeMap<String, SecretValue> {
        &self.files
    }

    /// Returns whether no Secret input is present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.files.is_empty()
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Secrets")
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("files", &self.files.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn valid_container_path(value: &str) -> bool {
    value.starts_with('/')
        && value != "/"
        && !value.ends_with('/')
        && !value.as_bytes().contains(&0)
        && value
            .split('/')
            .skip(1)
            .all(|component| !matches!(component, "" | "." | ".."))
}

/// Identity of one Program within a single [`RunInput`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProgramId(String);

impl ProgramId {
    /// Creates a Program identity. The protocol assigns no path or DNS meaning
    /// to the caller-selected text.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the required main Program identity.
    #[must_use]
    pub fn primary() -> Self {
        Self("primary".to_owned())
    }

    /// Returns whether this is the required main Program.
    #[must_use]
    pub fn is_primary(&self) -> bool {
        self.0 == "primary"
    }

    /// Returns the caller-selected identity text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProgramId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Network access allowed to cross this execution boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Network {
    /// Blocks inbound and outbound traffic across the execution boundary.
    Isolated,
    /// Allows outbound connections and their return traffic, but no new inbound connections.
    Egress,
}

/// Controls that apply to the complete Run Engine invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunControls {
    execution_timeout_ms: Option<NonZeroU64>,
    network: Network,
    capture_final_environment: bool,
}

impl RunControls {
    /// Creates the complete set of invocation controls.
    #[must_use]
    pub const fn new(
        execution_timeout_ms: Option<NonZeroU64>,
        network: Network,
        capture_final_environment: bool,
    ) -> Self {
        Self {
            execution_timeout_ms,
            network,
            capture_final_environment,
        }
    }

    /// Returns the optional monotonic execution deadline in milliseconds.
    #[must_use]
    pub const fn execution_timeout_ms(self) -> Option<NonZeroU64> {
        self.execution_timeout_ms
    }

    /// Returns the network policy for the complete invocation.
    #[must_use]
    pub const fn network(self) -> Network {
        self.network
    }

    /// Returns whether final environments must be captured and published.
    #[must_use]
    pub const fn capture_final_environment(self) -> bool {
        self.capture_final_environment
    }
}

/// Exact OCI Runtime Configuration bytes with validated JSON and typed views.
#[derive(Clone)]
pub struct RuntimeConfig {
    bytes: Arc<[u8]>,
    value: Value,
    spec: Spec,
}

impl RuntimeConfig {
    /// Parses an OCI Runtime Specification 1.3.0 configuration without
    /// rewriting the supplied bytes.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when JSON, the OCI typed view, or a fixed Run
    /// Protocol constraint is invalid.
    pub fn parse(bytes: impl Into<Vec<u8>>) -> Result<Self, InputError> {
        let bytes = bytes.into();
        let value = parse_unique(&bytes, InputPath::field("runtime_config"))?;
        let spec = serde_json::from_value::<Spec>(value.clone()).map_err(|error| {
            InputError::new(
                InputPath::field("runtime_config"),
                format!("invalid OCI Runtime Configuration: {error}"),
            )
        })?;

        if spec.version() != OCI_RUNTIME_VERSION {
            return Err(InputError::new(
                InputPath::field("runtime_config").child("ociVersion"),
                format!(
                    "expected OCI Runtime Specification {OCI_RUNTIME_VERSION}, received {}",
                    spec.version()
                ),
            ));
        }
        let root = spec.root().as_ref().ok_or_else(|| {
            InputError::new(
                InputPath::field("runtime_config").child("root"),
                "root is required",
            )
        })?;
        if root.path().as_os_str() != "rootfs" {
            return Err(InputError::new(
                InputPath::field("runtime_config")
                    .child("root")
                    .child("path"),
                "root.path must be the literal bundle path rootfs",
            ));
        }
        let process = spec.process().as_ref().ok_or_else(|| {
            InputError::new(
                InputPath::field("runtime_config").child("process"),
                "process is required",
            )
        })?;
        if process.terminal() == Some(true) {
            return Err(InputError::new(
                InputPath::field("runtime_config")
                    .child("process")
                    .child("terminal"),
                "terminal must be false so stdin, stdout, and stderr remain separate",
            ));
        }

        Ok(Self {
            bytes: Arc::from(bytes),
            value,
            spec,
        })
    }

    /// Returns the exact bytes supplied by the caller.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the parsed JSON value used for protocol-level comparison.
    #[must_use]
    pub fn as_json(&self) -> &Value {
        &self.value
    }

    /// Returns the typed OCI view without changing content identity.
    #[must_use]
    pub fn as_oci(&self) -> &Spec {
        &self.spec
    }
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("byte_len", &self.bytes.len())
            .field("oci_version", self.spec.version())
            .finish_non_exhaustive()
    }
}

impl PartialEq for RuntimeConfig {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for RuntimeConfig {}

/// Input for one Program in a [`RunInput`].
#[derive(Clone, Eq, PartialEq)]
pub struct ProgramInput {
    initial_environment: ImageDescriptor,
    runtime_config: RuntimeConfig,
    stdin: Arc<[u8]>,
    secrets: Secrets,
}

impl fmt::Debug for ProgramInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgramInput")
            .field("initial_environment", &self.initial_environment)
            .field("runtime_config", &self.runtime_config)
            .field("stdin_len", &self.stdin.len())
            .field("secrets", &self.secrets)
            .finish()
    }
}

impl ProgramInput {
    /// Creates one Program input after enforcing the fixed stdin limit.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when `stdin` exceeds [`MAX_STDIN_BYTES`].
    pub fn new(
        initial_environment: ImageDescriptor,
        runtime_config: RuntimeConfig,
        stdin: impl Into<Vec<u8>>,
        secrets: Secrets,
    ) -> Result<Self, InputError> {
        let stdin = stdin.into();
        if stdin.len() > MAX_STDIN_BYTES {
            return Err(InputError::new(
                InputPath::field("stdin"),
                format!(
                    "stdin contains {} bytes; the maximum is {MAX_STDIN_BYTES}",
                    stdin.len()
                ),
            ));
        }
        Ok(Self {
            initial_environment,
            runtime_config,
            stdin: Arc::from(stdin),
            secrets,
        })
    }

    #[must_use]
    /// Returns the OCI Image Manifest descriptor for the initial filesystem.
    pub fn initial_environment(&self) -> &ImageDescriptor {
        &self.initial_environment
    }

    #[must_use]
    /// Returns the exact and parsed OCI Runtime Configuration.
    pub fn runtime_config(&self) -> &RuntimeConfig {
        &self.runtime_config
    }

    #[must_use]
    /// Returns the raw bytes to write to standard input.
    pub fn stdin(&self) -> &[u8] {
        &self.stdin
    }

    /// Returns sensitive Program inputs delivered independently of the OCI
    /// Runtime Configuration.
    #[must_use]
    pub fn secrets(&self) -> &Secrets {
        &self.secrets
    }
}

/// Complete, resolved input to one Run Engine invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunInput {
    programs: BTreeMap<ProgramId, ProgramInput>,
    controls: RunControls,
}

impl RunInput {
    /// Creates a complete input. Product selectors and defaults must already
    /// have been resolved by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when the `primary` Program is absent.
    pub fn new(
        programs: BTreeMap<ProgramId, ProgramInput>,
        controls: RunControls,
    ) -> Result<Self, InputError> {
        if !programs.contains_key(&ProgramId::primary()) {
            return Err(InputError::new(
                InputPath::field("programs").key("primary"),
                "every RunInput must contain the primary Program",
            ));
        }
        Ok(Self { programs, controls })
    }

    #[must_use]
    /// Returns every Program keyed by its caller-selected identity.
    pub fn programs(&self) -> &BTreeMap<ProgramId, ProgramInput> {
        &self.programs
    }

    #[must_use]
    /// Returns the optional monotonic execution deadline in milliseconds.
    pub const fn controls(&self) -> RunControls {
        self.controls
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use oci_spec::image::Descriptor;

    use super::*;

    fn runtime(arguments: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "ociVersion": "1.3.0",
            "root": {"path": "rootfs"},
            "process": {
                "terminal": false,
                "args": arguments,
                "cwd": "/",
                "user": {"uid": 0, "gid": 0}
            },
            "linux": {}
        }))
        .expect("runtime JSON")
    }

    fn image() -> ImageDescriptor {
        let descriptor: Descriptor = serde_json::from_str(
            r#"{
                "mediaType":"application/vnd.oci.image.manifest.v1+json",
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size":123
            }"#,
        )
        .expect("OCI Descriptor");
        ImageDescriptor::new(descriptor).expect("Image Manifest")
    }

    fn program(stdin: Vec<u8>) -> ProgramInput {
        ProgramInput::new(
            image(),
            RuntimeConfig::parse(runtime(&["/bin/true"])).expect("RuntimeConfig"),
            stdin,
            Secrets::empty(),
        )
        .expect("ProgramInput")
    }

    #[test]
    fn runtime_config_keeps_exact_bytes_and_compares_json_values() {
        let first = br#"{
          "ociVersion":"1.3.0",
          "root":{"path":"rootfs"},
          "process":{"terminal":false,"args":["a","b"],"cwd":"/","user":{"uid":0,"gid":0}},
          "linux":{}
        }"#;
        let reordered = br#"{"linux":{},"process":{"user":{"gid":0,"uid":0},"cwd":"/","args":["a","b"],"terminal":false},"root":{"path":"rootfs"},"ociVersion":"1.3.0"}"#;
        let changed_array = br#"{"linux":{},"process":{"user":{"gid":0,"uid":0},"cwd":"/","args":["b","a"],"terminal":false},"root":{"path":"rootfs"},"ociVersion":"1.3.0"}"#;

        let first_config = RuntimeConfig::parse(first.to_vec()).expect("first config");
        let reordered_config = RuntimeConfig::parse(reordered.to_vec()).expect("reordered config");
        let changed_config = RuntimeConfig::parse(changed_array.to_vec()).expect("changed config");

        assert_eq!(first_config.as_bytes(), first);
        assert_eq!(first_config, reordered_config);
        assert_ne!(first_config, changed_config);
    }

    #[test]
    fn runtime_config_rejects_duplicate_keys_trailing_data_and_wrong_protocol_fields() {
        let duplicate = br#"{"ociVersion":"1.3.0","ociVersion":"1.3.0"}"#;
        let error = RuntimeConfig::parse(duplicate.to_vec()).expect_err("duplicate key");
        assert!(error.reason().contains("duplicate JSON key"));

        let mut trailing = runtime(&["/bin/true"]);
        trailing.extend_from_slice(b" false");
        let error = RuntimeConfig::parse(trailing).expect_err("trailing value");
        assert!(error.reason().contains("trailing JSON data"));

        let wrong_version = String::from_utf8(runtime(&["/bin/true"]))
            .expect("UTF-8")
            .replace("1.3.0", "1.2.0");
        let error = RuntimeConfig::parse(wrong_version.into_bytes()).expect_err("version");
        assert_eq!(error.path().to_string(), "runtime_config.ociVersion");

        let wrong_root = String::from_utf8(runtime(&["/bin/true"]))
            .expect("UTF-8")
            .replace("rootfs", "/tmp/rootfs");
        let error = RuntimeConfig::parse(wrong_root.into_bytes()).expect_err("root path");
        assert_eq!(error.path().to_string(), "runtime_config.root.path");

        let terminal = String::from_utf8(runtime(&["/bin/true"]))
            .expect("UTF-8")
            .replace("\"terminal\":false", "\"terminal\":true");
        let error = RuntimeConfig::parse(terminal.into_bytes()).expect_err("terminal");
        assert_eq!(error.path().to_string(), "runtime_config.process.terminal");
    }

    #[test]
    fn input_error_can_be_located_under_an_arbitrary_program_key() {
        let error = RuntimeConfig::parse(br"{}".to_vec())
            .expect_err("invalid config")
            .under(InputPath::field("programs").key("dependency.with[syntax]"));
        assert_eq!(
            error.path().to_string(),
            "programs[\"dependency.with[syntax]\"].runtime_config.ociVersion"
        );
    }

    #[test]
    fn stdin_limit_and_primary_program_are_structural_invariants() {
        ProgramInput::new(
            image(),
            RuntimeConfig::parse(runtime(&["/bin/true"])).expect("RuntimeConfig"),
            vec![0; MAX_STDIN_BYTES],
            Secrets::empty(),
        )
        .expect("stdin at limit");
        let error = ProgramInput::new(
            image(),
            RuntimeConfig::parse(runtime(&["/bin/true"])).expect("RuntimeConfig"),
            vec![0; MAX_STDIN_BYTES + 1],
            Secrets::empty(),
        )
        .expect_err("stdin over limit");
        assert_eq!(error.path().to_string(), "stdin");

        let mut programs = BTreeMap::new();
        programs.insert(ProgramId::new("dependency"), program(Vec::new()));
        let error = RunInput::new(
            programs,
            RunControls::new(NonZeroU64::new(1), Network::Isolated, true),
        )
        .expect_err("missing primary");
        assert_eq!(error.path().to_string(), "programs[\"primary\"]");
    }

    #[test]
    fn run_controls_keep_execution_choices_explicit() {
        let controls = RunControls::new(NonZeroU64::new(25), Network::Egress, false);
        assert_eq!(
            controls.execution_timeout_ms().map(NonZeroU64::get),
            Some(25)
        );
        assert_eq!(controls.network(), Network::Egress);
        assert!(!controls.capture_final_environment());
    }

    #[test]
    fn program_identity_has_no_hidden_dns_or_path_grammar() {
        let identity = ProgramId::new(" DB/Main service [] ");
        assert_eq!(identity.as_str(), " DB/Main service [] ");
        assert!(!identity.is_primary());
    }

    #[test]
    fn secrets_are_grouped_by_unique_environment_names_and_file_paths() {
        let secrets = Secrets::new(
            BTreeMap::from([(
                "DEEPSEEK_API_KEY".to_owned(),
                SecretValue::new(b"sensitive-env".to_vec()),
            )]),
            BTreeMap::from([(
                "/run/secrets/krb5cc".to_owned(),
                SecretValue::new(vec![0, 1, 2, 255]),
            )]),
        )
        .expect("Secrets");
        assert_eq!(
            secrets.env()["DEEPSEEK_API_KEY"].as_bytes(),
            b"sensitive-env"
        );
        assert_eq!(
            secrets.files()["/run/secrets/krb5cc"].as_bytes(),
            &[0, 1, 2, 255]
        );
        let debug = format!("{secrets:?}");
        assert!(debug.contains("DEEPSEEK_API_KEY"));
        assert!(!debug.contains("sensitive-env"));
    }

    #[test]
    fn secret_environment_and_file_destinations_are_validated() {
        for name in ["", "1TOKEN", "TOKEN-NAME", "TOKEN=VALUE"] {
            let error = Secrets::new(
                BTreeMap::from([(name.to_owned(), SecretValue::new(b"value".to_vec()))]),
                BTreeMap::new(),
            )
            .expect_err("invalid environment name");
            assert_eq!(error.path().to_string(), format!("secrets.env[{name:?}]"));
        }
        for path in ["", "relative", "/", "/run/../secret", "/run/./secret"] {
            let error = Secrets::new(
                BTreeMap::new(),
                BTreeMap::from([(path.to_owned(), SecretValue::new(Vec::new()))]),
            )
            .expect_err("invalid file path");
            assert_eq!(error.path().to_string(), format!("secrets.files[{path:?}]"));
        }
        for value in [vec![0xff], b"contains\0nul".to_vec()] {
            let error = Secrets::new(
                BTreeMap::from([("TOKEN".to_owned(), SecretValue::new(value))]),
                BTreeMap::new(),
            )
            .expect_err("invalid environment value");
            assert_eq!(error.path().to_string(), "secrets.env[\"TOKEN\"]");
        }
    }
}
