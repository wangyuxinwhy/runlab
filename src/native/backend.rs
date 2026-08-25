use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub(crate) use super::runc::{
    PreparedRuncRun, RuncCaptureLimits, RuncExecution, RuncOperationErrorKind, RuncRunFailure,
    RuncRunResult, RuncRunner, RuncStopReason, RuncStreamCapture,
};

use crate::core::{
    Architecture, BackendDetails, BackendFacts, ImageView, NativeFilesystemRealization,
    NativeRuntimeConfigRealization, NetworkControl, Platform, RunControls,
};
use crate::filesystem::{FilesystemOwnership, TreeCapture};
use crate::image::ImageService;
use crate::integrity::digest_bytes;
use crate::native::cgroup::PreparedNativeCgroup;
use crate::native::network::{EgressNetworkTools, NativeNetworkTools};
use crate::native::read_only_file::{VerifiedSourceFile, verify_sources};
use crate::native::resolver::ResolverConfig;
use crate::runtime::{RootlessMapping, RuntimeConfig};

const SUPPORTED_RUNC_VERSION: &str = "1.5.1";
const SUPPORTED_RUNC_COMMIT: &str = "v1.5.1-0-g8f2685a47";
const SUPPORTED_RUNC_SPEC: &str = "1.3.0";

/// The path the native resolver bind-mounts over. A bind mount needs a real
/// regular file at the destination, so an Image that ships a symlink there
/// cannot be given egress.
const RESOLVER_TARGET: &[u8] = b"/etc/resolv.conf";

/// Whether this Image can host the Run resolver projection.
pub(crate) fn verify_resolver_target(images: &ImageService, image: &ImageView) -> Result<()> {
    images
        .verify_regular_path_without_symlinks(image, RESOLVER_TARGET)
        .context("native egress resolver target is unsupported")
}

pub(crate) fn verify_file_mount_destinations(
    images: &ImageService,
    image: &ImageView,
    files: &[VerifiedSourceFile],
    participant: Option<&str>,
) -> Result<()> {
    let labels = files
        .iter()
        .map(|file| {
            participant.map_or_else(
                || format!("mounts[{}].destination", file.mount_index()),
                |participant| {
                    format!(
                        "{participant}.runtime_config.mounts[{}].destination",
                        file.mount_index()
                    )
                },
            )
        })
        .collect::<Vec<_>>();
    let paths = files
        .iter()
        .zip(&labels)
        .map(|(file, label)| (file.destination().as_os_str().as_bytes(), label.as_str()))
        .collect::<Vec<_>>();
    images.verify_regular_paths_without_symlinks(image, &paths)
}

/// Whether this Image survives being materialized and re-read under a rootless
/// single-ID mapping. Some ownership and mode combinations cannot be
/// represented, and finding that out during the Run is too late.
pub(crate) fn verify_rootless_image(
    images: &ImageService,
    image: &ImageView,
    state_root: &Path,
    ownership: FilesystemOwnership,
) -> Result<()> {
    let probe = tempfile::Builder::new()
        .prefix("runlab-rootless-image-probe-")
        .tempdir_in(state_root)
        .context("failed to create rootless Image preflight workspace")?;
    let materialized = images.materialize_rootfs_at(
        &image.manifest.digest,
        &probe.path().join("materialize"),
        ownership,
    )?;
    TreeCapture::with_ownership(ownership)
        .capture_inventory(materialized.path())
        .context("rootless Image filesystem profile is unsupported")?;
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct NativeBackend {
    runtime: RuncRunner,
}

#[derive(Debug)]
pub(crate) struct NativePreflight {
    pub facts: BackendFacts,
    pub runner: RuncRunner,
    pub mode: NativeExecutionMode,
    pub realized_runtime: Option<RuntimeConfig>,
    pub capture_limits: RuncCaptureLimits,
    pub primary_files: Vec<VerifiedSourceFile>,
    pub managed_service_files: Vec<VerifiedSourceFile>,
    pub native_network_tools: Option<NativeNetworkTools>,
    pub egress_network_tools: Option<EgressNetworkTools>,
    pub resolver: Option<ResolverConfig>,
}

struct NativePreflightRequest<'a> {
    rootless_runtime: Option<&'a RuntimeConfig>,
    controls: &'a RunControls,
    state_root: &'a Path,
    primary_files: Vec<VerifiedSourceFile>,
    managed_service_files: Vec<VerifiedSourceFile>,
    managed: bool,
    rootless: bool,
}

struct NativeRealization {
    runner: RuncRunner,
    mode: NativeExecutionMode,
    runtime: Option<RuntimeConfig>,
    runtime_fact: NativeRuntimeConfigRealization,
    filesystem_fact: NativeFilesystemRealization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeExecutionMode {
    Rootful,
    Rootless { mapping: RootlessMapping },
}

impl NativeExecutionMode {
    #[must_use]
    pub(crate) const fn ownership(self) -> FilesystemOwnership {
        match self {
            Self::Rootful => FilesystemOwnership::Native,
            Self::Rootless { mapping } => FilesystemOwnership::SingleId {
                host_uid: mapping.host_uid,
                host_gid: mapping.host_gid,
            },
        }
    }

    #[must_use]
    pub(crate) const fn is_rootless(self) -> bool {
        matches!(self, Self::Rootless { .. })
    }
}

impl NativeBackend {
    pub(crate) fn discover(helper_timeout: Duration) -> Result<Self> {
        Ok(Self {
            runtime: RuncRunner::discover(helper_timeout)?,
        })
    }

    pub(crate) fn preflight(
        &self,
        runtime: &RuntimeConfig,
        controls: &RunControls,
        state_root: &Path,
    ) -> Result<NativePreflight> {
        let rootless = !rustix::process::geteuid().is_root();
        let primary_files = if rootless {
            runtime.validate_native_rootless_profile(controls.network)?;
            Vec::new()
        } else {
            runtime.validate_native_run_profile(controls.network)?;
            verify_sources(&runtime.native_file_mounts()?, state_root)?
        };
        self.preflight_host(NativePreflightRequest {
            rootless_runtime: Some(runtime),
            controls,
            state_root,
            primary_files,
            managed_service_files: Vec::new(),
            managed: false,
            rootless,
        })
    }

    pub(crate) fn preflight_managed(
        &self,
        primary_runtime: &RuntimeConfig,
        service_runtime: &RuntimeConfig,
        controls: &RunControls,
        state_root: &Path,
    ) -> Result<NativePreflight> {
        if !rustix::process::geteuid().is_root() {
            bail!("rootless native execution does not support Managed Service");
        }
        primary_runtime.validate_native_managed_profile()?;
        service_runtime.validate_native_managed_profile()?;
        let primary_files = verify_sources(&primary_runtime.native_file_mounts()?, state_root)?;
        let managed_service_files =
            verify_sources(&service_runtime.native_file_mounts()?, state_root)?;
        if primary_files.len() + managed_service_files.len() > 8 {
            bail!("a native Run accepts at most 8 read-only file mounts across all participants");
        }
        self.preflight_host(NativePreflightRequest {
            rootless_runtime: None,
            controls,
            state_root,
            primary_files,
            managed_service_files,
            managed: true,
            rootless: false,
        })
    }

    fn preflight_host(&self, request: NativePreflightRequest<'_>) -> Result<NativePreflight> {
        self.verify_runtime_identity()?;
        let controls = request.controls;
        let native_network_tools = (!request.rootless
            && (request.managed || controls.network == NetworkControl::Egress))
            .then(|| {
                NativeNetworkTools::discover().context("native Run network tools are unavailable")
            })
            .transpose()?;
        let egress_network_tools = (!request.rootless
            && controls.network == NetworkControl::Egress)
            .then(|| {
                let tools = EgressNetworkTools::discover()
                    .context("native egress network tools are unavailable")?;
                tools
                    .preflight(Duration::from_secs(5))
                    .context("native egress network preflight failed")?;
                Ok::<_, anyhow::Error>(tools)
            })
            .transpose()?;
        let resolver = (!request.rootless && controls.network == NetworkControl::Egress)
            .then(|| ResolverConfig::preflight().context("native egress resolver is unavailable"))
            .transpose()?;
        checked_deadline(
            Duration::from_secs(controls.timeout_seconds),
            "native execution timeout",
        )?;
        let capture_limits =
            RuncCaptureLimits::new(controls.stdout_limit_bytes, controls.stderr_limit_bytes)?;
        let realization = self.realize_execution(
            request.rootless_runtime,
            request.state_root,
            request.rootless,
        )?;
        let architecture = match std::env::consts::ARCH {
            "x86_64" => Architecture::Amd64,
            "aarch64" => Architecture::Arm64,
            other => bail!("unsupported native Linux architecture: {other}"),
        };
        let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .context("failed to read Linux kernel release")?
            .trim()
            .to_owned();
        let identity = realization.runner.identity();
        Ok(NativePreflight {
            facts: BackendFacts {
                name: "native_linux".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                platform: Platform::linux(architecture),
                network: controls.network,
                run_network: None,
                details: BackendDetails::NativeLinux {
                    runtime_name: "runc".to_owned(),
                    runtime_version: identity.version.clone(),
                    runtime_commit: identity.commit.clone(),
                    runtime_spec: identity.runtime_spec.clone(),
                    runtime_digest: identity.digest.clone(),
                    runtime_size: identity.size,
                    kernel_release,
                    runtime_invocation: realization.runner.invocation_fact(),
                    runtime_config: realization.runtime_fact,
                    filesystem: realization.filesystem_fact,
                },
            },
            runner: realization.runner,
            mode: realization.mode,
            realized_runtime: realization.runtime,
            capture_limits,
            primary_files: request.primary_files,
            managed_service_files: request.managed_service_files,
            native_network_tools,
            egress_network_tools,
            resolver,
        })
    }

    fn realize_execution(
        &self,
        rootless_runtime: Option<&RuntimeConfig>,
        state_root: &Path,
        rootless: bool,
    ) -> Result<NativeRealization> {
        if !rootless {
            verify_cgroup_v2()?;
            PreparedNativeCgroup::probe().context("native cgroup preflight failed")?;
            crate::native::fs::OverlayRootfs::preflight_at(state_root)?;
            return Ok(NativeRealization {
                runner: self.runtime.clone(),
                mode: NativeExecutionMode::Rootful,
                runtime: None,
                runtime_fact: NativeRuntimeConfigRealization::Accepted,
                filesystem_fact: NativeFilesystemRealization::OverlayFs {
                    profile: "metacopy=off,redirect_dir=nofollow,index=on,nfs_export=off"
                        .to_owned(),
                },
            });
        }

        let mapping = RootlessMapping {
            host_uid: rustix::process::geteuid().as_raw(),
            host_gid: rustix::process::getegid().as_raw(),
        };
        let runner = self
            .runtime
            .probe_rootless_invocation(state_root, mapping)?;
        let runtime = rootless_runtime
            .context("rootless preflight requires a Runtime Config")?
            .realize_rootless(mapping)?;
        let encoded = runtime.encoded()?;
        Ok(NativeRealization {
            runner,
            mode: NativeExecutionMode::Rootless { mapping },
            runtime: Some(runtime),
            runtime_fact: NativeRuntimeConfigRealization::RootlessSingleId {
                digest: digest_bytes(&encoded),
                size: u64::try_from(encoded.len())
                    .context("realized OCI Runtime config size overflow")?,
            },
            filesystem_fact: NativeFilesystemRealization::WritableMaterialized {
                container_uid: 0,
                host_uid: mapping.host_uid,
                container_gid: 0,
                host_gid: mapping.host_gid,
            },
        })
    }

    fn verify_runtime_identity(&self) -> Result<()> {
        let identity = self.runtime.identity();
        if identity.version != SUPPORTED_RUNC_VERSION
            || identity.commit != SUPPORTED_RUNC_COMMIT
            || identity.runtime_spec != SUPPORTED_RUNC_SPEC
        {
            bail!(
                "native execution supports runc {SUPPORTED_RUNC_VERSION} commit {SUPPORTED_RUNC_COMMIT} spec {SUPPORTED_RUNC_SPEC}, received {} commit {} spec {}",
                identity.version,
                identity.commit,
                identity.runtime_spec
            );
        }
        Ok(())
    }
}

fn verify_cgroup_v2() -> Result<()> {
    fs::metadata("/proc/self/ns/cgroup").context("cgroup namespace is unavailable")?;
    fs::metadata("/sys/fs/cgroup/cgroup.controllers")
        .context("the unified cgroup v2 hierarchy is unavailable")?;
    let mounted = crate::native::fs::mounted_as(Path::new("/sys/fs/cgroup"), "cgroup2")?;
    if !mounted {
        bail!("/sys/fs/cgroup is not a cgroup v2 mount");
    }
    Ok(())
}

fn checked_deadline(duration: Duration, name: &str) -> Result<Instant> {
    if duration.is_zero() {
        bail!("{name} must be greater than zero");
    }
    Instant::now()
        .checked_add(duration)
        .with_context(|| format!("{name} is too large"))
}
