use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result as AnyResult, bail};
use run_protocol::{EngineError, ImageDescriptor, InputPath, Network, ProgramId, RunInput};
use rustix::process::geteuid;
use tempfile::TempDir;

use super::budget::OperationBudget;
use super::cgroup::current_cgroup_base;
use super::container_path::{reject_symlink_ancestor, safe_container_path};
use super::network::{EgressPlan, EgressTools};
use super::profile::{validate_host_resources, validate_platform, validate_runtime};
use super::runc::helper_message;
use super::subprocess::{InvocationSupervisor, run_helper};
use crate::oci::{OciSourceCategory, VerifiedImage, inspect_image};
use crate::rootfs::{Rootfs, RootfsError, RootfsErrorKind, RootfsLimits, VerifiedLayer};
use crate::{ContentError, ContentErrorKind, OciContentStore};

pub(super) const MAX_PROGRAMS: usize = 8;
#[allow(
    clippy::duration_suboptimal_units,
    reason = "the declared MSRV lacks Duration::from_hours"
)]
pub(super) const MAX_EXECUTION_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
static INVOCATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn prepare(
    input: &RunInput,
    store: &dyn OciContentStore,
    budget: OperationBudget,
    supervisor: &InvocationSupervisor,
    workspace_root: &Path,
    runc_executable: &Path,
) -> Result<PreparedInvocation, EngineError> {
    Preparation {
        input,
        store,
        budget,
        supervisor,
        workspace_root,
        runc_executable,
    }
    .prepare()
}

struct Preparation<'a> {
    input: &'a RunInput,
    store: &'a dyn OciContentStore,
    budget: OperationBudget,
    supervisor: &'a InvocationSupervisor,
    workspace_root: &'a Path,
    runc_executable: &'a Path,
}

impl Preparation<'_> {
    fn prepare(self) -> Result<PreparedInvocation, EngineError> {
        self.check_budget()?;
        validate_input_capabilities(self.input)?;
        let egress = if self.input.network() == Network::Egress {
            Some(EgressTools::preflight(
                self.supervisor,
                self.budget.deadline(),
            )?)
        } else {
            None
        };
        let workspace_root = validate_private_directory(self.workspace_root, "workspace_root")?;
        let runc = validate_runc(self.runc_executable, self.budget, self.supervisor)?;
        let images = self.inspect_images()?;
        let invocation = create_invocation_workspace(&workspace_root)?;
        let runtime_root = invocation.path().join("runtime");
        create_private_directory(&runtime_root).map_err(|error| {
            EngineError::internal(format!("failed to create private runc root: {error:#}"))
        })?;
        let cgroup_base = current_cgroup_base().map_err(|error| {
            EngineError::internal(format!(
                "failed to establish the Engine-owned default cgroup base: {error:#}"
            ))
        })?;
        let programs = self.prepare_programs(
            &invocation,
            &runtime_root,
            &cgroup_base,
            &images,
            egress.as_ref(),
        )?;

        Ok(PreparedInvocation {
            workspace: Some(invocation.keep()),
            runtime_root,
            runc,
            programs,
            supervisor: self.supervisor.clone(),
        })
    }

    fn inspect_images(&self) -> Result<BTreeMap<ProgramId, VerifiedImage>, EngineError> {
        let mut images = BTreeMap::new();
        for (program_id, program) in self.input.programs() {
            let image =
                inspect_program_image(program_id, self.store, program.initial_environment());
            self.check_budget()?;
            let image = image?;
            validate_platform(program_id, &image)?;
            images.insert(program_id.clone(), image);
            self.check_budget()?;
        }
        Ok(images)
    }

    fn prepare_programs(
        &self,
        invocation: &TempDir,
        runtime_root: &Path,
        cgroup_base: &Path,
        images: &BTreeMap<ProgramId, VerifiedImage>,
        egress: Option<&EgressTools>,
    ) -> Result<BTreeMap<ProgramId, PreparedProgram>, EngineError> {
        let mut programs = BTreeMap::new();
        for (index, (program_id, program)) in self.input.programs().iter().enumerate() {
            let prepared = self.prepare_program(ProgramPreparation {
                index,
                id: program_id,
                input: program,
                image: &images[program_id],
                invocation,
                runtime_root,
                cgroup_base,
                egress,
            })?;
            programs.insert(program_id.clone(), prepared);
            self.check_budget()?;
        }
        Ok(programs)
    }

    fn prepare_program(
        &self,
        program: ProgramPreparation<'_>,
    ) -> Result<PreparedProgram, EngineError> {
        let bundle = program
            .invocation
            .path()
            .join(format!("bundle-{}", program.index));
        create_private_directory(&bundle).map_err(|error| {
            EngineError::internal(format!("failed to create OCI bundle: {error:#}"))
        })?;
        let rootfs = materialize_program_rootfs(program.id, &bundle, program.image, self.store);
        self.check_budget()?;
        let rootfs = rootfs?;
        write_exact_config(&bundle, program.input.runtime_config().as_bytes()).map_err(
            |error| {
                EngineError::internal(format!(
                    "failed to write Program {:?} config.json: {error:#}",
                    program.id
                ))
            },
        )?;
        let artifacts = mount_artifacts(rootfs.path(), program.input.runtime_config().as_json())
            .map_err(|error| {
                EngineError::internal(format!(
                    "failed to inventory Program {:?} mount destinations: {error:#}",
                    program.id
                ))
            })?;
        let suffix = INVOCATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let runtime_id = format!(
            "run-engine-{}-{}-{suffix}",
            std::process::id(),
            program.index
        );
        let pidfd_path = program
            .runtime_root
            .join(format!("{runtime_id}.pidfd.sock"));
        let runc_log_path = program
            .runtime_root
            .join(format!("{runtime_id}.create.log"));
        let expected_cgroup_path = program.cgroup_base.join(&runtime_id);
        ensure_cgroup_absent(&expected_cgroup_path)?;
        preflight_pidfd_socket(&pidfd_path).map_err(|error| {
            EngineError::internal(format!(
                "failed to prepare Program {:?} pidfd socket: {error:#}",
                program.id
            ))
        })?;

        Ok(PreparedProgram {
            bundle,
            runtime_id,
            pidfd_path,
            runc_log_path,
            expected_cgroup_path,
            rootfs,
            parent: program.input.initial_environment().clone(),
            artifacts,
            egress: program
                .egress
                .map(|tools| tools.plan(std::process::id(), suffix)),
        })
    }

    fn check_budget(&self) -> Result<(), EngineError> {
        self.budget
            .check()
            .map_err(|error| EngineError::internal(format!("{error:#}")))
    }
}

#[derive(Clone, Copy)]
struct ProgramPreparation<'a> {
    index: usize,
    id: &'a ProgramId,
    input: &'a run_protocol::ProgramInput,
    image: &'a VerifiedImage,
    invocation: &'a TempDir,
    runtime_root: &'a Path,
    cgroup_base: &'a Path,
    egress: Option<&'a EgressTools>,
}

fn validate_input_capabilities(input: &RunInput) -> Result<(), EngineError> {
    if input.programs().len() > MAX_PROGRAMS {
        return Err(EngineError::unsupported(
            InputPath::field("programs"),
            format!(
                "NativeEngine supports at most {MAX_PROGRAMS} Programs because each Program retains two independent 100 MiB stream prefixes"
            ),
        ));
    }
    if let Some(timeout) = input.execution_timeout_ms() {
        let duration = Duration::from_millis(timeout.get());
        if duration > MAX_EXECUTION_TIMEOUT {
            return Err(EngineError::unsupported(
                InputPath::field("execution_timeout_ms"),
                format!(
                    "NativeEngine supports execution timeouts of at most {} ms",
                    MAX_EXECUTION_TIMEOUT.as_millis()
                ),
            ));
        }
    }
    for (program_id, program) in input.programs() {
        validate_runtime(program_id, program, input.network())?;
        validate_host_resources(program_id, program)?;
    }
    if geteuid().as_raw() != 0 {
        return Err(EngineError::unsupported(
            InputPath::field("programs"),
            "the current NativeEngine profile requires root; rootless OCI semantics have not been proved for this input",
        ));
    }
    Ok(())
}

fn create_invocation_workspace(workspace_root: &Path) -> Result<TempDir, EngineError> {
    let invocation = tempfile::Builder::new()
        .prefix("run-engine-native-")
        .tempdir_in(workspace_root)
        .map_err(|error| {
            EngineError::internal(format!(
                "failed to create private NativeEngine workspace: {error}"
            ))
        })?;
    fs::set_permissions(invocation.path(), fs::Permissions::from_mode(0o700)).map_err(|error| {
        EngineError::internal(format!("failed to protect NativeEngine workspace: {error}"))
    })?;
    Ok(invocation)
}

fn ensure_cgroup_absent(path: &Path) -> Result<(), EngineError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(EngineError::internal(format!(
            "private runtime cgroup already exists: {}",
            path.display()
        ))),
        Err(error) => Err(EngineError::internal(format!(
            "failed to check private runtime cgroup {}: {error}",
            path.display()
        ))),
    }
}

pub(super) struct PreparedInvocation {
    pub(super) workspace: Option<PathBuf>,
    pub(super) runtime_root: PathBuf,
    pub(super) runc: PathBuf,
    pub(super) programs: BTreeMap<ProgramId, PreparedProgram>,
    pub(super) supervisor: InvocationSupervisor,
}

pub(super) struct PreparedProgram {
    pub(super) bundle: PathBuf,
    pub(super) runtime_id: String,
    pub(super) pidfd_path: PathBuf,
    pub(super) runc_log_path: PathBuf,
    pub(super) expected_cgroup_path: PathBuf,
    pub(super) rootfs: Rootfs,
    pub(super) parent: ImageDescriptor,
    pub(super) artifacts: Vec<PathBuf>,
    pub(super) egress: Option<EgressPlan>,
}

fn preflight_pidfd_socket(path: &Path) -> AnyResult<()> {
    if path.as_os_str().as_bytes().len() >= 108 {
        bail!("Unix socket path is too long: {}", path.display());
    }
    let listener = UnixListener::bind(path)?;
    drop(listener);
    fs::remove_file(path)?;
    Ok(())
}

pub(super) fn inspect_program_image(
    id: &ProgramId,
    store: &dyn OciContentStore,
    image: &ImageDescriptor,
) -> Result<VerifiedImage, EngineError> {
    inspect_image(store, image).map_err(|error| map_oci_error(id, &error))
}

pub(super) fn materialize_program_rootfs(
    id: &ProgramId,
    bundle: &Path,
    image: &VerifiedImage,
    store: &dyn OciContentStore,
) -> Result<Rootfs, EngineError> {
    let layers = image
        .layers()
        .iter()
        .map(|layer| VerifiedLayer {
            descriptor: layer.descriptor(),
            expected_diff_id: layer.diff_id(),
        })
        .collect::<Vec<_>>();
    Rootfs::materialize_in(bundle, &layers, RootfsLimits::default(), |descriptor| {
        store.open(descriptor).map_err(anyhow::Error::new)
    })
    .map_err(|error| map_materialize_error(id, &error))
}

fn map_oci_error(id: &ProgramId, error: &crate::oci::OciError) -> EngineError {
    let path = program_path(id).child("initial_environment");
    let reason = error.to_string();
    match error.source_category() {
        OciSourceCategory::InvalidInput => EngineError::invalid(path, reason),
        OciSourceCategory::InputUnavailable => EngineError::input_unavailable(path, reason),
        OciSourceCategory::Unsupported => EngineError::unsupported(path, reason),
        OciSourceCategory::Internal => EngineError::internal(reason),
    }
}

fn map_materialize_error(id: &ProgramId, error: &RootfsError) -> EngineError {
    let reason = format!("failed to materialize Program {id:?}: {error:#}");
    let path = program_path(id).child("initial_environment");
    match error.kind() {
        RootfsErrorKind::InvalidInput => EngineError::invalid(path, reason),
        RootfsErrorKind::UnsupportedInput => EngineError::unsupported(path, reason),
        RootfsErrorKind::Content => {
            let content_kind = error
                .cause()
                .chain()
                .find_map(|source| source.downcast_ref::<ContentError>())
                .map(ContentError::kind);
            match content_kind {
                Some(ContentErrorKind::Unavailable | ContentErrorKind::Rejected) => {
                    EngineError::input_unavailable(path, reason)
                }
                Some(ContentErrorKind::Internal) | None => EngineError::internal(reason),
            }
        }
        RootfsErrorKind::Internal => EngineError::internal(reason),
    }
}

fn program_path(id: &ProgramId) -> InputPath {
    InputPath::field("programs").key(id.as_str())
}

fn validate_private_directory(path: &Path, label: &str) -> Result<PathBuf, EngineError> {
    if !path.is_absolute() {
        return Err(EngineError::internal(format!(
            "{label} must be absolute: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        EngineError::internal(format!(
            "failed to resolve {label} {}: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|error| EngineError::internal(format!("failed to inspect {label}: {error}")))?;
    if !metadata.is_dir() || metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(EngineError::internal(format!(
            "{label} must be an owned private directory with mode 0700 or stricter"
        )));
    }
    Ok(canonical)
}

fn validate_runc(
    path: &Path,
    budget: OperationBudget,
    supervisor: &InvocationSupervisor,
) -> Result<PathBuf, EngineError> {
    if !path.is_absolute() {
        return Err(EngineError::internal(format!(
            "runc executable must be absolute: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        EngineError::internal(format!("failed to resolve runc executable: {error}"))
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        EngineError::internal(format!("failed to inspect runc executable: {error}"))
    })?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(EngineError::internal(
            "runc executable is not an executable regular file",
        ));
    }
    let timeout = budget
        .remaining()
        .map_err(|error| EngineError::internal(format!("{error:#}")))?;
    let output = run_helper(
        supervisor,
        Command::new(&canonical).arg("--version"),
        timeout,
    )
    .map_err(|error| EngineError::internal(format!("runc --version failed: {error:#}")))?;
    if !output.status.success() {
        return Err(EngineError::internal(helper_message(
            "runc --version",
            &output,
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.lines().any(|line| line.trim() == "spec: 1.3.0") {
        return Err(EngineError::internal(
            "runc does not report exact OCI Runtime Specification 1.3.0 support",
        ));
    }
    let timeout = budget
        .remaining()
        .map_err(|error| EngineError::internal(format!("{error:#}")))?;
    let output = run_helper(
        supervisor,
        Command::new(&canonical).args(["create", "--help"]),
        timeout,
    )
    .map_err(|error| EngineError::internal(format!("runc create --help failed: {error:#}")))?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout).contains("--pidfd-socket")
    {
        return Err(EngineError::internal(
            "runc does not expose the required create --pidfd-socket capability",
        ));
    }
    Ok(canonical)
}

pub(super) fn create_private_directory(path: &Path) -> AnyResult<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_exact_config(bundle: &Path, bytes: &[u8]) -> AnyResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(bundle.join("config.json"))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn mount_artifacts(rootfs: &Path, config: &serde_json::Value) -> AnyResult<Vec<PathBuf>> {
    let mut artifacts = BTreeSet::new();
    for mount in config
        .get("mounts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let destination = mount
            .get("destination")
            .and_then(serde_json::Value::as_str)
            .context("OCI mount destination is absent")?;
        let relative = safe_container_path(destination)?;
        reject_symlink_ancestor(rootfs, &relative)?;
        let mut current = PathBuf::new();
        for component in relative.components() {
            current.push(component.as_os_str());
            if fs::symlink_metadata(rootfs.join(&current))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                artifacts.insert(current.clone());
            }
        }
    }
    let mut artifacts = artifacts.into_iter().collect::<Vec<_>>();
    artifacts.sort_by_key(|path| path.components().count());
    Ok(artifacts)
}
