use super::{
    BTreeSet, Context, Digest, DockerBackend, ImageSelector, ImageService, LoadedManagedService,
    MAX_CAPTURED_STREAM_BYTES, MAX_STDIN_BYTES, ManagedServiceFile, Path, Result, RunBackendArg,
    RunBytesCommand, RunCommand, RunControls, RunDatabase, RunDiffResult, RunFieldDifference,
    RunFieldValue, RunId, RunLifecycleArg, RunListResult, RunReconcileArgs, RunStartArgs,
    RunStartResult, RunStream, RunStreamGetResult, Runner, RuntimeConfig, StateCommand,
    StateGcCommand, StateGcPlan, StateGcPlanResult, StateMaintenance, StateOperation, StoredBytes,
    Value, absolute_path, bail, digest_bytes, emit, image_service, read_bounded_file,
    resolve_image, run_database, write_new_output,
};
#[cfg(target_os = "linux")]
use super::{ManagedPrimaryInput, ManagedServiceInput};

pub(super) fn run_run(state: &Path, command: RunCommand) -> Result<u8> {
    let command = match command {
        RunCommand::Start(arguments) => return run_start(state, arguments),
        command => command,
    };
    let _operation = StateOperation::enter_existing(state)?;
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

pub(super) fn run_state(state: &Path, command: StateCommand) -> Result<u8> {
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

pub(super) fn run_list(
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

pub(super) fn run_diff(state: &Path, left: RunId, right: RunId, limit: usize) -> Result<u8> {
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

pub(super) fn record_run_id(record: &crate::core::RunRecord) -> RunId {
    match record {
        crate::core::RunRecord::Accepted(record) => record.run_id,
        crate::core::RunRecord::Terminal(record) => record.run_id,
    }
}

pub(super) fn comparable_run_record(record: &crate::core::RunRecord) -> Result<Value> {
    let mut value = serde_json::to_value(record).context("failed to project Run Record")?;
    let object = value
        .as_object_mut()
        .context("Run Record projection must be an object")?;
    for field in ["schema_version", "run_id", "accepted_at", "terminal_at"] {
        object.remove(field);
    }
    Ok(value)
}

pub(super) fn collect_run_differences(
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

pub(super) fn run_field_value(value: Option<&Value>) -> RunFieldValue {
    value.map_or(RunFieldValue::Missing, |value| RunFieldValue::Value {
        value: value.clone(),
    })
}

pub(super) fn json_pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(target_os = "linux")]
pub(super) fn run_reconcile(state: &Path, arguments: &RunReconcileArgs) -> Result<u8> {
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
pub(super) fn run_reconcile(_state: &Path, _arguments: &RunReconcileArgs) -> Result<u8> {
    bail!("native Run reconciliation currently requires Linux")
}

pub(super) fn run_start(state: &Path, arguments: RunStartArgs) -> Result<u8> {
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
pub(super) fn run_native(
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
    let backend =
        crate::native_backend::NativeBackend::discover(std::time::Duration::from_secs(5))?;
    let runner = Runner::native(database, images, &backend);
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
pub(super) fn run_native(
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

pub(super) fn load_managed_service(path: &Path) -> Result<LoadedManagedService> {
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

pub(super) fn run_bytes(state: &Path, command: RunBytesCommand, stream: RunStream) -> Result<u8> {
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
