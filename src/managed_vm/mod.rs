mod guest;
mod host;
mod protocol;
mod staging;

pub use guest::{
    guest_abandon, guest_cancel, guest_discard, guest_file_info, guest_handshake, guest_prepare,
    guest_read_file, guest_read_stream, guest_remove, guest_seal_inputs, guest_start, guest_status,
    guest_stream_info,
};
pub use host::HostVm;

use protocol::{
    ensure_guest_linux, ensure_regular_file, ensure_status, file_identity, guest_binary_path,
    guest_state_path, load_guest_operation, normalize_architecture, operation_file, operation_path,
    parse_runc_identity, parse_slot, parse_systemd_status, pinned_lima_template,
    privileged_file_identity, privileged_file_to_stdout, register_interrupts, rewrite_file_tokens,
    selected_instance_image, set_private_permissions, unit_name, unregister_interrupts,
    validate_file_slot, validate_forwarded_argv, validate_handshake, validate_name,
    validate_reference_profile, validate_runc_identity, write_new_json,
};
use staging::{
    derived_input_path, seal_runtime_config_inputs, sealed_operation_path,
    validate_runtime_config_inputs,
};

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::core::Digest;
use crate::integrity::finish_sha256;

const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_INSTANCE: &str = "runlab";
const GUEST_BINARY_ROOT: &str = "/usr/local/libexec/runlab";
const GUEST_OPERATION_ROOT: &str = "/var/tmp/runlab-operations";
const GUEST_STATE_ROOT: &str = "/var/lib/runlab/namespaces";
const GUEST_SEALED_INPUT_ROOT: &str = "/var/lib/runlab/vm-inputs";
const MAX_CONTROL_OUTPUT: usize = 1024 * 1024;
const MAX_GUEST_STREAM: usize = 64 * 1024 * 1024;
const MAX_FILE_SLOTS: usize = 32;
const MAX_FORWARDED_ARGUMENTS: usize = 256;
const MAX_FORWARDED_ARGUMENT_BYTES: usize = 64 * 1024;
const LIMA_VERSION: &str = "2.2.0";
const RUNC_VERSION: &str = "1.5.1";
const RUNC_COMMIT: &str = "v1.5.1-0-g8f2685a47";
const RUNC_SPEC: &str = "1.3.0";
const CONNTRACK_PACKAGE_VERSION: &str = "1:1.4.8-1ubuntu1";
const SYSCTL_CONFIG_PATH: &str = "/etc/sysctl.d/90-runlab-reference-profile.conf";
const SYSCTL_CONFIG: &[u8] = b"net.ipv4.ip_forward = 1\n";
const MODULES_CONFIG_PATH: &str = "/etc/modules-load.d/90-runlab-reference-profile.conf";
const MODULES_CONFIG: &[u8] = b"overlay\n";

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmHandshake {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub runlab_version: String,
    pub os: String,
    pub architecture: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmStatus {
    pub schema_version: u32,
    pub instance: String,
    pub status: String,
    pub lima_version: String,
    pub vm_type: String,
    pub architecture: String,
    pub plain: bool,
    pub mounts: usize,
    pub image: VmImage,
    pub handshake: Option<VmHandshake>,
    pub handshake_error: Option<String>,
    pub runc: Option<VmRuncIdentity>,
    pub runc_error: Option<String>,
    pub reference_profile: Option<VmReferenceProfile>,
    pub reference_profile_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmImage {
    pub location: String,
    pub digest: Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct VmRuncIdentity {
    pub version: String,
    pub commit: String,
    pub spec: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct VmReferenceProfile {
    pub ready: bool,
    pub tools: VmReferenceTools,
    pub kernel: VmKernelFacts,
    pub systemd: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct VmKernelFacts {
    pub cgroup_version: Option<u8>,
    pub overlayfs: VmKernelFeatureFacts,
    pub ipv4_forwarding: VmKernelFeatureFacts,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct VmKernelFeatureFacts {
    pub active: bool,
    pub configured: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct VmReferenceTools {
    pub ip: VmExecutableFact,
    pub nft: VmExecutableFact,
    pub conntrack: VmExecutableFact,
    pub unshare: VmExecutableFact,
    pub nsenter: VmExecutableFact,
    pub cat: VmExecutableFact,
    pub modprobe: VmExecutableFact,
    pub systemd_run: VmExecutableFact,
    pub systemctl: VmExecutableFact,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct VmExecutableFact {
    pub path: String,
    pub executable: bool,
    pub package_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmInstallResult {
    pub schema_version: u32,
    pub instance: String,
    pub binary: String,
    pub digest: Digest,
    pub size: u64,
    pub handshake: VmHandshake,
    pub runc_binary: String,
    pub runc_digest: Digest,
    pub runc_size: u64,
    pub runc: VmRuncIdentity,
    pub reference_profile: VmReferenceProfile,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmOperationResult {
    pub schema_version: u32,
    pub instance: String,
    pub operation_id: Uuid,
    pub namespace: String,
    pub detached: bool,
    pub runtime_config_inputs: Vec<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmOperationStatus {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub namespace: String,
    pub state: String,
    pub terminal: bool,
    pub exit_code: Option<u8>,
    pub result: Option<String>,
    pub output_count: usize,
    pub runtime_config_inputs: Vec<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmCancelResult {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub signal_sent: bool,
    pub status: VmOperationStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct VmDiscardResult {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub removed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileIdentity {
    pub schema_version: u32,
    pub digest: Digest,
    pub size: u64,
}

#[derive(Debug)]
pub struct AttachedOperation {
    pub operation_id: Uuid,
    pub status: VmOperationStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LimaInstance {
    name: String,
    status: String,
    #[serde(rename = "vmType")]
    vm_type: String,
    arch: String,
    #[serde(rename = "limaVersion")]
    lima_version: String,
    config: LimaConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LimaConfig {
    plain: bool,
    #[serde(default)]
    mounts: Vec<serde_json::Value>,
    images: Vec<LimaImage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LimaImage {
    location: String,
    arch: String,
    digest: Option<Digest>,
    #[serde(default)]
    variant: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GuestOperation {
    schema_version: u32,
    protocol_version: u32,
    runlab_version: String,
    operation_id: Uuid,
    namespace: String,
    input_count: usize,
    input_identities: Vec<FileIdentity>,
    runtime_config_inputs: Vec<usize>,
    output_count: usize,
    argv: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_fixed_guest_roots() {
        for invalid in ["", "../escape", "Upper", "a/b", "_hidden", "a.b"] {
            assert!(
                validate_name("namespace", invalid).is_err(),
                "accepted {invalid}"
            );
        }
        assert!(validate_name("namespace", "agent-1_trial").is_ok());
    }

    #[test]
    fn forwarding_requires_public_commands_and_exact_file_slots() {
        let valid = vec![
            "image".to_owned(),
            "import".to_owned(),
            "@input/0".to_owned(),
            "--name".to_owned(),
            "hello".to_owned(),
        ];
        assert!(validate_forwarded_argv(&valid, 1, 0).is_ok());
        assert!(validate_forwarded_argv(&valid, 2, 0).is_ok());
        assert!(validate_forwarded_argv(&["__internal-vm-handshake".to_owned()], 0, 0).is_err());
        assert!(
            validate_forwarded_argv(
                &[
                    "run".to_owned(),
                    "get".to_owned(),
                    "x".to_owned(),
                    "--state=/tmp/x".to_owned()
                ],
                0,
                0
            )
            .is_err()
        );
        assert!(
            validate_forwarded_argv(
                &[
                    "image".to_owned(),
                    "import".to_owned(),
                    "@input/1".to_owned()
                ],
                1,
                0
            )
            .is_err()
        );
    }

    #[test]
    fn systemd_status_preserves_exit_and_signal_facts() {
        let id = Uuid::now_v7();
        let exited = parse_systemd_status(id, "test", 0, &[], b"LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=17\n").unwrap();
        assert_eq!(exited.exit_code, Some(17));
        let killed = parse_systemd_status(id, "test", 0, &[], b"LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=signal\nExecMainCode=2\nExecMainStatus=2\n").unwrap();
        assert_eq!(killed.exit_code, Some(130));
    }

    #[test]
    fn vm_template_contains_one_digest_pinned_image_without_fallback() {
        let template = pinned_lima_template("aarch64").unwrap();
        let value: serde_json::Value = serde_json::from_str(&template).unwrap();
        let images = value["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["arch"], "aarch64");
        assert_eq!(
            images[0]["digest"],
            "sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc"
        );
        assert!(
            images[0]["location"]
                .as_str()
                .unwrap()
                .contains("release-20260705")
        );
    }

    #[test]
    fn managed_vm_rejects_a_different_image_or_fallback() {
        let expected = VmImage {
            location: "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-arm64.img".to_owned(),
            digest: Digest::parse(
                "sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc",
            )
            .unwrap(),
        };
        let instance = LimaInstance {
            name: "runlab".to_owned(),
            status: "Stopped".to_owned(),
            vm_type: "vz".to_owned(),
            arch: "aarch64".to_owned(),
            lima_version: format!("v{LIMA_VERSION}"),
            config: LimaConfig {
                plain: true,
                mounts: Vec::new(),
                images: vec![LimaImage {
                    location: expected.location.clone(),
                    arch: "aarch64".to_owned(),
                    digest: Some(expected.digest.clone()),
                    variant: "server".to_owned(),
                }],
            },
        };
        assert_eq!(
            selected_instance_image(&instance).unwrap().digest,
            expected.digest
        );

        let mut wrong_digest = instance.clone();
        wrong_digest.config.images[0].digest =
            Some(Digest::parse(format!("sha256:{}", "0".repeat(64))).unwrap());
        assert!(selected_instance_image(&wrong_digest).is_err());

        let mut wrong_location = instance.clone();
        wrong_location.config.images[0].location = "https://example.invalid/image.img".to_owned();
        assert!(selected_instance_image(&wrong_location).is_err());

        let mut fallback = instance;
        let duplicate = fallback.config.images[0].clone();
        fallback.config.images.push(duplicate);
        assert!(selected_instance_image(&fallback).is_err());
    }

    #[test]
    fn forwarding_rewrites_slots_used_as_equals_option_values() {
        let operation_id = Uuid::now_v7();
        let operation = GuestOperation {
            schema_version: 1,
            protocol_version: PROTOCOL_VERSION,
            runlab_version: env!("CARGO_PKG_VERSION").to_owned(),
            operation_id,
            namespace: "test".to_owned(),
            input_count: 2,
            input_identities: Vec::new(),
            runtime_config_inputs: vec![0],
            output_count: 1,
            argv: vec![
                "run".to_owned(),
                "start".to_owned(),
                "image".to_owned(),
                "--runtime-config=@input/0".to_owned(),
                "--managed-service=@input/1".to_owned(),
                "--output=@output/0".to_owned(),
            ],
        };
        validate_forwarded_argv(&operation.argv, 2, 1).unwrap();
        let rewritten = rewrite_file_tokens(&operation.argv, operation_id, &operation, 1).unwrap();
        assert_eq!(
            rewritten[3],
            format!(
                "--runtime-config={}",
                Path::new(GUEST_SEALED_INPUT_ROOT)
                    .join(operation_id.to_string())
                    .join("runtime-config-0.json")
                    .display()
            )
        );
        assert_eq!(
            rewritten[4],
            format!(
                "--managed-service={}",
                Path::new(GUEST_SEALED_INPUT_ROOT)
                    .join(operation_id.to_string())
                    .join("managed-service-1.json")
                    .display()
            )
        );
        assert_eq!(
            rewritten[5],
            format!("--output={}", operation_file(operation_id, "output", 0))
        );
    }

    #[test]
    fn runc_identity_is_exactly_pinned() {
        let identity = parse_runc_identity(
            b"runc version 1.5.1\ncommit: v1.5.1-0-g8f2685a47\nspec: 1.3.0\ngo: go1.25.12\n",
        )
        .unwrap();
        validate_runc_identity(&identity).unwrap();
        let wrong = VmRuncIdentity {
            version: "1.5.0".to_owned(),
            ..identity
        };
        assert!(validate_runc_identity(&wrong).is_err());
    }

    fn executable(path: &str, version: &str) -> VmExecutableFact {
        VmExecutableFact {
            path: path.to_owned(),
            executable: true,
            package_version: Some(version.to_owned()),
        }
    }

    fn ready_reference_profile() -> VmReferenceProfile {
        VmReferenceProfile {
            ready: true,
            tools: VmReferenceTools {
                ip: executable("/usr/sbin/ip", "6.1.0"),
                nft: executable("/usr/sbin/nft", "1.0.9"),
                conntrack: executable("/usr/sbin/conntrack", CONNTRACK_PACKAGE_VERSION),
                unshare: executable("/usr/bin/unshare", "2.39.3"),
                nsenter: executable("/usr/bin/nsenter", "2.39.3"),
                cat: executable("/usr/bin/cat", "9.4"),
                modprobe: executable("/usr/sbin/modprobe", "31"),
                systemd_run: executable("/usr/bin/systemd-run", "255.4"),
                systemctl: executable("/usr/bin/systemctl", "255.4"),
            },
            kernel: VmKernelFacts {
                cgroup_version: Some(2),
                overlayfs: VmKernelFeatureFacts {
                    active: true,
                    configured: true,
                },
                ipv4_forwarding: VmKernelFeatureFacts {
                    active: true,
                    configured: true,
                },
            },
            systemd: true,
        }
    }

    #[test]
    fn reference_profile_requires_exact_conntrack_and_kernel_capabilities() {
        let profile = ready_reference_profile();
        validate_reference_profile(&profile).unwrap();

        let mut wrong_conntrack = profile.clone();
        wrong_conntrack.tools.conntrack.package_version = Some("1:1.4.7-1".to_owned());
        assert!(validate_reference_profile(&wrong_conntrack).is_err());

        let mut no_forwarding = profile;
        no_forwarding.kernel.ipv4_forwarding.active = false;
        assert!(validate_reference_profile(&no_forwarding).is_err());

        let mut no_persistent_overlay = ready_reference_profile();
        no_persistent_overlay.kernel.overlayfs.configured = false;
        assert!(validate_reference_profile(&no_persistent_overlay).is_err());
    }
}
