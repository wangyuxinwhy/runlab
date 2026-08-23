//! The noun-verb command surface: parse arguments, print one JSON document on
//! stdout, and return an exit status.
//!
//! This layer owns argument shapes and output shapes and nothing else. Lifecycle
//! decisions belong to `execution`, Image decisions to `image`, and durability to
//! `storage`; a handler here reads inputs, calls one of those, and emits the
//! result. Errors go to stderr as plain text so stdout stays machine-readable.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::catalog::{CatalogEntry, ImageSelector};
use crate::core::{
    Architecture, Digest, ImageView, MAX_CAPTURED_STREAM_BYTES, NetworkControl, OciDescriptor,
    Platform, RunId, ServiceName, TcpReadinessCondition,
};
use crate::docker::DockerBackend;
use crate::image::{ImageService, ImageStructureDiff};
use crate::image_ingress::{
    ImageImportResult as IngressImportResult, ImagePullResult as IngressPullResult,
};
use crate::ingress::ImportSourceKind;
use crate::integrity::ensure_private_directory;
use crate::managed_vm::HostVm;
use crate::oci::OciLayout;
use crate::render::FilesystemChange;
use crate::runtime::RuntimeConfig;
use crate::state::StateOperation;
use crate::storage::{RunBytesField, RunDatabase};
use crate::subprocess::{NETWORK_HOLDER_COMMAND, TCP_PROBE_COMMAND};
use crate::topology::ManagedServiceFile;

const DEFAULT_TIMEOUT_SECONDS: u64 = 3600;
const DEFAULT_STREAM_LIMIT_BYTES: u64 = MAX_CAPTURED_STREAM_BYTES;
const MAX_STDIN_BYTES: u64 = 16 * 1024 * 1024;

mod image;
mod inputs;
mod run;
mod schema;
mod vm;

use image::{run_docker, run_image};
use inputs::{check_runtime_config, run_managed_service, run_runtime_config};
use run::{run_run, run_state};
use schema::run_schema;

#[derive(Debug, Parser)]
#[command(
    name = "runlab",
    version,
    about = "Execute OCI Images and preserve immutable Run Records."
)]
struct Cli {
    /// Local OCI Layout and Run database; `vm` rejects host state and requires `--namespace`.
    #[arg(long, global = true, value_name = "DIRECTORY")]
    state: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(flatten)]
    InternalVm(vm::GuestCommand),
    #[command(name = NETWORK_HOLDER_COMMAND, hide = true)]
    InternalNetworkHolder {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        run_id: String,
    },
    #[command(name = TCP_PROBE_COMMAND, hide = true)]
    InternalTcpProbe {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        timeout_milliseconds: u64,
    },
    /// Run Linux `RunLab` in a managed Lima VM without mounting host state.
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
    /// Operate on standard OCI Images in the local OCI Image Layout.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Use the explicit Docker compatibility adapter.
    Docker {
        #[command(subcommand)]
        command: DockerCommand,
    },
    /// Create or check standard OCI Runtime config.json files.
    #[command(name = "runtime-config")]
    RuntimeConfig {
        #[command(subcommand)]
        command: RuntimeConfigCommand,
    },
    /// Validate a bounded Managed Service participant declaration.
    #[command(name = "managed-service")]
    ManagedService {
        #[command(subcommand)]
        command: ManagedServiceCommand,
    },
    /// Execute one OCI Image with one OCI Runtime config.json.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Verify or maintain the complete local `RunLab` state.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    /// Inspect versioned `RunLab` public JSON schemas.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
}

#[derive(Debug, Subcommand)]
enum VmCommand {
    /// Create a same-architecture plain Lima VM with no host mounts.
    Create {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, default_value_t = 4)]
        cpus: u16,
        #[arg(long, default_value_t = 4)]
        memory_gib: u16,
        #[arg(long, default_value_t = 20)]
        disk_gib: u16,
    },
    /// Inspect the VM boundary, guest protocol, runtime, and reference-profile facts.
    Status {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Start and validate an existing plain, unmounted Lima VM.
    Start {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Install exact RunLab/runtime inputs and provision the rootful reference profile.
    Install {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, value_name = "LINUX_RUNLAB")]
        binary: PathBuf,
        #[arg(long, value_name = "LINUX_RUNC")]
        runc: PathBuf,
    },
    /// Execute one `RunLab` command in an isolated guest state namespace.
    Exec {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        namespace: String,
        /// Host file copied to the corresponding @input/N argument.
        #[arg(long, value_name = "HOST_FILE")]
        input: Vec<PathBuf>,
        /// Input slot containing an OCI Runtime Config whose @input/N mount sources are sealed in the guest.
        #[arg(long, value_name = "INDEX")]
        runtime_config_input: Vec<usize>,
        /// New host file copied from the corresponding @output/N argument.
        #[arg(long, value_name = "HOST_FILE")]
        output: Vec<PathBuf>,
        /// Return an operation identity without waiting for completion.
        #[arg(long)]
        detach: bool,
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Inspect, attach to, or cancel a recoverable guest operation.
    Operation {
        #[command(subcommand)]
        command: VmOperationCommand,
    },
}

#[derive(Debug, Subcommand)]
enum VmOperationCommand {
    /// Inspect an operation without changing it.
    Get {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
    },
    /// Wait, retrieve exact streams and declared outputs, then remove transport state.
    Attach {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, value_name = "HOST_FILE")]
        output: Vec<PathBuf>,
    },
    /// Deliver SIGINT through the explicit guest control path.
    Cancel {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
    },
    /// Remove terminal transport state without retrieving streams or outputs.
    Discard {
        operation_id: Uuid,
        #[arg(long)]
        instance: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ImageCommand {
    /// Import and verify one Image from an OCI Layout directory or plain tar archive.
    Import {
        #[arg(value_name = "SOURCE")]
        source: PathBuf,
        /// Exact Linux platform expected from the selected Image Manifest.
        #[arg(long, value_enum)]
        platform: Option<PlatformArg>,
        /// Exact reachable Image Manifest; required when a platform is ambiguous.
        #[arg(long, conflicts_with = "source_reference")]
        manifest: Option<Digest>,
        /// Exact `org.opencontainers.image.ref.name` on the source root index.
        #[arg(long, value_name = "SOURCE_REFERENCE", conflicts_with = "manifest")]
        source_reference: Option<String>,
        /// Local Catalog reference created or moved after complete verification.
        #[arg(long, value_name = "LOCAL_REFERENCE")]
        name: String,
        /// Mutable local Catalog description.
        #[arg(long)]
        description: Option<String>,
    },
    /// Pull and verify one remote OCI Image into the local Catalog.
    Pull {
        remote_reference: String,
        /// Exact Linux platform selected from an OCI Image Index.
        #[arg(long, value_enum)]
        platform: Option<PlatformArg>,
        /// Local Catalog reference; defaults to the remote repository and selector.
        #[arg(long, value_name = "LOCAL_REFERENCE")]
        name: Option<String>,
        /// Mutable local Catalog description.
        #[arg(long)]
        description: Option<String>,
    },
    /// List or resolve entries in the Local Image Catalog.
    Catalog {
        #[command(subcommand)]
        command: ImageCatalogCommand,
    },
    /// Verify and inspect one OCI Image selected by digest or local reference.
    Inspect {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
    },
    /// Compare OCI structure and the resolved filesystem of two Images.
    Diff {
        #[arg(value_name = "FROM_IMAGE")]
        from: ImageSelector,
        #[arg(value_name = "TO_IMAGE")]
        to: ImageSelector,
        /// Maximum filesystem changes returned in one response.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this raw absolute path encoded as lowercase hex.
        #[arg(long, value_name = "PATH_HEX")]
        after_path_hex: Option<String>,
    },
    /// Export the resolved filesystem as a deterministic plain tar archive.
    Export {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Read a regular file from an OCI Image.
    File {
        #[command(subcommand)]
        command: ImageFileCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DockerCommand {
    /// Operate on OCI Images through the Docker compatibility adapter.
    Image {
        #[command(subcommand)]
        command: DockerImageCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DockerImageCommand {
    /// Import one native Linux `DOCKER_IMAGE` as an OCI Image Manifest.
    Import { docker_image: String },
    /// Materialize an OCI Image in the disposable Docker cache.
    Materialize { manifest_digest: Digest },
    /// Author a new OCI Image through an ordinary mutable Docker container.
    Checkout {
        #[command(subcommand)]
        command: CheckoutCommand,
    },
}

#[derive(Debug, Subcommand)]
enum CheckoutCommand {
    /// Start a mutable checkout from `MANIFEST_DIGEST`.
    Create { manifest_digest: Digest },
    /// Commit `CHECKOUT_ID` as a new OCI Image Manifest.
    Commit { checkout_id: String },
}

#[derive(Debug, Subcommand)]
enum ImageFileCommand {
    /// Copy SOURCE from an image digest or local reference to a new `--output`.
    Get {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
        source: String,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ImageCatalogCommand {
    /// List Catalog entries in stable reference order.
    List {
        /// Maximum entries returned in one response.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this local reference.
        #[arg(long, value_name = "LOCAL_REFERENCE")]
        after: Option<String>,
    },
    /// Resolve one local reference and verify its complete OCI Image.
    Show {
        #[arg(value_name = "LOCAL_REFERENCE")]
        reference: String,
    },
    /// Create or move one Catalog reference to an existing verified Manifest.
    Set {
        #[arg(value_name = "LOCAL_REFERENCE")]
        reference: String,
        #[arg(value_name = "MANIFEST_DIGEST")]
        manifest: Digest,
        /// Set the mutable local description.
        #[arg(long, conflicts_with = "clear_description")]
        description: Option<String>,
        /// Remove the mutable local description.
        #[arg(long, conflicts_with = "description")]
        clear_description: bool,
    },
    /// Remove one Catalog reference without deleting OCI content.
    Remove {
        #[arg(value_name = "LOCAL_REFERENCE")]
        reference: String,
    },
}

#[derive(Debug, Subcommand)]
enum RuntimeConfigCommand {
    /// Convert OCI Image defaults into an OCI Runtime config.json.
    Create {
        #[arg(value_name = "IMAGE")]
        image: ImageSelector,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Validate an OCI Runtime config.json without selecting an execution backend.
    Check { path: PathBuf },
}

#[derive(Debug, Subcommand)]
enum ManagedServiceCommand {
    /// Validate the declaration, OCI Image, Runtime Config, and TCP readiness condition.
    Check { path: PathBuf },
}

#[derive(Debug, Subcommand)]
enum RunCommand {
    /// Execute `INITIAL_MANIFEST` with one accepted Runtime Config and Run Controls.
    Start(RunStartArgs),
    /// Read one immutable `RUN_ID` projection from `SQLite`.
    Get { run_id: RunId },
    /// Verify one Run Record, its stored bytes, and all retained OCI Images.
    Verify { run_id: RunId },
    /// List Run Records in descending identity order.
    List {
        /// Maximum records returned in one response.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Continue strictly after this Run identity.
        #[arg(long)]
        after: Option<RunId>,
        /// Restrict results to one persistent lifecycle.
        #[arg(long, value_enum)]
        lifecycle: Option<RunLifecycleArg>,
    },
    /// Compare two Run Records without reading stored stream bytes.
    Diff {
        left: RunId,
        right: RunId,
        /// Maximum field differences returned in one response.
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Reconcile an interrupted native Run without re-executing its process.
    Reconcile(RunReconcileArgs),
    /// Read stdout stored in the `SQLite` Run Record.
    Stdout {
        #[command(subcommand)]
        command: RunBytesCommand,
    },
    /// Read stderr stored in the `SQLite` Run Record.
    Stderr {
        #[command(subcommand)]
        command: RunBytesCommand,
    },
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// Verify Catalog, Run records, all OCI blobs, and complete rooted Image graphs.
    Verify,
    /// Plan or apply garbage collection for unreachable OCI blobs.
    Gc {
        #[command(subcommand)]
        command: StateGcCommand,
    },
}

#[derive(Debug, Subcommand)]
enum StateGcCommand {
    /// Write an inspectable, immutable plan without deleting content.
    Plan {
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Apply exactly one previously written plan after rechecking reachability.
    Apply {
        #[arg(value_name = "PLAN")]
        plan: PathBuf,
    },
}

#[derive(Debug, Args)]
struct RunReconcileArgs {
    #[arg(
        value_name = "RUN_ID",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    run_id: Option<RunId>,

    /// Discover and reconcile one bounded page of accepted Runs and recovery attempts.
    #[arg(long, required_unless_present = "run_id", conflicts_with = "run_id")]
    all: bool,

    /// Maximum candidates processed with `--all`.
    #[arg(
        long,
        value_name = "COUNT",
        requires = "all",
        conflicts_with = "run_id"
    )]
    limit: Option<usize>,

    /// Continue `--all` strictly after this Run identity.
    #[arg(long, requires = "all", conflicts_with = "run_id")]
    after: Option<RunId>,

    /// Report required actions without changing Run or runtime state.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct RunStartArgs {
    #[arg(value_name = "INITIAL_IMAGE")]
    initial_image: ImageSelector,

    /// Execution implementation; native requires Linux and the supported runc identity.
    #[arg(long, value_enum, default_value_t = RunBackendArg::Native)]
    backend: RunBackendArg,

    #[arg(long, value_name = "FILE")]
    runtime_config: PathBuf,

    /// One required sidecar participant; supported by the native backend only.
    #[arg(long, value_name = "FILE")]
    managed_service: Option<PathBuf>,

    /// File whose exact bytes become process stdin; omitted means empty bytes and EOF.
    #[arg(long, value_name = "FILE")]
    stdin: Option<PathBuf>,

    /// Maximum process runtime after start.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
    timeout_seconds: u64,

    /// Maximum persisted stdout prefix.
    #[arg(long, default_value_t = DEFAULT_STREAM_LIMIT_BYTES)]
    stdout_limit_bytes: u64,

    /// Maximum persisted stderr prefix.
    #[arg(long, default_value_t = DEFAULT_STREAM_LIMIT_BYTES)]
    stderr_limit_bytes: u64,

    /// Requested network provisioning; native supports `none|egress`, Docker supports `none`.
    #[arg(long, value_enum, default_value_t = NetworkArg::None)]
    network: NetworkArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RunBackendArg {
    Docker,
    Native,
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "native execution is rejected after portable input validation"
    )
)]
struct LoadedManagedService {
    declaration: ManagedServiceFile,
    image: ImageSelector,
    runtime: RuntimeConfig,
    runtime_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NetworkArg {
    None,
    Egress,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PlatformArg {
    #[value(name = "linux/amd64")]
    LinuxAmd64,
    #[value(name = "linux/arm64")]
    LinuxArm64,
}

impl From<PlatformArg> for Platform {
    fn from(value: PlatformArg) -> Self {
        match value {
            PlatformArg::LinuxAmd64 => Self::linux(Architecture::Amd64),
            PlatformArg::LinuxArm64 => Self::linux(Architecture::Arm64),
        }
    }
}

impl From<NetworkArg> for NetworkControl {
    fn from(value: NetworkArg) -> Self {
        match value {
            NetworkArg::None => Self::None,
            NetworkArg::Egress => Self::Egress,
        }
    }
}

#[derive(Debug, Subcommand)]
enum RunBytesCommand {
    /// Write captured bytes to a new --output file.
    Get {
        run_id: RunId,
        /// Participant whose captured stream is read.
        #[arg(long, value_enum, default_value_t = RunParticipantArg::Primary)]
        participant: RunParticipantArg,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RunParticipantArg {
    Primary,
    #[value(name = "managed-service")]
    ManagedService,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RunLifecycleArg {
    Accepted,
    Terminal,
}

impl RunLifecycleArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RunStream {
    Stdout,
    Stderr,
}

impl RunStream {
    const fn storage_field(self, participant: RunParticipantArg) -> RunBytesField {
        match (participant, self) {
            (RunParticipantArg::Primary, Self::Stdout) => RunBytesField::Stdout,
            (RunParticipantArg::Primary, Self::Stderr) => RunBytesField::Stderr,
            (RunParticipantArg::ManagedService, Self::Stdout) => {
                RunBytesField::ManagedServiceStdout
            }
            (RunParticipantArg::ManagedService, Self::Stderr) => {
                RunBytesField::ManagedServiceStderr
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum SchemaCommand {
    /// List the versioned public JSON schema names.
    List,
    /// Print one public JSON Schema.
    Show {
        #[arg(value_enum, default_value_t = SchemaName::TerminalRunRecord)]
        name: SchemaName,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum SchemaName {
    AcceptedRunRecord,
    TerminalRunRecord,
    RunStartResult,
    RunListResult,
    RunDiffResult,
    RunStreamGetResult,
    RunReconcileResult,
    RunReconcileBatchResult,
    RunVerifyResult,
    ImageOperationResult,
    ImageInspectResult,
    ImageImportResult,
    ImagePullResult,
    ImageCatalogListResult,
    ImageCatalogShowResult,
    ImageCatalogSetResult,
    ImageCatalogRemoveResult,
    ImageDiffResult,
    ImageExportResult,
    ImageFileGetResult,
    DockerImageMaterializeResult,
    DockerImageCheckoutCreateResult,
    RuntimeConfigCreateResult,
    RuntimeConfigCheckResult,
    ManagedServiceCheckResult,
    StateVerifyResult,
    StateGcPlan,
    StateGcPlanResult,
    StateGcApplyResult,
    VmStatus,
    VmInstallResult,
    VmOperationResult,
    VmOperationStatus,
    VmCancelResult,
    VmDiscardResult,
    SchemaListResult,
}

impl SchemaName {
    const ALL: [Self; 36] = [
        Self::AcceptedRunRecord,
        Self::TerminalRunRecord,
        Self::RunStartResult,
        Self::RunListResult,
        Self::RunDiffResult,
        Self::RunStreamGetResult,
        Self::RunReconcileResult,
        Self::RunReconcileBatchResult,
        Self::RunVerifyResult,
        Self::ImageOperationResult,
        Self::ImageInspectResult,
        Self::ImageImportResult,
        Self::ImagePullResult,
        Self::ImageCatalogListResult,
        Self::ImageCatalogShowResult,
        Self::ImageCatalogSetResult,
        Self::ImageCatalogRemoveResult,
        Self::ImageDiffResult,
        Self::ImageExportResult,
        Self::ImageFileGetResult,
        Self::DockerImageMaterializeResult,
        Self::DockerImageCheckoutCreateResult,
        Self::RuntimeConfigCreateResult,
        Self::RuntimeConfigCheckResult,
        Self::ManagedServiceCheckResult,
        Self::StateVerifyResult,
        Self::StateGcPlan,
        Self::StateGcPlanResult,
        Self::StateGcApplyResult,
        Self::VmStatus,
        Self::VmInstallResult,
        Self::VmOperationResult,
        Self::VmOperationStatus,
        Self::VmCancelResult,
        Self::VmDiscardResult,
        Self::SchemaListResult,
    ];
}

#[derive(Debug, Serialize, JsonSchema)]
struct RunListResult {
    schema_version: u32,
    runs: Vec<crate::core::RunRecord>,
    next_after: Option<RunId>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RunDiffResult {
    schema_version: u32,
    left_run_id: RunId,
    right_run_id: RunId,
    equal: bool,
    total_differences: usize,
    truncated: bool,
    differences: Vec<RunFieldDifference>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RunFieldDifference {
    path: String,
    left: RunFieldValue,
    right: RunFieldValue,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "presence", rename_all = "snake_case")]
enum RunFieldValue {
    Missing,
    Value { value: Value },
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageOperationResult {
    schema_version: u32,
    manifest: crate::core::OciDescriptor,
    platform: crate::core::Platform,
    config: crate::core::OciDescriptor,
    layers: Vec<Digest>,
    parent_manifest: Option<Digest>,
    added_layers: Vec<Digest>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageInspectResult {
    manifest: OciDescriptor,
    config: OciDescriptor,
    platform: Platform,
    layers: Vec<OciDescriptor>,
    diff_ids: Vec<Digest>,
    parent_manifest: Option<Digest>,
    added_layers: Vec<Digest>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImagePullResult {
    schema_version: u32,
    remote_reference: String,
    source_index: Option<OciDescriptor>,
    selected_manifest: OciDescriptor,
    platform: Platform,
    downloaded_blobs: u64,
    downloaded_bytes: u64,
    local_reference: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageImportResult {
    schema_version: u32,
    source_kind: ImportSourceKind,
    source_index: OciDescriptor,
    selected_manifest: OciDescriptor,
    platform: Platform,
    verified_blobs: u64,
    verified_bytes: u64,
    local_reference: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CatalogEntryResult {
    reference: String,
    name: String,
    tag: String,
    manifest: OciDescriptor,
    platform: Option<Platform>,
    description: Option<String>,
    source: Option<String>,
    maintainer: Option<String>,
}

impl From<CatalogEntry> for CatalogEntryResult {
    fn from(value: CatalogEntry) -> Self {
        let (name, tag) = value
            .reference
            .rsplit_once(':')
            .expect("Catalog references are normalized with an explicit tag");
        Self {
            reference: value.reference.clone(),
            name: name.to_owned(),
            tag: tag.to_owned(),
            manifest: value.manifest,
            platform: value.platform,
            description: value.metadata.description,
            source: value.metadata.source,
            maintainer: value.metadata.maintainer,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageCatalogListResult {
    schema_version: u32,
    entries: Vec<CatalogEntryResult>,
    next_after: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageCatalogShowResult {
    schema_version: u32,
    entry: CatalogEntryResult,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageCatalogSetResult {
    schema_version: u32,
    changed: bool,
    previous: Option<CatalogEntryResult>,
    entry: CatalogEntryResult,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageCatalogRemoveResult {
    schema_version: u32,
    reference: String,
    removed: bool,
    previous: Option<CatalogEntryResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ResolvedImageResult {
    requested_reference: Option<String>,
    manifest: OciDescriptor,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageFilesystemDiffResult {
    total_changes: usize,
    changes: Vec<FilesystemChange>,
    next_after_path_hex: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageDiffResult {
    schema_version: u32,
    from: ResolvedImageResult,
    to: ResolvedImageResult,
    structure: ImageStructureDiff,
    filesystem: ImageFilesystemDiffResult,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ImageExportFormat {
    Tar,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageExportResult {
    schema_version: u32,
    requested_reference: Option<String>,
    manifest_digest: Digest,
    output: String,
    digest: Digest,
    size: u64,
    format: ImageExportFormat,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ImageFileGetResult {
    schema_version: u32,
    requested_reference: Option<String>,
    manifest_digest: Digest,
    source: String,
    output: String,
    digest: Digest,
    size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DockerImageMaterializeResult {
    schema_version: u32,
    manifest_digest: Digest,
    docker_image: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DockerImageCheckoutCreateResult {
    schema_version: u32,
    checkout_id: String,
    parent_manifest: Digest,
    exec_argv: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RuntimeConfigCreateResult {
    schema_version: u32,
    requested_reference: Option<String>,
    manifest_digest: Digest,
    output: String,
    size: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RuntimeConfigCheckResult {
    schema_version: u32,
    valid: bool,
    oci_version: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ContentSummary {
    digest: Digest,
    size: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ManagedServiceCheckResult {
    schema_version: u32,
    valid: bool,
    name: ServiceName,
    requested_reference: Option<String>,
    initial_image: OciDescriptor,
    runtime_config: ContentSummary,
    readiness: TcpReadinessCondition,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RunStreamGetResult {
    schema_version: u32,
    run_id: RunId,
    participant: RunParticipantArg,
    field: RunStream,
    output: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SchemaListResult {
    schema_version: u32,
    schemas: Vec<SchemaName>,
}

impl From<IngressImportResult> for ImageImportResult {
    fn from(value: IngressImportResult) -> Self {
        Self {
            schema_version: 1,
            source_kind: value.source_kind,
            source_index: value.source_index,
            selected_manifest: value.selected_manifest,
            platform: value.platform,
            verified_blobs: value.verified_blobs,
            verified_bytes: value.verified_bytes,
            local_reference: value.local_reference,
        }
    }
}

impl From<IngressPullResult> for ImagePullResult {
    fn from(value: IngressPullResult) -> Self {
        Self {
            schema_version: 1,
            remote_reference: value.remote_reference,
            source_index: value.source_index,
            selected_manifest: value.selected_manifest,
            platform: value.platform,
            downloaded_blobs: value.downloaded_blobs,
            downloaded_bytes: value.downloaded_bytes,
            local_reference: value.local_reference,
        }
    }
}

impl From<ImageView> for ImageInspectResult {
    fn from(value: ImageView) -> Self {
        Self {
            manifest: value.manifest,
            config: value.config,
            platform: value.platform,
            layers: value.layers,
            diff_ids: value.diff_ids,
            parent_manifest: value.parent_manifest,
            added_layers: value.added_layers,
        }
    }
}

impl From<ImageView> for ImageOperationResult {
    fn from(value: ImageView) -> Self {
        Self {
            schema_version: 1,
            manifest: value.manifest,
            platform: value.platform,
            config: value.config,
            layers: value.layers.into_iter().map(|layer| layer.digest).collect(),
            parent_manifest: value.parent_manifest,
            added_layers: value.added_layers,
        }
    }
}

pub fn run() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        Command::InternalNetworkHolder { directory, run_id } => {
            run_internal_network_holder(&directory, &run_id)
        }
        Command::InternalTcpProbe {
            port,
            timeout_milliseconds,
        } => run_internal_tcp_probe(port, timeout_milliseconds),
        Command::InternalVm(command) => vm::run_guest(command),
        Command::Vm { command } => run_vm(cli.state.as_ref(), command),
        Command::Image { command } => run_image(&resolve_state(cli.state)?, command),
        Command::Docker { command } => run_docker_with_state(cli.state, command),
        Command::RuntimeConfig {
            command: RuntimeConfigCommand::Check { path },
        } => check_runtime_config(&path),
        Command::RuntimeConfig { command } => {
            with_existing_state(cli.state, |state| run_runtime_config(state, command))
        }
        Command::ManagedService { command } => {
            with_existing_state(cli.state, |state| run_managed_service(state, command))
        }
        Command::Run { command } => run_run(&resolve_state(cli.state)?, command),
        Command::State { command } => run_state(&resolve_state(cli.state)?, command),
        Command::Schema { command } => run_schema(command),
    }
}

fn run_vm(state: Option<&PathBuf>, command: VmCommand) -> Result<u8> {
    ensure_vm_owns_state(state)?;
    match command {
        VmCommand::Create {
            instance,
            cpus,
            memory_gib,
            disk_gib,
        } => {
            emit(&HostVm::new(instance.as_deref())?.create(cpus, memory_gib, disk_gib)?)?;
            Ok(0)
        }
        VmCommand::Status { instance } => {
            emit(&HostVm::new(instance.as_deref())?.status()?)?;
            Ok(0)
        }
        VmCommand::Start { instance } => {
            emit(&HostVm::new(instance.as_deref())?.start()?)?;
            Ok(0)
        }
        VmCommand::Install {
            instance,
            binary,
            runc,
        } => {
            emit(&HostVm::new(instance.as_deref())?.install(&binary, &runc)?)?;
            Ok(0)
        }
        VmCommand::Exec {
            instance,
            namespace,
            input,
            runtime_config_input,
            output,
            detach,
            argv,
        } => {
            let (started, attached) = HostVm::new(instance.as_deref())?.execute(
                &namespace,
                &input,
                &runtime_config_input,
                &output,
                &argv,
                detach,
            )?;
            let Some(attached) = attached else {
                emit(&started)?;
                return Ok(0);
            };
            std::io::stdout().lock().write_all(&attached.stdout)?;
            std::io::stderr().lock().write_all(&attached.stderr)?;
            let exit_code = attached.status.exit_code.unwrap_or(1);
            HostVm::new(instance.as_deref())?.complete(attached.operation_id)?;
            Ok(exit_code)
        }
        VmCommand::Operation { command } => match command {
            VmOperationCommand::Get {
                operation_id,
                instance,
            } => {
                emit(&HostVm::new(instance.as_deref())?.operation_status(operation_id)?)?;
                Ok(0)
            }
            VmOperationCommand::Attach {
                operation_id,
                instance,
                output,
            } => {
                let attached = HostVm::new(instance.as_deref())?.attach(operation_id, &output)?;
                std::io::stdout().lock().write_all(&attached.stdout)?;
                std::io::stderr().lock().write_all(&attached.stderr)?;
                let exit_code = attached.status.exit_code.unwrap_or(1);
                HostVm::new(instance.as_deref())?.complete(attached.operation_id)?;
                Ok(exit_code)
            }
            VmOperationCommand::Cancel {
                operation_id,
                instance,
            } => {
                emit(&HostVm::new(instance.as_deref())?.cancel(operation_id)?)?;
                Ok(0)
            }
            VmOperationCommand::Discard {
                operation_id,
                instance,
            } => {
                emit(&HostVm::new(instance.as_deref())?.discard(operation_id)?)?;
                Ok(0)
            }
        },
    }
}

fn ensure_vm_owns_state(state: Option<&PathBuf>) -> Result<()> {
    if state.is_some() {
        bail!("vm commands do not accept host --state; use --namespace for guest state")
    }
    Ok(())
}

fn with_existing_state(
    explicit: Option<PathBuf>,
    operation: impl FnOnce(&Path) -> Result<u8>,
) -> Result<u8> {
    let state = resolve_state(explicit)?;
    let _operation = StateOperation::enter_existing(&state)?;
    operation(&state)
}

fn run_docker_with_state(explicit: Option<PathBuf>, command: DockerCommand) -> Result<u8> {
    let state = resolve_state(explicit)?;
    let imports_image = matches!(
        command,
        DockerCommand::Image {
            command: DockerImageCommand::Import { .. }
        }
    );
    let _operation = if imports_image {
        StateOperation::enter(&state)?
    } else {
        StateOperation::enter_existing(&state)?
    };
    run_docker(&state, command)
}

#[cfg(target_os = "linux")]
fn run_internal_network_holder(directory: &Path, run_id: &str) -> Result<u8> {
    let run_id = RunId::parse(run_id).context("internal network holder Run identity is invalid")?;
    crate::native::network::hold_network_namespace(directory, run_id)
        .context("internal network holder failed")?;
    Ok(0)
}

#[cfg(not(target_os = "linux"))]
fn run_internal_network_holder(_directory: &Path, _run_id: &str) -> Result<u8> {
    bail!("internal network holder requires Linux")
}

#[cfg(target_os = "linux")]
fn run_internal_tcp_probe(port: u16, timeout_milliseconds: u64) -> Result<u8> {
    use std::io::ErrorKind;

    match crate::native::network::connect_loopback_tcp(
        port,
        std::time::Duration::from_millis(timeout_milliseconds),
    ) {
        Ok(()) => Ok(0),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::TimedOut | ErrorKind::WouldBlock
            ) =>
        {
            Ok(75)
        }
        Err(error) => Err(error).context("internal TCP readiness probe failed"),
    }
}

#[cfg(not(target_os = "linux"))]
fn run_internal_tcp_probe(_port: u16, _timeout_milliseconds: u64) -> Result<u8> {
    bail!("internal TCP readiness probe requires Linux")
}

fn resolve_state(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = env::var_os("RUNLAB_STATE") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("runlab"));
    }
    let home = env::var_os("HOME").context("HOME is not set and no --state was supplied")?;
    Ok(PathBuf::from(home).join(".local/share/runlab"))
}

fn image_service(state: &Path) -> Result<ImageService> {
    ensure_private_directory(state)?;
    Ok(ImageService::new(OciLayout::open(state.join("oci"))?))
}

fn run_database(state: &Path) -> Result<RunDatabase> {
    ensure_private_directory(state)?;
    RunDatabase::open(state.join("runs.sqlite3"))
}

fn local_docker() -> Result<DockerBackend> {
    let docker = DockerBackend::discover()?;
    docker.preflight(NetworkControl::None)?;
    Ok(docker)
}

fn emit(value: &impl Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value).context("failed to serialize JSON output")?;
    writeln!(lock).context("failed to write JSON output")
}

fn absolute_path(path: &Path) -> Result<String> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}
