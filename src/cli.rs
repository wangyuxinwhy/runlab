use std::collections::BTreeSet;
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::backend::DockerBackend;
use crate::catalog::{
    CatalogDescriptionUpdate, CatalogEntry, ImageSelector, LocalImageCatalog, normalize_reference,
};
use crate::core::{
    AcceptedRunRecord, Architecture, Digest, ImageView, MAX_CAPTURED_STREAM_BYTES, NetworkControl,
    OciDescriptor, Platform, RunControls, RunId, ServiceName, StoredBytes, TcpReadinessCondition,
    TerminalRunRecord,
};
#[cfg(target_os = "linux")]
use crate::execution::{ManagedPrimaryInput, ManagedServiceInput};
use crate::execution::{RunReconcileBatchResult, RunReconcileResult, RunStartResult, Runner};
use crate::image::{ImageService, ImageStructureDiff};
use crate::ingress::ImportSourceKind;
use crate::integrity::{digest_bytes, ensure_private_directory, write_new_output};
use crate::maintenance::{
    RunVerifyResult, StateGcApplyResult, StateGcPlan, StateGcPlanResult, StateVerifyResult,
};
use crate::managed_vm::HostVm;
use crate::oci::OciLayout;
use crate::render::FilesystemChange;
use crate::runtime::RuntimeConfig;
use crate::state::{StateMaintenance, StateOperation};
use crate::storage::{RunBytesField, RunDatabase};
use crate::topology::ManagedServiceFile;

const DEFAULT_TIMEOUT_SECONDS: u64 = 3600;
const DEFAULT_STREAM_LIMIT_BYTES: u64 = MAX_CAPTURED_STREAM_BYTES;
const MAX_STDIN_BYTES: u64 = 16 * 1024 * 1024;

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
    #[command(name = "__internal-network-holder", hide = true)]
    InternalNetworkHolder {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        run_id: String,
    },
    #[command(name = "__internal-tcp-probe", hide = true)]
    InternalTcpProbe {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        timeout_milliseconds: u64,
    },
    #[command(name = "__internal-vm-handshake", hide = true)]
    InternalVmHandshake,
    #[command(name = "__internal-vm-prepare", hide = true)]
    InternalVmPrepare {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        input_identities: String,
        #[arg(long)]
        runtime_config_inputs: String,
        #[arg(long)]
        output_count: usize,
        #[arg(last = true)]
        argv: Vec<String>,
    },
    #[command(name = "__internal-vm-seal-inputs", hide = true)]
    InternalVmSealInputs {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = "__internal-vm-start", hide = true)]
    InternalVmStart {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = "__internal-vm-status", hide = true)]
    InternalVmStatus {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = "__internal-vm-cancel", hide = true)]
    InternalVmCancel {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = "__internal-vm-discard", hide = true)]
    InternalVmDiscard {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = "__internal-vm-file-info", hide = true)]
    InternalVmFileInfo {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        index: usize,
    },
    #[command(name = "__internal-vm-read-file", hide = true)]
    InternalVmReadFile {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        index: usize,
    },
    #[command(name = "__internal-vm-read-stream", hide = true)]
    InternalVmReadStream {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        stream: String,
    },
    #[command(name = "__internal-vm-stream-info", hide = true)]
    InternalVmStreamInfo {
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        stream: String,
    },
    #[command(name = "__internal-vm-remove", hide = true)]
    InternalVmRemove {
        #[arg(long)]
        operation_id: Uuid,
    },
    #[command(name = "__internal-vm-abandon", hide = true)]
    InternalVmAbandon {
        #[arg(long)]
        operation_id: Uuid,
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

impl From<crate::image::ImageImportResult> for ImageImportResult {
    fn from(value: crate::image::ImageImportResult) -> Self {
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

impl From<crate::image::ImagePullResult> for ImagePullResult {
    fn from(value: crate::image::ImagePullResult) -> Self {
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
        Command::InternalVmHandshake => emit(&crate::managed_vm::guest_handshake()).map(|()| 0),
        Command::InternalVmPrepare {
            operation_id,
            namespace,
            input_identities,
            runtime_config_inputs,
            output_count,
            argv,
        } => run_internal_vm_prepare(
            operation_id,
            &namespace,
            &input_identities,
            &runtime_config_inputs,
            output_count,
            argv,
        ),
        Command::InternalVmSealInputs { operation_id } => {
            crate::managed_vm::guest_seal_inputs(operation_id).map(|()| 0)
        }
        Command::InternalVmStart { operation_id } => {
            crate::managed_vm::guest_start(operation_id).map(|()| 0)
        }
        Command::InternalVmStatus { operation_id } => {
            emit(&crate::managed_vm::guest_status(operation_id)?)?;
            Ok(0)
        }
        Command::InternalVmCancel { operation_id } => {
            emit(&crate::managed_vm::guest_cancel(operation_id)?)?;
            Ok(0)
        }
        Command::InternalVmDiscard { operation_id } => {
            emit(&crate::managed_vm::guest_discard(operation_id)?)?;
            Ok(0)
        }
        Command::InternalVmFileInfo {
            operation_id,
            kind,
            index,
        } => {
            emit(&crate::managed_vm::guest_file_info(
                operation_id,
                &kind,
                index,
            )?)?;
            Ok(0)
        }
        Command::InternalVmReadFile {
            operation_id,
            kind,
            index,
        } => {
            crate::managed_vm::guest_read_file(operation_id, &kind, index)?;
            Ok(0)
        }
        Command::InternalVmReadStream {
            operation_id,
            stream,
        } => {
            crate::managed_vm::guest_read_stream(operation_id, &stream)?;
            Ok(0)
        }
        Command::InternalVmStreamInfo {
            operation_id,
            stream,
        } => run_internal_vm_stream_info(operation_id, &stream),
        Command::InternalVmRemove { operation_id } => {
            crate::managed_vm::guest_remove(operation_id)?;
            Ok(0)
        }
        Command::InternalVmAbandon { operation_id } => {
            crate::managed_vm::guest_abandon(operation_id)?;
            Ok(0)
        }
        Command::Vm { command } => run_vm(cli.state.as_ref(), command),
        Command::Image { command } => run_image(&resolve_state(cli.state)?, command),
        Command::Docker { command } => with_state(cli.state, |state| run_docker(state, command)),
        Command::RuntimeConfig {
            command: RuntimeConfigCommand::Check { path },
        } => check_runtime_config(&path),
        Command::RuntimeConfig { command } => {
            with_state(cli.state, |state| run_runtime_config(state, command))
        }
        Command::ManagedService { command } => {
            with_state(cli.state, |state| run_managed_service(state, command))
        }
        Command::Run { command } => run_run(&resolve_state(cli.state)?, command),
        Command::State { command } => run_state(&resolve_state(cli.state)?, command),
        Command::Schema { command } => run_schema(command),
    }
}

fn run_internal_vm_prepare(
    operation_id: Uuid,
    namespace: &str,
    input_identities: &str,
    runtime_config_inputs: &str,
    output_count: usize,
    argv: Vec<String>,
) -> Result<u8> {
    crate::managed_vm::guest_prepare(
        operation_id,
        namespace,
        serde_json::from_str(input_identities).context("invalid VM input identities")?,
        serde_json::from_str(runtime_config_inputs)
            .context("invalid VM Runtime Config input slots")?,
        output_count,
        argv,
    )?;
    Ok(0)
}

fn run_internal_vm_stream_info(operation_id: Uuid, stream: &str) -> Result<u8> {
    emit(&crate::managed_vm::guest_stream_info(operation_id, stream)?)?;
    Ok(0)
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

fn with_state(
    explicit: Option<PathBuf>,
    operation: impl FnOnce(&Path) -> Result<u8>,
) -> Result<u8> {
    let state = resolve_state(explicit)?;
    let _operation = StateOperation::enter(&state)?;
    operation(&state)
}

#[cfg(target_os = "linux")]
fn run_internal_network_holder(directory: &Path, run_id: &str) -> Result<u8> {
    let run_id = RunId::parse(run_id).context("internal network holder Run identity is invalid")?;
    crate::native_network::hold_network_namespace(directory, run_id)
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

    match crate::native_network::connect_loopback_tcp(
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

fn run_image(state: &Path, command: ImageCommand) -> Result<u8> {
    if let ImageCommand::Import { source, .. } = &command {
        crate::ingress::validate_source_destination(source, &state.join("oci"))?;
    }
    let _operation = StateOperation::enter(state)?;
    match command {
        ImageCommand::Import {
            source,
            platform,
            manifest,
            source_reference,
            name,
            description,
        } => {
            let platform = match platform {
                Some(platform) => platform.into(),
                None => host_platform()?,
            };
            let result = image_service(state)?.import_oci(
                &source,
                platform,
                manifest.as_ref(),
                source_reference.as_deref(),
                &name,
                description.as_deref(),
            )?;
            emit(&ImageImportResult::from(result))?;
        }
        ImageCommand::Pull {
            remote_reference,
            platform,
            name,
            description,
        } => {
            let platform = match platform {
                Some(platform) => platform.into(),
                None => host_platform()?,
            };
            let result = image_service(state)?.pull_image(
                &remote_reference,
                platform,
                name.as_deref(),
                description.as_deref(),
            )?;
            emit(&ImagePullResult::from(result))?;
        }
        ImageCommand::Catalog { command } => run_image_catalog(state, command)?,
        ImageCommand::Inspect { image } => {
            let images = image_service(state)?;
            let (image, _) = resolve_image(state, &images, &image)?;
            emit(&ImageInspectResult::from(image))?;
        }
        ImageCommand::Diff {
            from,
            to,
            limit,
            after_path_hex,
        } => run_image_diff(state, &from, &to, limit, after_path_hex.as_deref())?,
        ImageCommand::Export { image, output } => {
            let images = image_service(state)?;
            let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
            let manifest_digest = resolved.manifest.digest;
            let (digest, size) = images.export_tar(&manifest_digest, &output)?;
            emit(&ImageExportResult {
                schema_version: 1,
                requested_reference,
                manifest_digest,
                output: absolute_path(&output)?,
                digest,
                size,
                format: ImageExportFormat::Tar,
            })?;
        }
        ImageCommand::File { command } => match command {
            ImageFileCommand::Get {
                image,
                source,
                output,
            } => {
                let images = image_service(state)?;
                let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
                let manifest_digest = resolved.manifest.digest;
                let (digest, size) = images.get_file(&manifest_digest, &source, &output)?;
                emit(&ImageFileGetResult {
                    schema_version: 1,
                    requested_reference,
                    manifest_digest,
                    source,
                    output: absolute_path(&output)?,
                    digest,
                    size,
                })?;
            }
        },
    }
    Ok(0)
}

fn run_docker(state: &Path, command: DockerCommand) -> Result<u8> {
    match command {
        DockerCommand::Image { command } => run_docker_image(state, command),
    }
}

fn run_docker_image(state: &Path, command: DockerImageCommand) -> Result<u8> {
    let images = image_service(state)?;
    let docker = local_docker()?;
    match command {
        DockerImageCommand::Import { docker_image } => emit(&ImageOperationResult::from(
            images.import_image(&docker, &docker_image)?,
        ))?,
        DockerImageCommand::Materialize { manifest_digest } => {
            let docker_image = images.materialize(&docker, &manifest_digest)?;
            emit(&DockerImageMaterializeResult {
                schema_version: 1,
                manifest_digest,
                docker_image,
            })?;
        }
        DockerImageCommand::Checkout { command } => match command {
            CheckoutCommand::Create { manifest_digest } => {
                let (container, parent) = images.create_checkout(&docker, &manifest_digest)?;
                emit(&DockerImageCheckoutCreateResult {
                    schema_version: 1,
                    checkout_id: container.clone(),
                    parent_manifest: parent,
                    exec_argv: vec![
                        "docker".to_owned(),
                        "exec".to_owned(),
                        "-it".to_owned(),
                        container,
                        "/bin/sh".to_owned(),
                    ],
                })?;
            }
            CheckoutCommand::Commit { checkout_id } => emit(&ImageOperationResult::from(
                images.freeze_checkout(&docker, &checkout_id)?,
            ))?,
        },
    }
    Ok(0)
}

fn run_image_catalog(state: &Path, command: ImageCatalogCommand) -> Result<()> {
    let layout = catalog_layout(state)?;
    let catalog = LocalImageCatalog::new(&layout);
    match command {
        ImageCatalogCommand::List { limit, after } => {
            if !(1..=1000).contains(&limit) {
                bail!("--limit must be between 1 and 1000");
            }
            let after = after.as_deref().map(normalize_reference).transpose()?;
            let mut entries = catalog
                .list()?
                .into_iter()
                .filter(|entry| {
                    after
                        .as_ref()
                        .is_none_or(|after| entry.reference.as_str() > after.as_str())
                })
                .take(limit + 1)
                .collect::<Vec<_>>();
            let has_more = entries.len() > limit;
            if has_more {
                entries.truncate(limit);
            }
            let next_after = has_more
                .then(|| entries.last().map(|entry| entry.reference.clone()))
                .flatten();
            emit(&ImageCatalogListResult {
                schema_version: 1,
                entries: entries.into_iter().map(Into::into).collect(),
                next_after,
            })?;
        }
        ImageCatalogCommand::Show { reference } => {
            let reference = normalize_reference(&reference)?;
            let entry = catalog
                .resolve(&reference)?
                .with_context(|| format!("local OCI reference is unknown: {reference}"))?;
            let image = image_service(state)?.inspect(&entry.manifest.digest)?;
            if image.manifest != entry.manifest {
                bail!("Catalog descriptor does not match resolved OCI Manifest: {reference}");
            }
            emit(&ImageCatalogShowResult {
                schema_version: 1,
                entry: entry.into(),
            })?;
        }
        ImageCatalogCommand::Set {
            reference,
            manifest,
            description,
            clear_description,
        } => {
            let reference = normalize_reference(&reference)?;
            let image = image_service(state)?.inspect(&manifest)?;
            let description = match (clear_description, description.as_deref()) {
                (true, _) => CatalogDescriptionUpdate::Clear,
                (false, Some(description)) => CatalogDescriptionUpdate::Set(description),
                (false, None) => CatalogDescriptionUpdate::Preserve,
            };
            let update = catalog.set(&reference, &image.manifest, image.platform, description)?;
            emit(&ImageCatalogSetResult {
                schema_version: 1,
                changed: update.changed,
                previous: update.previous.map(Into::into),
                entry: update.entry.into(),
            })?;
        }
        ImageCatalogCommand::Remove { reference } => {
            let reference = normalize_reference(&reference)?;
            let previous = catalog.remove(&reference)?;
            emit(&ImageCatalogRemoveResult {
                schema_version: 1,
                reference,
                removed: previous.is_some(),
                previous: previous.map(Into::into),
            })?;
        }
    }
    Ok(())
}

fn run_image_diff(
    state: &Path,
    from: &ImageSelector,
    to: &ImageSelector,
    limit: usize,
    after_path_hex: Option<&str>,
) -> Result<()> {
    if !(1..=1000).contains(&limit) {
        bail!("--limit must be between 1 and 1000");
    }
    let after_path_hex = after_path_hex.map(validate_path_hex_cursor).transpose()?;
    let images = image_service(state)?;
    let (from, from_requested_reference) = resolve_image(state, &images, from)?;
    let (to, to_requested_reference) = resolve_image(state, &images, to)?;
    let diff = images.diff(&from.manifest.digest, &to.manifest.digest)?;
    let total_changes = diff.filesystem.changes.len();
    let mut changes = diff
        .filesystem
        .changes
        .into_iter()
        .filter(|change| {
            after_path_hex
                .as_ref()
                .is_none_or(|after| change.path_hex.as_str() > after.as_str())
        })
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = changes.len() > limit;
    if has_more {
        changes.truncate(limit);
    }
    let next_after_path_hex = has_more
        .then(|| changes.last().map(|change| change.path_hex.clone()))
        .flatten();
    emit(&ImageDiffResult {
        schema_version: diff.schema_version,
        from: ResolvedImageResult {
            requested_reference: from_requested_reference,
            manifest: diff.from,
        },
        to: ResolvedImageResult {
            requested_reference: to_requested_reference,
            manifest: diff.to,
        },
        structure: diff.structure,
        filesystem: ImageFilesystemDiffResult {
            total_changes,
            changes,
            next_after_path_hex,
        },
    })?;
    Ok(())
}

fn resolve_image(
    state: &Path,
    images: &ImageService,
    selector: &ImageSelector,
) -> Result<(ImageView, Option<String>)> {
    match selector {
        ImageSelector::Digest(digest) => Ok((images.inspect(digest)?, None)),
        ImageSelector::Reference(reference) => {
            let layout = catalog_layout(state)?;
            let entry = LocalImageCatalog::new(&layout)
                .resolve(reference)?
                .with_context(|| format!("local OCI reference is unknown: {reference}"))?;
            let image = images.inspect(&entry.manifest.digest)?;
            if image.manifest != entry.manifest {
                bail!("Catalog descriptor does not match resolved OCI Manifest: {reference}");
            }
            Ok((image, Some(reference.clone())))
        }
    }
}

fn validate_path_hex_cursor(value: &str) -> Result<String> {
    if value.len() < 2
        || !value.len().is_multiple_of(2)
        || !value.starts_with("2f")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("--after-path-hex must be lowercase hex for an absolute raw path");
    }
    Ok(value.to_owned())
}

fn catalog_layout(state: &Path) -> Result<OciLayout> {
    ensure_private_directory(state)?;
    OciLayout::open(state.join("oci"))
}

fn host_platform() -> Result<Platform> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Platform::linux(Architecture::Amd64)),
        "aarch64" => Ok(Platform::linux(Architecture::Arm64)),
        architecture => {
            bail!("host architecture {architecture} has no default OCI platform; supply --platform")
        }
    }
}

fn run_runtime_config(state: &Path, command: RuntimeConfigCommand) -> Result<u8> {
    match command {
        RuntimeConfigCommand::Create { image, output } => {
            let images = image_service(state)?;
            let (resolved, requested_reference) = resolve_image(state, &images, &image)?;
            let manifest_digest = resolved.manifest.digest;
            let runtime =
                RuntimeConfig::from_image_config(&images.image_config(&manifest_digest)?)?;
            let bytes = runtime.encoded()?;
            write_new_output(&output, &bytes)?;
            emit(&RuntimeConfigCreateResult {
                schema_version: 1,
                requested_reference,
                manifest_digest,
                output: absolute_path(&output)?,
                size: bytes.len(),
            })?;
        }
        RuntimeConfigCommand::Check { path } => return check_runtime_config(&path),
    }
    Ok(0)
}

fn check_runtime_config(path: &Path) -> Result<u8> {
    let bytes = read_bounded_file(path, 16 * 1024 * 1024)?;
    let runtime = RuntimeConfig::load(&bytes)?;
    emit(&RuntimeConfigCheckResult {
        schema_version: 1,
        valid: true,
        oci_version: runtime.oci_version().to_owned(),
    })?;
    Ok(0)
}

fn run_managed_service(state: &Path, command: ManagedServiceCommand) -> Result<u8> {
    let ManagedServiceCommand::Check { path } = command;
    let service = ManagedServiceFile::load(&path)?;
    let runtime_source = read_bounded_file(&service.runtime_config_file, 16 * 1024 * 1024)?;
    let runtime = RuntimeConfig::load(&runtime_source)?;
    runtime.validate_native_managed_profile()?;
    let runtime_bytes = runtime.encoded()?;
    let images = image_service(state)?;
    let (image, requested_reference) =
        resolve_image(state, &images, &service.initial_image.parse()?)?;
    emit(&ManagedServiceCheckResult {
        schema_version: 1,
        valid: true,
        name: service.name,
        requested_reference,
        initial_image: image.manifest,
        runtime_config: ContentSummary {
            digest: digest_bytes(&runtime_bytes),
            size: runtime_bytes.len(),
        },
        readiness: service.readiness,
    })?;
    Ok(0)
}

fn run_run(state: &Path, command: RunCommand) -> Result<u8> {
    let command = match command {
        RunCommand::Start(arguments) => return run_start(state, arguments),
        command => command,
    };
    let _operation = match &command {
        RunCommand::Verify { .. } => StateOperation::enter_existing(state)?,
        RunCommand::Reconcile(arguments) if arguments.dry_run => {
            StateOperation::enter_existing(state)?
        }
        _ => StateOperation::enter(state)?,
    };
    match command {
        RunCommand::Start(_) => unreachable!("Run start returned before acquiring state"),
        RunCommand::Get { run_id } => {
            emit(&run_database(state)?.get(run_id)?)?;
            Ok(0)
        }
        RunCommand::Verify { run_id } => {
            emit(&crate::maintenance::verify_run(state, run_id)?)?;
            Ok(0)
        }
        RunCommand::List {
            limit,
            after,
            lifecycle,
        } => run_list(state, limit, after, lifecycle),
        RunCommand::Diff { left, right, limit } => run_diff(state, left, right, limit),
        RunCommand::Reconcile(arguments) => run_reconcile(state, &arguments),
        RunCommand::Stdout { command } => run_bytes(state, command, RunStream::Stdout),
        RunCommand::Stderr { command } => run_bytes(state, command, RunStream::Stderr),
    }
}

fn run_state(state: &Path, command: StateCommand) -> Result<u8> {
    match command {
        StateCommand::Verify => {
            let _maintenance = StateMaintenance::enter_existing(state)?;
            emit(&crate::maintenance::verify_state(state)?)?;
            Ok(0)
        }
        StateCommand::Gc {
            command: StateGcCommand::Plan { output },
        } => {
            let _maintenance = StateMaintenance::enter_existing(state)?;
            let plan = crate::maintenance::plan_gc(state)?;
            write_new_output(&output, &plan.encoded()?)?;
            emit(&StateGcPlanResult {
                schema_version: 1,
                output: absolute_path(&output)?,
                plan_digest: plan.plan_digest.clone(),
                roots: u64::try_from(plan.roots.len()).context("GC root count overflow")?,
                reachable_oci_blobs: plan.reachable_oci_blobs,
                reachable_oci_bytes: plan.reachable_oci_bytes,
                delete_oci_blobs: u64::try_from(plan.delete.len())
                    .context("GC delete count overflow")?,
                delete_oci_bytes: plan.delete_bytes()?,
            })?;
            Ok(0)
        }
        StateCommand::Gc {
            command: StateGcCommand::Apply { plan },
        } => {
            let bytes = read_bounded_file(&plan, crate::maintenance::MAX_STATE_GC_PLAN_BYTES)?;
            let plan: StateGcPlan =
                serde_json::from_slice(&bytes).context("state GC plan is invalid JSON")?;
            let _maintenance = StateMaintenance::enter_existing(state)?;
            let result = crate::maintenance::apply_gc(state, &plan)?;
            let exit = u8::from(result.failed > 0);
            emit(&result)?;
            Ok(exit)
        }
    }
}

fn run_list(
    state: &Path,
    limit: usize,
    after: Option<RunId>,
    lifecycle: Option<RunLifecycleArg>,
) -> Result<u8> {
    if !(1..=100).contains(&limit) {
        bail!("--limit must be between 1 and 100");
    }
    let page = run_database(state)?.list(lifecycle.map(RunLifecycleArg::as_str), after, limit)?;
    let next_after = page
        .has_more
        .then(|| page.records.last().map(record_run_id))
        .flatten();
    emit(&RunListResult {
        schema_version: 1,
        runs: page.records,
        next_after,
    })?;
    Ok(0)
}

fn run_diff(state: &Path, left: RunId, right: RunId, limit: usize) -> Result<u8> {
    if !(1..=1000).contains(&limit) {
        bail!("--limit must be between 1 and 1000");
    }
    let database = run_database(state)?;
    let left_record = database.get(left)?;
    let right_record = database.get(right)?;
    let left_value = comparable_run_record(&left_record)?;
    let right_value = comparable_run_record(&right_record)?;
    let mut differences = Vec::new();
    collect_run_differences("", Some(&left_value), Some(&right_value), &mut differences);
    let total_differences = differences.len();
    differences.truncate(limit);
    emit(&RunDiffResult {
        schema_version: 1,
        left_run_id: left,
        right_run_id: right,
        equal: total_differences == 0,
        total_differences,
        truncated: total_differences > differences.len(),
        differences,
    })?;
    Ok(0)
}

fn record_run_id(record: &crate::core::RunRecord) -> RunId {
    match record {
        crate::core::RunRecord::Accepted(record) => record.run_id,
        crate::core::RunRecord::Terminal(record) => record.run_id,
    }
}

fn comparable_run_record(record: &crate::core::RunRecord) -> Result<Value> {
    let mut value = serde_json::to_value(record).context("failed to project Run Record")?;
    let object = value
        .as_object_mut()
        .context("Run Record projection must be an object")?;
    for field in ["schema_version", "run_id", "accepted_at", "terminal_at"] {
        object.remove(field);
    }
    Ok(value)
}

fn collect_run_differences(
    path: &str,
    left: Option<&Value>,
    right: Option<&Value>,
    output: &mut Vec<RunFieldDifference>,
) {
    if left == right {
        return;
    }
    match (left, right) {
        (Some(Value::Object(left)), Some(Value::Object(right))) => {
            let keys = left.keys().chain(right.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                let path = format!("{path}/{}", json_pointer_segment(key));
                collect_run_differences(&path, left.get(key), right.get(key), output);
            }
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) => {
            for index in 0..left.len().max(right.len()) {
                let path = format!("{path}/{index}");
                collect_run_differences(&path, left.get(index), right.get(index), output);
            }
        }
        _ => output.push(RunFieldDifference {
            path: if path.is_empty() {
                "/".to_owned()
            } else {
                path.to_owned()
            },
            left: run_field_value(left),
            right: run_field_value(right),
        }),
    }
}

fn run_field_value(value: Option<&Value>) -> RunFieldValue {
    value.map_or(RunFieldValue::Missing, |value| RunFieldValue::Value {
        value: value.clone(),
    })
}

fn json_pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(target_os = "linux")]
fn run_reconcile(state: &Path, arguments: &RunReconcileArgs) -> Result<u8> {
    let database = if arguments.dry_run {
        RunDatabase::open_existing(state.join("runs.sqlite3"))?
    } else {
        run_database(state)?
    };
    let images = (!arguments.dry_run)
        .then(|| image_service(state))
        .transpose()?;
    if let Some(run_id) = arguments.run_id {
        emit(&crate::native_reconcile::reconcile_native_run(
            state,
            &database,
            images.as_ref(),
            run_id,
            arguments.dry_run,
        )?)?;
        return Ok(0);
    }
    let limit = arguments.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        bail!("--limit must be between 1 and 100");
    }
    let result = crate::native_reconcile::reconcile_native_runs(
        state,
        &database,
        images.as_ref(),
        arguments.after,
        limit,
        arguments.dry_run,
    )?;
    let exit_code = u8::from(result.failed > 0);
    emit(&result)?;
    Ok(exit_code)
}

#[cfg(not(target_os = "linux"))]
fn run_reconcile(_state: &Path, _arguments: &RunReconcileArgs) -> Result<u8> {
    bail!("native Run reconciliation currently requires Linux")
}

fn run_start(state: &Path, arguments: RunStartArgs) -> Result<u8> {
    if arguments.timeout_seconds == 0 {
        bail!("--timeout-seconds must be greater than zero");
    }
    if arguments.stdout_limit_bytes == 0 || arguments.stderr_limit_bytes == 0 {
        bail!("stream limits must be greater than zero");
    }
    if arguments.stdout_limit_bytes > MAX_CAPTURED_STREAM_BYTES
        || arguments.stderr_limit_bytes > MAX_CAPTURED_STREAM_BYTES
    {
        bail!("stream limits must not exceed {MAX_CAPTURED_STREAM_BYTES} bytes");
    }
    if matches!(arguments.backend, RunBackendArg::Docker) && arguments.managed_service.is_some() {
        bail!("--managed-service requires --backend native");
    }
    let runtime_source = read_bounded_file(&arguments.runtime_config, 16 * 1024 * 1024)?;
    let runtime = RuntimeConfig::load(&runtime_source)?;
    let runtime_bytes = runtime.encoded()?;
    let managed_service = arguments
        .managed_service
        .as_deref()
        .map(load_managed_service)
        .transpose()?;
    let stdin = match arguments.stdin {
        Some(path) => read_bounded_file(&path, MAX_STDIN_BYTES)?,
        None => Vec::new(),
    };
    let controls = RunControls {
        stdin: StoredBytes::Available {
            digest: digest_bytes(&stdin),
            size: u64::try_from(stdin.len()).context("stdin size overflow")?,
        },
        timeout_seconds: arguments.timeout_seconds,
        stdout_limit_bytes: arguments.stdout_limit_bytes,
        stderr_limit_bytes: arguments.stderr_limit_bytes,
        network: arguments.network.into(),
    };
    let _operation = StateOperation::enter(state)?;
    let images = image_service(state)?;
    let (initial_image, requested_image_reference) =
        resolve_image(state, &images, &arguments.initial_image)?;
    let initial_manifest = initial_image.manifest.digest;
    let managed_service = managed_service
        .map(|mut service| {
            let (image, requested_reference) = resolve_image(state, &images, &service.image)?;
            service.image = ImageSelector::Digest(image.manifest.digest);
            Ok::<_, anyhow::Error>((service, requested_reference))
        })
        .transpose()?;
    let database = run_database(state)?;
    let result = match arguments.backend {
        RunBackendArg::Docker => {
            let docker = DockerBackend::discover()?;
            Runner::docker(&database, &images, &docker).run_selected(
                &initial_manifest,
                requested_image_reference.as_deref(),
                &runtime,
                &runtime_bytes,
                controls,
                &stdin,
            )?
        }
        RunBackendArg::Native => run_native(
            &database,
            &images,
            &initial_manifest,
            requested_image_reference.as_deref(),
            &runtime,
            &runtime_bytes,
            controls,
            &stdin,
            managed_service.as_ref(),
        )?,
    };
    emit(&RunStartResult::from(&result))?;
    Ok(result.cli_exit_code)
}

#[cfg(target_os = "linux")]
#[allow(
    clippy::too_many_arguments,
    reason = "the CLI boundary passes each accepted Run input without hiding protocol fields"
)]
fn run_native(
    database: &RunDatabase,
    images: &ImageService,
    initial_manifest: &Digest,
    requested_image_reference: Option<&str>,
    runtime: &RuntimeConfig,
    runtime_bytes: &[u8],
    controls: RunControls,
    stdin: &[u8],
    managed_service: Option<&(LoadedManagedService, Option<String>)>,
) -> Result<crate::execution::RunResult> {
    let runc = crate::runc::RuncRunner::discover(std::time::Duration::from_secs(5))?;
    let runner = Runner::native(database, images, &runc);
    match managed_service {
        Some((service, service_requested_reference)) => runner.run_with_managed_service(
            ManagedPrimaryInput {
                initial_manifest,
                requested_image_reference,
                runtime,
                runtime_bytes,
                controls,
                stdin,
            },
            &ManagedServiceInput {
                name: service.declaration.name.clone(),
                requested_image_reference: service_requested_reference.as_deref(),
                initial_manifest: match &service.image {
                    ImageSelector::Digest(digest) => digest,
                    ImageSelector::Reference(_) => {
                        unreachable!("Managed Service image was resolved before acceptance")
                    }
                },
                runtime: &service.runtime,
                runtime_bytes: &service.runtime_bytes,
                readiness: service.declaration.readiness.clone(),
            },
        ),
        None => runner.run_selected(
            initial_manifest,
            requested_image_reference,
            runtime,
            runtime_bytes,
            controls,
            stdin,
        ),
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::too_many_arguments,
    reason = "the portable stub mirrors the native CLI boundary exactly"
)]
fn run_native(
    _database: &RunDatabase,
    _images: &ImageService,
    _initial_manifest: &Digest,
    _requested_image_reference: Option<&str>,
    _runtime: &RuntimeConfig,
    _runtime_bytes: &[u8],
    _controls: RunControls,
    _stdin: &[u8],
    _managed_service: Option<&(LoadedManagedService, Option<String>)>,
) -> Result<crate::execution::RunResult> {
    bail!("the native execution backend currently requires Linux")
}

fn load_managed_service(path: &Path) -> Result<LoadedManagedService> {
    let declaration = ManagedServiceFile::load(path)?;
    let image = declaration.initial_image.parse()?;
    let source = read_bounded_file(&declaration.runtime_config_file, 16 * 1024 * 1024)?;
    let runtime = RuntimeConfig::load(&source)?;
    let runtime_bytes = runtime.encoded()?;
    Ok(LoadedManagedService {
        declaration,
        image,
        runtime,
        runtime_bytes,
    })
}

fn run_bytes(state: &Path, command: RunBytesCommand, stream: RunStream) -> Result<u8> {
    let RunBytesCommand::Get {
        run_id,
        participant,
        output,
    } = command;
    let field = stream.storage_field(participant);
    let bytes = run_database(state)?.bytes(run_id, field)?;
    write_new_output(&output, &bytes)?;
    emit(&RunStreamGetResult {
        schema_version: 2,
        run_id,
        participant,
        field: stream,
        output: absolute_path(&output)?,
    })?;
    Ok(0)
}

fn run_schema(command: SchemaCommand) -> Result<u8> {
    match command {
        SchemaCommand::List => emit(&SchemaListResult {
            schema_version: 1,
            schemas: SchemaName::ALL.to_vec(),
        })?,
        SchemaCommand::Show { name } => match name {
            SchemaName::AcceptedRunRecord => emit(&schemars::schema_for!(AcceptedRunRecord))?,
            SchemaName::TerminalRunRecord => emit(&schemars::schema_for!(TerminalRunRecord))?,
            SchemaName::RunStartResult => emit(&schemars::schema_for!(RunStartResult))?,
            SchemaName::RunListResult => emit(&schemars::schema_for!(RunListResult))?,
            SchemaName::RunDiffResult => emit(&schemars::schema_for!(RunDiffResult))?,
            SchemaName::RunStreamGetResult => emit(&schemars::schema_for!(RunStreamGetResult))?,
            SchemaName::RunReconcileResult => emit(&schemars::schema_for!(RunReconcileResult))?,
            SchemaName::RunReconcileBatchResult => {
                emit(&schemars::schema_for!(RunReconcileBatchResult))?;
            }
            SchemaName::RunVerifyResult => emit(&schemars::schema_for!(RunVerifyResult))?,
            SchemaName::ImageOperationResult => emit(&schemars::schema_for!(ImageOperationResult))?,
            SchemaName::ImageInspectResult => emit(&schemars::schema_for!(ImageInspectResult))?,
            SchemaName::ImageImportResult => emit(&schemars::schema_for!(ImageImportResult))?,
            SchemaName::ImagePullResult => emit(&schemars::schema_for!(ImagePullResult))?,
            SchemaName::ImageCatalogListResult => {
                emit(&schemars::schema_for!(ImageCatalogListResult))?;
            }
            SchemaName::ImageCatalogShowResult => {
                emit(&schemars::schema_for!(ImageCatalogShowResult))?;
            }
            SchemaName::ImageCatalogSetResult => {
                emit(&schemars::schema_for!(ImageCatalogSetResult))?;
            }
            SchemaName::ImageCatalogRemoveResult => {
                emit(&schemars::schema_for!(ImageCatalogRemoveResult))?;
            }
            SchemaName::ImageDiffResult => emit(&schemars::schema_for!(ImageDiffResult))?,
            SchemaName::ImageExportResult => emit(&schemars::schema_for!(ImageExportResult))?,
            SchemaName::ImageFileGetResult => emit(&schemars::schema_for!(ImageFileGetResult))?,
            SchemaName::DockerImageMaterializeResult => {
                emit(&schemars::schema_for!(DockerImageMaterializeResult))?;
            }
            SchemaName::DockerImageCheckoutCreateResult => {
                emit(&schemars::schema_for!(DockerImageCheckoutCreateResult))?;
            }
            SchemaName::RuntimeConfigCreateResult => {
                emit(&schemars::schema_for!(RuntimeConfigCreateResult))?;
            }
            SchemaName::RuntimeConfigCheckResult => {
                emit(&schemars::schema_for!(RuntimeConfigCheckResult))?;
            }
            SchemaName::ManagedServiceCheckResult => {
                emit(&schemars::schema_for!(ManagedServiceCheckResult))?;
            }
            SchemaName::StateVerifyResult => emit(&schemars::schema_for!(StateVerifyResult))?,
            SchemaName::StateGcPlan => emit(&schemars::schema_for!(StateGcPlan))?,
            SchemaName::StateGcPlanResult => {
                emit(&schemars::schema_for!(StateGcPlanResult))?;
            }
            SchemaName::StateGcApplyResult => {
                emit(&schemars::schema_for!(StateGcApplyResult))?;
            }
            SchemaName::VmStatus => emit(&schemars::schema_for!(crate::managed_vm::VmStatus))?,
            SchemaName::VmInstallResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmInstallResult))?;
            }
            SchemaName::VmOperationResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmOperationResult))?;
            }
            SchemaName::VmOperationStatus => {
                emit(&schemars::schema_for!(crate::managed_vm::VmOperationStatus))?;
            }
            SchemaName::VmCancelResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmCancelResult))?;
            }
            SchemaName::VmDiscardResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmDiscardResult))?;
            }
            SchemaName::SchemaListResult => emit(&schemars::schema_for!(SchemaListResult))?,
        },
    }
    Ok(0)
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

fn read_bounded_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    if size > max_bytes {
        bail!(
            "{} exceeds the {max_bytes}-byte input limit",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).context("input is too large")?);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if u64::try_from(bytes.len()).context("input is too large")? > max_bytes {
        bail!(
            "{} exceeds the {max_bytes}-byte input limit",
            path.display()
        );
    }
    Ok(bytes)
}

fn absolute_path(path: &Path) -> Result<String> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}
