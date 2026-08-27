use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::num::NonZeroU64;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result as AnyResult;
use oci_spec::image::{Descriptor, Digest, ImageIndex, ImageManifest, MediaType};
use run_protocol::{ImageDescriptor, Network, ProgramId, ProgramInput, RunInput, RuntimeConfig};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

use crate::native::prepare::{PreparedInvocation, PreparedProgram, create_private_directory};
use crate::native::subprocess::{HELPER_OUTPUT_LIMIT, InvocationSupervisor};
use crate::oci::inspect_image;
use crate::rootfs::{Rootfs, RootfsLimits, VerifiedLayer};
use crate::{
    ContentError, ContentErrorKind, NativeEngine, OciContent, OciContentStore, OperationTimeouts,
};

pub(super) struct UnavailableStore;

impl OciContentStore for UnavailableStore {
    fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        Err(ContentError::new(
            ContentErrorKind::Unavailable,
            "test content is absent",
        ))
    }

    fn publish(
        &self,
        _descriptor: &Descriptor,
        _content: &mut dyn Read,
    ) -> Result<(), ContentError> {
        Err(ContentError::new(
            ContentErrorKind::Rejected,
            "test store is read-only",
        ))
    }
}

pub(super) fn empty_prepared_invocation(
    workspace: TempDir,
    runc: PathBuf,
    supervisor: InvocationSupervisor,
    runtime_id: &str,
) -> PreparedInvocation {
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private invocation workspace");
    let runtime_root = workspace.path().join("runtime");
    create_private_directory(&runtime_root).expect("runtime root");
    let bundle = workspace.path().join("bundle");
    create_private_directory(&bundle).expect("bundle");
    let rootfs = Rootfs::materialize_in(
        &bundle,
        &[],
        RootfsLimits::default(),
        |_descriptor| -> AnyResult<std::io::Cursor<Vec<u8>>> {
            unreachable!("empty layer set does not open content")
        },
    )
    .expect("empty rootfs");
    let program = PreparedProgram {
        bundle: bundle.clone(),
        runtime_id: runtime_id.to_owned(),
        pidfd_path: bundle.join("pidfd.sock"),
        runc_log_path: bundle.join("runc.log"),
        expected_cgroup_path: Path::new("/sys/fs/cgroup").join(runtime_id),
        rootfs,
        parent: test_image(),
        artifacts: Vec::new(),
        egress: None,
    };
    PreparedInvocation {
        workspace: Some(workspace.keep()),
        runtime_root,
        runc,
        programs: BTreeMap::from([(ProgramId::primary(), program)]),
        supervisor,
    }
}

#[derive(Default)]
pub(super) struct MemoryStore {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl OciContentStore for MemoryStore {
    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        self.blobs
            .lock()
            .expect("store lock")
            .get(&descriptor.digest().to_string())
            .cloned()
            .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn OciContent>)
            .ok_or_else(|| {
                ContentError::new(ContentErrorKind::Unavailable, "test content is absent")
            })
    }

    fn publish(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<(), ContentError> {
        let mut bytes = Vec::new();
        content
            .read_to_end(&mut bytes)
            .map_err(|error| ContentError::new(ContentErrorKind::Internal, error.to_string()))?;
        let actual = descriptor_for_test_bytes(descriptor.media_type().clone(), &bytes);
        if actual.size() != descriptor.size() || actual.digest() != descriptor.digest() {
            return Err(ContentError::new(
                ContentErrorKind::Rejected,
                "published bytes do not match descriptor",
            ));
        }
        self.blobs
            .lock()
            .expect("store lock")
            .insert(descriptor.digest().to_string(), bytes);
        Ok(())
    }
}

pub(super) fn import_oci_layout(store: &MemoryStore, layout: &Path) -> ImageDescriptor {
    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(layout.join("oci-layout")).expect("read OCI layout marker"),
    )
    .expect("parse OCI layout marker");
    assert_eq!(marker["imageLayoutVersion"], "1.0.0");

    let index = ImageIndex::from_file(layout.join("index.json")).expect("parse OCI index");
    assert_eq!(index.schema_version(), 2);
    let [manifest_descriptor] = index.manifests().as_slice() else {
        panic!("NativeEngine E2E OCI layout must contain exactly one manifest");
    };
    assert_eq!(manifest_descriptor.media_type(), &MediaType::ImageManifest);

    let manifest_bytes = read_oci_layout_blob(layout, manifest_descriptor);
    let manifest: ImageManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse OCI image manifest");
    assert_eq!(manifest.schema_version(), 2);
    for descriptor in std::iter::once(manifest.config()).chain(manifest.layers()) {
        publish_test_blob(store, descriptor, read_oci_layout_blob(layout, descriptor));
    }
    publish_test_blob(store, manifest_descriptor, manifest_bytes);
    ImageDescriptor::new(manifest_descriptor.clone()).expect("image descriptor")
}

pub(super) fn read_oci_layout_blob(layout: &Path, descriptor: &Descriptor) -> Vec<u8> {
    let digest = descriptor.digest().to_string();
    let (algorithm, encoded) = digest.split_once(':').expect("qualified OCI digest");
    assert_eq!(
        algorithm, "sha256",
        "NativeEngine test store supports sha256"
    );
    fs::read(layout.join("blobs").join(algorithm).join(encoded)).expect("read OCI layout blob")
}

pub(super) fn publish_test_blob(store: &MemoryStore, descriptor: &Descriptor, bytes: Vec<u8>) {
    store
        .publish(descriptor, &mut std::io::Cursor::new(bytes))
        .expect("publish exact OCI layout blob");
}

pub(super) fn descriptor_for_test_bytes(media_type: MediaType, bytes: &[u8]) -> Descriptor {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("hex write");
    }
    Descriptor::new(
        media_type,
        u64::try_from(bytes.len()).expect("blob size"),
        Digest::try_from(format!("sha256:{encoded}")).expect("digest"),
    )
}

pub(super) fn image_with_platform_field(
    store: &MemoryStore,
    field: &str,
    value: serde_json::Value,
) -> ImageDescriptor {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    let mut config_value = json!({
        "architecture": architecture,
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": []},
        "config": {}
    });
    config_value
        .as_object_mut()
        .expect("config object")
        .insert(field.to_owned(), value);
    let config_bytes = serde_json::to_vec(&config_value).expect("config bytes");
    let config = descriptor_for_test_bytes(MediaType::ImageConfig, &config_bytes);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": &config,
        "layers": []
    }))
    .expect("manifest bytes");
    let manifest = descriptor_for_test_bytes(MediaType::ImageManifest, &manifest_bytes);
    let mut blobs = store.blobs.lock().expect("store lock");
    blobs.insert(config.digest().to_string(), config_bytes);
    blobs.insert(manifest.digest().to_string(), manifest_bytes);
    ImageDescriptor::new(manifest).expect("image descriptor")
}

pub(super) fn e2e_input(
    image: &ImageDescriptor,
    name: &str,
    script: &str,
    stdin: &[u8],
    timeout: Option<NonZeroU64>,
) -> RunInput {
    e2e_input_with_cwd(image, name, script, stdin, timeout, "/")
}

pub(super) fn e2e_input_with_network(
    image: &ImageDescriptor,
    name: &str,
    script: &str,
    network: Network,
) -> RunInput {
    RunInput::new(
        BTreeMap::from([(
            ProgramId::primary(),
            e2e_program(image, name, script, b"", false),
        )]),
        None,
        network,
    )
    .expect("RunInput")
}

pub(super) fn delayed_runc_wrapper(
    real_runc: &Path,
    delayed_operation: &str,
) -> (TempDir, PathBuf) {
    let workspace = tempfile::tempdir().expect("wrapper workspace");
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private wrapper workspace");
    let wrapper = workspace.path().join("runc");
    let quoted_runc = format!("'{}'", real_runc.to_string_lossy().replace('\'', "'\\''"));
    fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\noperation=\nhelp=0\nfor argument in \"$@\"; do\n  case \"$argument\" in create|start) operation=\"$argument\";; --help) help=1;; esac\ndone\nif [ \"$operation\" = {delayed_operation} ] && [ \"$help\" -eq 0 ]; then sleep 1; fi\nexec {quoted_runc} \"$@\"\n"
            ),
        )
        .expect("write runc wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
        .expect("executable runc wrapper");
    (workspace, wrapper)
}

pub(super) fn noisy_runc_wrapper(real_runc: &Path) -> (TempDir, PathBuf) {
    let workspace = tempfile::tempdir().expect("wrapper workspace");
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private wrapper workspace");
    let wrapper = workspace.path().join("runc");
    let quoted_runc = format!("'{}'", real_runc.to_string_lossy().replace('\'', "'\\''"));
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\noperation=\nhelp=0\nfor argument in \"$@\"; do\n  case \"$argument\" in create) operation=create;; --help) help=1;; esac\ndone\nif [ \"$operation\" = create ] && [ \"$help\" -eq 0 ]; then\n  head -c {} /dev/zero >&2\n  sleep 30\nfi\nexec {quoted_runc} \"$@\"\n",
            HELPER_OUTPUT_LIMIT + 1
        ),
    )
    .expect("write runc wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
        .expect("executable runc wrapper");
    (workspace, wrapper)
}

pub(super) fn e2e_input_uncgrouped(
    image: &ImageDescriptor,
    name: &str,
    script: &str,
    stdin: &[u8],
    timeout: Option<NonZeroU64>,
) -> RunInput {
    e2e_input_with_cwd(image, name, script, stdin, timeout, "/")
}

pub(super) fn e2e_input_with_cwd(
    image: &ImageDescriptor,
    name: &str,
    script: &str,
    stdin: &[u8],
    timeout: Option<NonZeroU64>,
    cwd: &str,
) -> RunInput {
    let program = e2e_program_with_options(image, name, script, stdin, cwd, false);
    RunInput::new(
        BTreeMap::from([(ProgramId::primary(), program)]),
        timeout,
        Network::Isolated,
    )
    .expect("RunInput")
}

pub(super) fn e2e_input_with_file_bind(image: &ImageDescriptor, source: &Path) -> RunInput {
    let base = e2e_program_with_options(
        image,
        "file-bind",
        "test \"$(cat /task/input.json)\" = task",
        b"",
        "/",
        false,
    );
    let mut value = base.runtime_config().as_json().clone();
    value
        .pointer_mut("/mounts")
        .and_then(serde_json::Value::as_array_mut)
        .expect("mounts")
        .push(json!({
            "destination": "/task/input.json",
            "source": source,
            "type": "bind",
            "options": ["bind", "ro"]
        }));
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime JSON"))
        .expect("runtime config");
    let program = ProgramInput::new(image.clone(), runtime, Vec::new()).expect("program");
    RunInput::new(
        BTreeMap::from([(ProgramId::primary(), program)]),
        None,
        Network::Isolated,
    )
    .expect("RunInput")
}

pub(super) fn e2e_program(
    image: &ImageDescriptor,
    name: &str,
    script: &str,
    stdin: &[u8],
    _resources: bool,
) -> ProgramInput {
    e2e_program_with_options(image, name, script, stdin, "/", false)
}

pub(super) fn e2e_input_with_invalid_rlimit(
    image: &ImageDescriptor,
    name: &str,
    script: &str,
) -> RunInput {
    let program = e2e_program_with_options(image, name, script, b"", "/", true);
    RunInput::new(
        BTreeMap::from([(ProgramId::primary(), program)]),
        None,
        Network::Isolated,
    )
    .expect("RunInput")
}

pub(super) fn e2e_program_with_options(
    image: &ImageDescriptor,
    _name: &str,
    script: &str,
    stdin: &[u8],
    cwd: &str,
    invalid_rlimit: bool,
) -> ProgramInput {
    let linux = json!({
        "namespaces": [
            {"type": "pid"}, {"type": "network"}, {"type": "ipc"},
            {"type": "uts"}, {"type": "mount"}, {"type": "cgroup"}
        ],
        "resources": {"memory": {"limit": 134_217_728}, "pids": {"limit": 64}}
    });
    let mut value = json!({
        "ociVersion": "1.3.0",
        "root": {"path": "rootfs", "readonly": false},
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": ["/bin/sh", "-c", script],
            "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "cwd": cwd,
            "noNewPrivileges": true,
            "capabilities": {
                "bounding": [], "effective": [], "inheritable": [],
                "permitted": [], "ambient": []
            }
        },
        "hostname": "runlab-e2e",
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc"},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]},
            {"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "noexec", "nodev", "ro"]},
            {"destination": "/runtime-created/nested", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=755", "size=1m"]}
        ],
        "linux": linux
    });
    if invalid_rlimit {
        value["process"]["rlimits"] = json!([{
            "type": "RLIMIT_NOFILE", "soft": 2, "hard": 1
        }]);
    }
    let runtime = RuntimeConfig::parse(serde_json::to_vec(&value).expect("runtime JSON"))
        .expect("runtime config");
    ProgramInput::new(image.clone(), runtime, stdin.to_vec()).expect("program")
}

pub(super) fn assert_final_delta(store: &MemoryStore, image: &ImageDescriptor) {
    let verified = inspect_image(store, image).expect("verify final image");
    let layers = verified
        .layers()
        .iter()
        .map(|layer| VerifiedLayer {
            descriptor: layer.descriptor(),
            expected_diff_id: layer.diff_id(),
        })
        .collect::<Vec<_>>();
    let workspace = tempfile::tempdir().expect("materialize workspace");
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private materialize workspace");
    let rootfs = Rootfs::materialize_in(
        workspace.path(),
        &layers,
        RootfsLimits::default(),
        |descriptor| store.open(descriptor).map_err(anyhow::Error::from),
    )
    .expect("materialize final image");
    assert_eq!(
        fs::read(rootfs.path().join("result/value")).expect("delta"),
        b"delta"
    );
    assert!(rootfs.path().join("result/cgroup").is_file());
    assert!(!rootfs.path().join("runtime-created").exists());
    assert!(!rootfs.path().join("runtime-created/nested").exists());
}

pub(super) fn assert_workspace_empty(path: &Path) {
    assert_eq!(fs::read_dir(path).expect("workspace entries").count(), 0);
}

pub(super) fn engine_cgroups() -> BTreeSet<PathBuf> {
    let root = Path::new("/sys/fs/cgroup");
    let mut pending = vec![root.to_path_buf()];
    let mut matches = BTreeSet::new();
    let mut visited = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read cgroup tree") {
            let entry = entry.expect("cgroup entry");
            if !entry.file_type().expect("cgroup entry type").is_dir() {
                continue;
            }
            visited += 1;
            assert!(visited <= 100_000, "cgroup tree exceeds test bound");
            let path = entry.path();
            if entry.file_name().as_bytes().starts_with(b"run-engine-") {
                matches.insert(path.clone());
            }
            pending.push(path);
        }
    }
    matches
}

pub(super) fn test_engine() -> NativeEngine {
    NativeEngine::new(
        Arc::new(UnavailableStore),
        PathBuf::from("/intentionally-unprobed"),
        PathBuf::from("/intentionally-unprobed/runc"),
        OperationTimeouts::default(),
    )
}

pub(super) fn test_program() -> ProgramInput {
    let runtime = RuntimeConfig::parse(
            br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"terminal":false,"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0},"noNewPrivileges":true,"capabilities":{"bounding":[],"effective":[],"inheritable":[],"permitted":[],"ambient":[]}},"linux":{"namespaces":[{"type":"pid"},{"type":"network"},{"type":"ipc"},{"type":"uts"},{"type":"mount"},{"type":"cgroup"}]}}"#.to_vec(),
        )
        .expect("runtime");
    ProgramInput::new(test_image(), runtime, Vec::new()).expect("program")
}

pub(super) fn test_image() -> ImageDescriptor {
    ImageDescriptor::new(Descriptor::new(
        MediaType::ImageManifest,
        0,
        Digest::try_from("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("digest"),
    ))
    .expect("image descriptor")
}
