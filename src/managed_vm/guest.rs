use super::*;

pub fn guest_handshake() -> VmHandshake {
    VmHandshake {
        schema_version: 1,
        protocol_version: PROTOCOL_VERSION,
        runlab_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: env::consts::OS.to_owned(),
        architecture: normalize_architecture(env::consts::ARCH).to_owned(),
    }
}

pub fn guest_prepare(
    operation_id: Uuid,
    namespace: &str,
    input_identities: Vec<FileIdentity>,
    runtime_config_inputs: Vec<usize>,
    output_count: usize,
    argv: Vec<String>,
) -> Result<()> {
    ensure_guest_linux()?;
    validate_name("state namespace", namespace)?;
    let input_count = input_identities.len();
    validate_forwarded_argv(&argv, input_count, output_count)?;
    validate_runtime_config_inputs(&runtime_config_inputs, input_count)?;
    let root = operation_path(operation_id);
    fs::create_dir_all(GUEST_OPERATION_ROOT).context("cannot create guest operation root")?;
    fs::create_dir(&root).context("cannot create guest operation directory")?;
    set_private_permissions(&root)?;
    let metadata = GuestOperation {
        schema_version: 1,
        protocol_version: PROTOCOL_VERSION,
        runlab_version: env!("CARGO_PKG_VERSION").to_owned(),
        operation_id,
        namespace: namespace.to_owned(),
        input_count,
        input_identities,
        runtime_config_inputs,
        output_count,
        argv,
    };
    write_new_json(&root.join("operation.json"), &metadata)
}

pub fn guest_start(operation_id: Uuid) -> Result<()> {
    ensure_guest_linux()?;
    let root = operation_path(operation_id);
    let operation = load_guest_operation(operation_id)?;
    for index in 0..operation.input_count {
        ensure_regular_file(Path::new(&operation_file(operation_id, "input", index)))?;
    }
    if !operation.runtime_config_inputs.is_empty() {
        let binary = guest_binary_path();
        let operation_id_text = operation_id.to_string();
        let output = Command::new("/usr/bin/sudo")
            .args([
                binary.as_str(),
                "__internal-vm-seal-inputs",
                "--operation-id",
                operation_id_text.as_str(),
            ])
            .output()
            .context("cannot seal guest OCI Runtime Config inputs")?;
        ensure_status(&output, "guest OCI Runtime Config input sealing")?;
    }
    let argv = rewrite_file_tokens(
        &operation.argv,
        operation_id,
        &operation,
        operation.output_count,
    )?;
    let state = guest_state_path(&operation.namespace)?;
    let state = state.to_str().context("guest state path is not UTF-8")?;
    let created = Command::new("/usr/bin/sudo")
        .args(["/usr/bin/install", "-d", "-m", "0700", state])
        .output()
        .context("cannot create guest state namespace")?;
    ensure_status(&created, "guest state namespace creation")?;
    let unit = unit_name(operation_id);
    let stdout = root.join("stdout");
    let stderr = root.join("stderr");
    let mut command = Command::new("/usr/bin/sudo");
    command.args([
        "/usr/bin/systemd-run",
        "--quiet",
        "--service-type=exec",
        "--remain-after-exit",
        "--unit",
        &unit,
        "--property",
        &format!("StandardOutput=file:{}", stdout.display()),
        "--property",
        &format!("StandardError=file:{}", stderr.display()),
        "--",
        &guest_binary_path(),
        "--state",
        state,
    ]);
    command.args(argv);
    let output = command
        .output()
        .context("cannot start guest RunLab operation")?;
    ensure_status(&output, "systemd-run")
}

pub fn guest_seal_inputs(operation_id: Uuid) -> Result<()> {
    seal_runtime_config_inputs(operation_id)
}

pub fn guest_status(operation_id: Uuid) -> Result<VmOperationStatus> {
    ensure_guest_linux()?;
    let operation = load_guest_operation(operation_id)?;
    let output = Command::new("/usr/bin/sudo")
        .args([
            "/usr/bin/systemctl",
            "show",
            &unit_name(operation_id),
            "--no-pager",
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=Result",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
        ])
        .output()
        .context("cannot inspect guest RunLab operation")?;
    ensure_status(&output, "systemctl show")?;
    parse_systemd_status(
        operation_id,
        &operation.namespace,
        operation.output_count,
        &operation.runtime_config_inputs,
        &output.stdout,
    )
}

pub fn guest_cancel(operation_id: Uuid) -> Result<VmCancelResult> {
    let before = guest_status(operation_id)?;
    let signal_sent = if before.terminal {
        false
    } else {
        let output = Command::new("/usr/bin/sudo")
            .args([
                "/usr/bin/systemctl",
                "kill",
                "--kill-whom=main",
                "--signal=SIGINT",
                &unit_name(operation_id),
            ])
            .output()
            .context("cannot cancel guest RunLab operation")?;
        ensure_status(&output, "systemctl kill")?;
        true
    };
    let status = guest_status(operation_id)?;
    Ok(VmCancelResult {
        schema_version: 1,
        operation_id,
        signal_sent,
        status,
    })
}

pub fn guest_file_info(operation_id: Uuid, kind: &str, index: usize) -> Result<FileIdentity> {
    let operation = load_guest_operation(operation_id)?;
    validate_file_slot(&operation, kind, index)?;
    privileged_file_identity(&operation_file(operation_id, kind, index))
}

pub fn guest_read_file(operation_id: Uuid, kind: &str, index: usize) -> Result<()> {
    let operation = load_guest_operation(operation_id)?;
    validate_file_slot(&operation, kind, index)?;
    privileged_file_to_stdout(&operation_file(operation_id, kind, index))
}

pub fn guest_read_stream(operation_id: Uuid, stream: &str) -> Result<()> {
    let _ = load_guest_operation(operation_id)?;
    ensure!(
        matches!(stream, "stdout" | "stderr"),
        "invalid operation stream"
    );
    privileged_file_to_stdout(
        operation_path(operation_id)
            .join(stream)
            .to_str()
            .context("guest stream path is not UTF-8")?,
    )
}

pub fn guest_stream_info(operation_id: Uuid, stream: &str) -> Result<FileIdentity> {
    let _ = load_guest_operation(operation_id)?;
    ensure!(
        matches!(stream, "stdout" | "stderr"),
        "invalid operation stream"
    );
    privileged_file_identity(
        operation_path(operation_id)
            .join(stream)
            .to_str()
            .context("guest stream path is not UTF-8")?,
    )
}

pub fn guest_remove(operation_id: Uuid) -> Result<()> {
    let _ = guest_status(operation_id).and_then(|status| {
        ensure!(status.terminal, "cannot remove a running guest operation");
        Ok(status)
    })?;
    let unit = unit_name(operation_id);
    let _ = Command::new("/usr/bin/sudo")
        .args(["/usr/bin/systemctl", "stop", &unit])
        .status();
    let _ = Command::new("/usr/bin/sudo")
        .args(["/usr/bin/systemctl", "reset-failed", &unit])
        .status();
    remove_operation_directories(operation_id)
}

fn remove_operation_directories(operation_id: Uuid) -> Result<()> {
    let root = operation_path(operation_id);
    let sealed = sealed_operation_path(operation_id);
    let output = Command::new("/usr/bin/sudo")
        .args([
            "/usr/bin/rm",
            "-r",
            "-f",
            "--",
            root.to_str().context("guest operation path is not UTF-8")?,
            sealed
                .to_str()
                .context("sealed guest input path is not UTF-8")?,
        ])
        .output()
        .context("cannot remove guest operation")?;
    ensure_status(&output, "guest operation removal")
}

pub fn guest_discard(operation_id: Uuid) -> Result<VmDiscardResult> {
    ensure_guest_linux()?;
    let root = operation_path(operation_id);
    let removed = match fs::symlink_metadata(&root) {
        Ok(_) => {
            guest_remove(operation_id)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let output = Command::new("/usr/bin/sudo")
                .args([
                    "/usr/bin/systemctl",
                    "show",
                    &unit_name(operation_id),
                    "--no-pager",
                    "--property=LoadState",
                ])
                .output()
                .context("cannot inspect discarded guest operation")?;
            ensure_status(&output, "systemctl show")?;
            ensure!(
                output.stdout == b"LoadState=not-found\n",
                "guest operation metadata is absent while its systemd unit exists"
            );
            false
        }
        Err(error) => return Err(error).context("cannot inspect guest operation metadata"),
    };
    Ok(VmDiscardResult {
        schema_version: 1,
        operation_id,
        removed,
    })
}

pub fn guest_abandon(operation_id: Uuid) -> Result<()> {
    ensure_guest_linux()?;
    let _ = load_guest_operation(operation_id)?;
    let output = Command::new("/usr/bin/sudo")
        .args([
            "/usr/bin/systemctl",
            "show",
            &unit_name(operation_id),
            "--no-pager",
            "--property=LoadState",
        ])
        .output()
        .context("cannot inspect prepared guest operation")?;
    ensure_status(&output, "systemctl show")?;
    ensure!(
        output.stdout == b"LoadState=not-found\n",
        "cannot abandon an operation after its systemd unit exists"
    );
    remove_operation_directories(operation_id)
}
