use super::*;

pub(super) fn validate_handshake(handshake: &VmHandshake) -> Result<()> {
    ensure!(
        handshake.schema_version == 1,
        "unsupported VM handshake schema"
    );
    ensure!(
        handshake.protocol_version == PROTOCOL_VERSION,
        "unsupported VM transport protocol"
    );
    ensure!(
        handshake.runlab_version == env!("CARGO_PKG_VERSION"),
        "guest RunLab version {} does not match host {}",
        handshake.runlab_version,
        env!("CARGO_PKG_VERSION")
    );
    ensure!(handshake.os == "linux", "managed VM guest must run Linux");
    ensure!(
        normalize_architecture(&handshake.architecture)
            == normalize_architecture(env::consts::ARCH),
        "guest RunLab architecture does not match the host"
    );
    Ok(())
}

pub(super) fn parse_runc_identity(bytes: &[u8]) -> Result<VmRuncIdentity> {
    let text = std::str::from_utf8(bytes).context("runc version output is not UTF-8")?;
    let mut lines = text.lines();
    let version = lines
        .next()
        .and_then(|line| line.strip_prefix("runc version "))
        .context("runc version output omitted its version")?;
    let commit = lines
        .next()
        .and_then(|line| line.strip_prefix("commit: "))
        .context("runc version output omitted its commit")?;
    let spec = lines
        .next()
        .and_then(|line| line.strip_prefix("spec: "))
        .context("runc version output omitted its Runtime Spec version")?;
    Ok(VmRuncIdentity {
        version: version.to_owned(),
        commit: commit.to_owned(),
        spec: spec.to_owned(),
    })
}

pub(super) fn validate_runc_identity(identity: &VmRuncIdentity) -> Result<()> {
    ensure!(
        identity.version == RUNC_VERSION
            && identity.commit == RUNC_COMMIT
            && identity.spec == RUNC_SPEC,
        "managed VM requires runc {RUNC_VERSION}, commit {RUNC_COMMIT}, spec {RUNC_SPEC}; found {} / {} / {}",
        identity.version,
        identity.commit,
        identity.spec
    );
    Ok(())
}

impl VmReferenceTools {
    pub(super) fn all_executable(&self) -> bool {
        [
            &self.ip,
            &self.nft,
            &self.conntrack,
            &self.unshare,
            &self.nsenter,
            &self.cat,
            &self.modprobe,
            &self.systemd_run,
            &self.systemctl,
        ]
        .into_iter()
        .all(|tool| tool.executable)
    }
}

pub(super) fn validate_reference_profile(profile: &VmReferenceProfile) -> Result<()> {
    ensure!(
        profile.tools.all_executable(),
        "managed VM reference profile is missing a required executable"
    );
    ensure!(
        profile.tools.conntrack.package_version.as_deref() == Some(CONNTRACK_PACKAGE_VERSION),
        "managed VM requires conntrack package {CONNTRACK_PACKAGE_VERSION}"
    );
    ensure!(
        profile.kernel.cgroup_version == Some(2),
        "managed VM requires cgroup v2"
    );
    ensure!(
        profile.kernel.overlayfs.active,
        "managed VM requires active OverlayFS support"
    );
    ensure!(
        profile.kernel.overlayfs.configured,
        "managed VM requires persistent OverlayFS module configuration"
    );
    ensure!(
        profile.systemd,
        "managed VM requires systemd as the system manager"
    );
    ensure!(
        profile.kernel.ipv4_forwarding.active,
        "managed VM requires net.ipv4.ip_forward=1"
    );
    ensure!(
        profile.kernel.ipv4_forwarding.configured,
        "managed VM requires persistent net.ipv4.ip_forward=1 configuration"
    );
    ensure!(profile.ready, "managed VM reference profile is not ready");
    Ok(())
}

pub(super) fn selected_instance_image(instance: &LimaInstance) -> Result<VmImage> {
    let architecture = normalize_architecture(&instance.arch);
    ensure!(
        instance.config.images.len() == 1,
        "managed VM must use exactly one image without fallback"
    );
    let image = &instance.config.images[0];
    ensure!(
        normalize_architecture(&image.arch) == architecture && image.variant == "server",
        "managed VM must use the pinned server image for its architecture"
    );
    let digest = image
        .digest
        .as_ref()
        .context("managed VM image is not digest-pinned")?;
    let expected = pinned_lima_image(architecture)?;
    ensure!(
        image.location == expected.location && digest == &expected.digest,
        "managed VM requires reference image {} at {}; found {digest} at {}",
        expected.digest,
        expected.location,
        image.location
    );
    Ok(VmImage {
        location: image.location.clone(),
        digest: digest.clone(),
    })
}

fn pinned_lima_image(architecture: &str) -> Result<VmImage> {
    let architecture = normalize_architecture(architecture);
    let (location, digest) = match architecture {
        "aarch64" => (
            "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-arm64.img",
            "sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc",
        ),
        "x86_64" => (
            "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-amd64.img",
            "sha256:ffe6203da54deeb6db5d2a98a83f9ec8e55f149d3f7ba622e1abe5fa966ee3d6",
        ),
        value => bail!("managed VM creation does not support host architecture {value}"),
    };
    Ok(VmImage {
        location: location.to_owned(),
        digest: Digest::parse(digest)?,
    })
}

pub(super) fn pinned_lima_template(architecture: &str) -> Result<String> {
    let architecture = normalize_architecture(architecture);
    let image = pinned_lima_image(architecture)?;
    serde_json::to_string(&serde_json::json!({
        "minimumLimaVersion": LIMA_VERSION,
        "images": [{
            "location": image.location,
            "arch": architecture,
            "digest": image.digest,
            "variant": "server"
        }]
    }))
    .context("cannot encode pinned Lima template")
}

pub(super) fn validate_forwarded_argv(
    argv: &[String],
    input_count: usize,
    output_count: usize,
) -> Result<()> {
    ensure!(
        input_count <= MAX_FILE_SLOTS,
        "VM execution supports at most {MAX_FILE_SLOTS} inputs"
    );
    ensure!(
        output_count <= MAX_FILE_SLOTS,
        "VM execution supports at most {MAX_FILE_SLOTS} outputs"
    );
    ensure!(
        argv.len() <= MAX_FORWARDED_ARGUMENTS,
        "VM execution supports at most {MAX_FORWARDED_ARGUMENTS} forwarded arguments"
    );
    let argument_bytes = argv.iter().try_fold(0_usize, |total, argument| {
        total
            .checked_add(argument.len())
            .context("forwarded argument size overflow")
    })?;
    ensure!(
        argument_bytes <= MAX_FORWARDED_ARGUMENT_BYTES,
        "VM forwarded arguments exceed {MAX_FORWARDED_ARGUMENT_BYTES} bytes"
    );
    let first = argv
        .first()
        .context("VM execution requires RunLab arguments after --")?;
    ensure!(
        matches!(
            first.as_str(),
            "image" | "runtime-config" | "managed-service" | "run" | "state" | "schema" | "docker"
        ),
        "VM execution only accepts public RunLab commands"
    );
    ensure!(
        !argv
            .iter()
            .any(|arg| arg == "--state" || arg.starts_with("--state=")),
        "VM execution owns --state through its namespace"
    );
    let mut inputs = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for argument in argv {
        if let Some((_, index)) = parse_argument_slot(argument, "@input/")? {
            ensure!(index < input_count, "input slot {index} was not declared");
            ensure!(
                inputs.insert(index),
                "input slot {index} is referenced more than once"
            );
        }
        if let Some((_, index)) = parse_argument_slot(argument, "@output/")? {
            ensure!(index < output_count, "output slot {index} was not declared");
            ensure!(
                outputs.insert(index),
                "output slot {index} is referenced more than once"
            );
        }
    }
    ensure!(
        outputs.len() == output_count,
        "every declared output must be referenced exactly once"
    );
    Ok(())
}

pub(super) fn rewrite_file_tokens(
    argv: &[String],
    operation_id: Uuid,
    operation: &GuestOperation,
    output_count: usize,
) -> Result<Vec<String>> {
    argv.iter()
        .map(|argument| {
            if let Some((option, index)) = parse_argument_slot(argument, "@input/")? {
                ensure!(index < operation.input_count, "invalid input slot");
                return Ok(replace_argument_slot(
                    option,
                    derived_input_path(operation_id, operation, index)?,
                ));
            }
            if let Some((option, index)) = parse_argument_slot(argument, "@output/")? {
                ensure!(index < output_count, "invalid output slot");
                return Ok(replace_argument_slot(
                    option,
                    operation_file(operation_id, "output", index),
                ));
            }
            Ok(argument.clone())
        })
        .collect()
}

fn parse_argument_slot<'a>(
    argument: &'a str,
    prefix: &str,
) -> Result<Option<(Option<&'a str>, usize)>> {
    if let Some(index) = parse_slot(argument, prefix)? {
        return Ok(Some((None, index)));
    }
    let Some((option, value)) = argument.split_once('=') else {
        return Ok(None);
    };
    if !option.starts_with("--") {
        return Ok(None);
    }
    Ok(parse_slot(value, prefix)?.map(|index| (Some(option), index)))
}

fn replace_argument_slot(option: Option<&str>, path: String) -> String {
    match option {
        Some(option) => format!("{option}={path}"),
        None => path,
    }
}

pub(super) fn parse_slot(argument: &str, prefix: &str) -> Result<Option<usize>> {
    let Some(value) = argument.strip_prefix(prefix) else {
        return Ok(None);
    };
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid file slot token: {argument}"
    );
    Ok(Some(value.parse().context("file slot index is too large")?))
}

pub(super) fn validate_name(label: &str, value: &str) -> Result<()> {
    ensure!(
        (1..=63).contains(&value.len()),
        "{label} must contain 1 to 63 characters"
    );
    let mut bytes = value.bytes();
    ensure!(
        bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "{label} must start with a lowercase letter or digit"
    );
    ensure!(
        bytes.all(|byte| byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'_')),
        "{label} contains an invalid character"
    );
    Ok(())
}

pub(super) fn ensure_guest_linux() -> Result<()> {
    ensure!(
        env::consts::OS == "linux",
        "managed VM guest controls require Linux"
    );
    Ok(())
}

pub(super) fn guest_operation(root: &Path) -> Result<GuestOperation> {
    let bytes = fs::read(root.join("operation.json")).context("guest operation is unknown")?;
    let operation: GuestOperation =
        serde_json::from_slice(&bytes).context("guest operation metadata is invalid")?;
    validate_name("state namespace", &operation.namespace)?;
    Ok(operation)
}

pub(super) fn load_guest_operation(operation_id: Uuid) -> Result<GuestOperation> {
    let operation = guest_operation(&operation_path(operation_id))?;
    ensure!(
        operation.schema_version == 1,
        "guest operation schema mismatch"
    );
    ensure!(
        operation.protocol_version == PROTOCOL_VERSION,
        "guest operation protocol mismatch"
    );
    ensure!(
        operation.runlab_version == env!("CARGO_PKG_VERSION"),
        "guest operation version mismatch"
    );
    ensure!(
        operation.operation_id == operation_id,
        "guest operation identity mismatch"
    );
    ensure!(
        operation.input_identities.len() == operation.input_count,
        "guest operation input identity count mismatch"
    );
    validate_runtime_config_inputs(&operation.runtime_config_inputs, operation.input_count)?;
    Ok(operation)
}

pub(super) fn operation_path(operation_id: Uuid) -> PathBuf {
    Path::new(GUEST_OPERATION_ROOT).join(operation_id.to_string())
}

pub(super) fn guest_state_path(namespace: &str) -> Result<PathBuf> {
    validate_name("state namespace", namespace)?;
    Ok(Path::new(GUEST_STATE_ROOT).join(namespace))
}

pub(super) fn operation_file(operation_id: Uuid, kind: &str, index: usize) -> String {
    operation_path(operation_id)
        .join(format!("{kind}-{index}"))
        .to_string_lossy()
        .into_owned()
}

pub(super) fn validate_file_slot(
    operation: &GuestOperation,
    kind: &str,
    index: usize,
) -> Result<()> {
    match kind {
        "input" => ensure!(index < operation.input_count, "input slot is out of range"),
        "output" => ensure!(
            index < operation.output_count,
            "output slot is out of range"
        ),
        _ => bail!("invalid operation file kind"),
    }
    Ok(())
}

pub(super) fn unit_name(operation_id: Uuid) -> String {
    format!("runlab-vm-{operation_id}.service")
}

pub(super) fn guest_binary_path() -> String {
    format!("{GUEST_BINARY_ROOT}/{}/runlab", env!("CARGO_PKG_VERSION"))
}

pub(super) fn parse_systemd_status(
    operation_id: Uuid,
    namespace: &str,
    output_count: usize,
    runtime_config_inputs: &[usize],
    bytes: &[u8],
) -> Result<VmOperationStatus> {
    let text = std::str::from_utf8(bytes).context("systemd status is not UTF-8")?;
    let mut fields = std::collections::BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key, value);
        }
    }
    let load = fields.get("LoadState").copied().unwrap_or("unknown");
    ensure!(
        load == "loaded",
        "guest operation unit is unavailable: {load}"
    );
    let active = fields.get("ActiveState").copied().unwrap_or("unknown");
    let sub = fields.get("SubState").copied().unwrap_or("unknown");
    let terminal = active == "failed" || sub == "exited" || active == "inactive";
    let main_code = fields
        .get("ExecMainCode")
        .copied()
        .unwrap_or("0")
        .parse::<u16>()
        .unwrap_or(0);
    let main_status = fields
        .get("ExecMainStatus")
        .copied()
        .unwrap_or("0")
        .parse::<u16>()
        .unwrap_or(0);
    let exit_code = terminal.then(|| match main_code {
        1 => u8::try_from(main_status.min(255)).unwrap_or(255),
        2 | 3 => u8::try_from((128 + main_status).min(255)).unwrap_or(255),
        _ if fields.get("Result") == Some(&"success") => 0,
        _ => 1,
    });
    Ok(VmOperationStatus {
        schema_version: 1,
        operation_id,
        namespace: namespace.to_owned(),
        state: format!("{active}/{sub}"),
        terminal,
        exit_code,
        result: fields.get("Result").map(ToString::to_string),
        output_count,
        runtime_config_inputs: runtime_config_inputs.to_vec(),
    })
}

pub(super) fn file_identity(path: &Path) -> Result<FileIdentity> {
    ensure_regular_file(path)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(u64::try_from(read)?)
            .context("file size overflow")?;
    }
    Ok(FileIdentity {
        schema_version: 1,
        digest: finish_sha256(hasher),
        size,
    })
}

pub(super) fn ensure_regular_file(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "file must be a regular file: {}",
        path.display()
    );
    Ok(())
}

pub(super) fn privileged_file_identity(path: &str) -> Result<FileIdentity> {
    let digest_output = bounded_output(
        Command::new("/usr/bin/sudo").args(["/usr/bin/sha256sum", "--", path]),
        None,
        VM_TRANSFER_TIMEOUT,
        MAX_CONTROL_OUTPUT,
        "guest operation file hash",
    )?;
    ensure_status(&digest_output, "guest operation file hash")?;
    let hexadecimal = std::str::from_utf8(&digest_output.stdout)?
        .split_ascii_whitespace()
        .next()
        .context("sha256sum omitted the digest")?;
    let size_output = bounded_output(
        Command::new("/usr/bin/sudo").args(["/usr/bin/stat", "--format=%s", "--", path]),
        None,
        VM_CONTROL_TIMEOUT,
        MAX_CONTROL_OUTPUT,
        "guest operation file inspection",
    )?;
    ensure_status(&size_output, "guest operation file inspection")?;
    Ok(FileIdentity {
        schema_version: 1,
        digest: format!("sha256:{hexadecimal}").parse()?,
        size: std::str::from_utf8(&size_output.stdout)?
            .trim()
            .parse()
            .context("stat returned an invalid size")?,
    })
}

pub(super) fn privileged_file_to_stdout(path: &str) -> Result<()> {
    let output = bounded_status_with_stdout(
        Command::new("/usr/bin/sudo").args(["/usr/bin/cat", "--", path]),
        Stdio::inherit(),
        VM_TRANSFER_TIMEOUT,
        MAX_CONTROL_OUTPUT,
        "guest operation file read",
    )?;
    ensure_status(&output, "guest operation file read")?;
    Ok(())
}

pub(super) fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_new_private(path, &canonical_json(value)?)
}

#[cfg(unix)]
pub(super) fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

pub(super) fn normalize_architecture(value: &str) -> &str {
    match value {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        value => value,
    }
}

pub(super) fn ensure_status(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "{operation} failed with {}: {}",
        output.status,
        stderr.trim()
    )
}
