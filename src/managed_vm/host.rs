use super::*;

struct StagedOutput {
    temporary: NamedTempFile,
    destination: PathBuf,
}

#[derive(Debug)]
pub struct HostVm {
    limactl: PathBuf,
    instance: String,
}

impl HostVm {
    pub fn new(instance: Option<&str>) -> Result<Self> {
        let instance = instance.unwrap_or(DEFAULT_INSTANCE);
        validate_name("VM instance", instance)?;
        let limactl =
            env::var_os("RUNLAB_LIMACTL").map_or_else(|| PathBuf::from("limactl"), PathBuf::from);
        Ok(Self {
            limactl,
            instance: instance.to_owned(),
        })
    }

    pub fn status(&self) -> Result<VmStatus> {
        let instance = self.inspect_instance()?;
        let image = selected_instance_image(&instance)?;
        let (handshake, handshake_error) = if instance.status == "Running" {
            match self.handshake() {
                Ok(handshake) => (Some(handshake), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        } else {
            (None, None)
        };
        let (runc, runc_error) = if instance.status == "Running" {
            match self.runc_identity_at("/usr/local/bin/runc") {
                Ok(identity) => (Some(identity), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        } else {
            (None, None)
        };
        let (reference_profile, reference_profile_error) = if instance.status == "Running" {
            match self.reference_profile() {
                Ok(profile) => (Some(profile), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        } else {
            (None, None)
        };
        Ok(VmStatus {
            schema_version: 1,
            instance: instance.name,
            status: instance.status,
            lima_version: instance.lima_version,
            vm_type: instance.vm_type,
            architecture: instance.arch,
            plain: instance.config.plain,
            mounts: instance.config.mounts.len(),
            image,
            handshake,
            handshake_error,
            runc,
            runc_error,
            reference_profile,
            reference_profile_error,
        })
    }

    pub fn create(&self, cpus: u16, memory_gib: u16, disk_gib: u16) -> Result<VmStatus> {
        ensure!((1..=64).contains(&cpus), "--cpus must be between 1 and 64");
        ensure!(
            (1..=256).contains(&memory_gib),
            "--memory-gib must be between 1 and 256"
        );
        ensure!(
            (8..=1024).contains(&disk_gib),
            "--disk-gib must be between 8 and 1024"
        );
        self.validate_limactl_version()?;
        let template = pinned_lima_template(env::consts::ARCH)?;
        self.limactl_output_with_stdin(
            [
                "--tty=false",
                "create",
                "--name",
                &self.instance,
                "--plain",
                "--vm-type",
                "vz",
                "--arch",
                normalize_architecture(env::consts::ARCH),
                "--cpus",
                &cpus.to_string(),
                "--memory",
                &memory_gib.to_string(),
                "--disk",
                &disk_gib.to_string(),
                "-",
            ],
            template.as_bytes(),
        )?;
        self.status()
    }

    pub fn start(&self) -> Result<VmStatus> {
        let instance = self.inspect_instance()?;
        self.validate_instance(&instance)?;
        if instance.status != "Running" {
            self.limactl_status([
                OsStr::new("--tty=false"),
                OsStr::new("start"),
                OsStr::new(&self.instance),
            ])?;
        }
        self.status()
    }

    pub fn install(&self, binary: &Path, runc: &Path) -> Result<VmInstallResult> {
        self.ensure_running()?;
        let identity = file_identity(binary)?;
        let runc_identity = file_identity(runc)?;
        let operation_id = Uuid::now_v7();
        let staged = format!("/var/tmp/runlab-install-{operation_id}");
        let staged_runc = format!("/var/tmp/runlab-runc-install-{operation_id}");
        let result = (|| -> Result<VmInstallResult> {
            self.guest_status(["/usr/bin/install", "-m", "0700", "/dev/null", &staged])?;
            self.guest_status(["/usr/bin/install", "-m", "0700", "/dev/null", &staged_runc])?;
            self.copy_to_guest(binary, &staged)?;
            self.copy_to_guest(runc, &staged_runc)?;
            let remote = self.remote_file_identity(&staged)?;
            ensure!(
                remote.digest == identity.digest && remote.size == identity.size,
                "staged guest binary does not match the host binary"
            );
            let remote_runc = self.remote_file_identity(&staged_runc)?;
            ensure!(
                remote_runc.digest == runc_identity.digest
                    && remote_runc.size == runc_identity.size,
                "staged runc binary does not match the host binary"
            );
            let staged_handshake = self.handshake_at(&staged)?;
            validate_handshake(&staged_handshake)?;
            let staged_runc_identity = self.runc_identity_at(&staged_runc)?;
            let reference_profile = self.provision_reference_profile(operation_id)?;
            let directory = format!("{GUEST_BINARY_ROOT}/{}", env!("CARGO_PKG_VERSION"));
            let target = format!("{directory}/runlab");
            let temporary = format!("{directory}/.runlab-{operation_id}");
            self.guest_status([
                "/usr/bin/sudo",
                "/usr/bin/install",
                "-d",
                "-m",
                "0755",
                &directory,
            ])?;
            self.guest_status([
                "/usr/bin/sudo",
                "/usr/bin/install",
                "-m",
                "0755",
                &staged,
                &temporary,
            ])?;
            self.guest_status(["/usr/bin/sudo", "/usr/bin/mv", &temporary, &target])?;
            let runc_target = "/usr/local/bin/runc";
            let runc_temporary = format!("/usr/local/bin/.runc-{operation_id}");
            self.guest_status([
                "/usr/bin/sudo",
                "/usr/bin/install",
                "-m",
                "0755",
                &staged_runc,
                &runc_temporary,
            ])?;
            self.guest_status(["/usr/bin/sudo", "/usr/bin/mv", &runc_temporary, runc_target])?;
            let handshake = self.handshake()?;
            let installed_runc = self.runc_identity_at(runc_target)?;
            ensure!(
                installed_runc == staged_runc_identity,
                "installed runc identity changed during publication"
            );
            let installed_binary_identity = self.remote_file_identity(&target)?;
            ensure!(
                installed_binary_identity.digest == identity.digest
                    && installed_binary_identity.size == identity.size,
                "installed guest binary does not match the verified staging input"
            );
            let installed_runc_identity = self.remote_file_identity(runc_target)?;
            ensure!(
                installed_runc_identity.digest == runc_identity.digest
                    && installed_runc_identity.size == runc_identity.size,
                "installed runc binary does not match the verified staging input"
            );
            Ok(VmInstallResult {
                schema_version: 1,
                instance: self.instance.clone(),
                binary: target,
                digest: identity.digest,
                size: identity.size,
                handshake,
                runc_binary: runc_target.to_owned(),
                runc_digest: runc_identity.digest,
                runc_size: runc_identity.size,
                runc: installed_runc,
                reference_profile,
            })
        })();
        let _ = self.guest_status(["/usr/bin/rm", "-f", &staged]);
        let _ = self.guest_status(["/usr/bin/rm", "-f", &staged_runc]);
        result
    }

    pub fn execute(
        &self,
        namespace: &str,
        inputs: &[PathBuf],
        runtime_config_inputs: &[usize],
        outputs: &[PathBuf],
        argv: &[String],
        detach: bool,
    ) -> Result<(VmOperationResult, Option<AttachedOperation>)> {
        validate_name("state namespace", namespace)?;
        validate_forwarded_argv(argv, inputs.len(), outputs.len())?;
        validate_runtime_config_inputs(runtime_config_inputs, inputs.len())?;
        self.ensure_ready()?;
        let input_identities = inputs
            .iter()
            .map(|source| {
                file_identity(source)
                    .with_context(|| format!("cannot inspect input {}", source.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let operation_id = Uuid::now_v7();
        let binary = guest_binary_path();
        let mut prepare = vec![
            binary.clone(),
            "__internal-vm-prepare".to_owned(),
            "--operation-id".to_owned(),
            operation_id.to_string(),
            "--namespace".to_owned(),
            namespace.to_owned(),
            "--input-identities".to_owned(),
            serde_json::to_string(&input_identities)?,
            "--runtime-config-inputs".to_owned(),
            serde_json::to_string(runtime_config_inputs)?,
            "--output-count".to_owned(),
            outputs.len().to_string(),
            "--".to_owned(),
        ];
        prepare.extend(argv.iter().cloned());
        self.guest_status(prepare.iter().map(String::as_str))?;

        let stage_result = self.stage_inputs(operation_id, inputs, &input_identities);
        if let Err(error) = stage_result {
            let _ = self.abandon_operation(operation_id);
            return Err(error);
        }
        self.guest_status([
            binary.as_str(),
            "__internal-vm-start",
            "--operation-id",
            &operation_id.to_string(),
        ])
        .with_context(|| {
            format!(
                "guest operation {operation_id} has uncertain start status; inspect it with `runlab vm operation get {operation_id}`"
            )
        })?;
        let started = VmOperationResult {
            schema_version: 1,
            instance: self.instance.clone(),
            operation_id,
            namespace: namespace.to_owned(),
            detached: detach,
            runtime_config_inputs: runtime_config_inputs.to_vec(),
        };
        if detach {
            return Ok((started, None));
        }
        let attached = self.attach(operation_id, outputs).with_context(|| {
            format!(
                "guest operation {operation_id} remains available for `runlab vm operation attach {operation_id}`"
            )
        })?;
        Ok((started, Some(attached)))
    }

    pub fn operation_status(&self, operation_id: Uuid) -> Result<VmOperationStatus> {
        self.ensure_ready()?;
        let binary = guest_binary_path();
        self.guest_json([
            binary.as_str(),
            "__internal-vm-status",
            "--operation-id",
            &operation_id.to_string(),
        ])
    }

    pub fn cancel(&self, operation_id: Uuid) -> Result<VmCancelResult> {
        self.ensure_ready()?;
        let binary = guest_binary_path();
        self.guest_json([
            binary.as_str(),
            "__internal-vm-cancel",
            "--operation-id",
            &operation_id.to_string(),
        ])
    }

    pub fn discard(&self, operation_id: Uuid) -> Result<VmDiscardResult> {
        self.ensure_ready()?;
        let binary = guest_binary_path();
        self.guest_json([
            binary.as_str(),
            "__internal-vm-discard",
            "--operation-id",
            &operation_id.to_string(),
        ])
    }

    pub fn attach(&self, operation_id: Uuid, outputs: &[PathBuf]) -> Result<AttachedOperation> {
        self.ensure_ready()?;
        let interrupted = Arc::new(AtomicBool::new(false));
        let registrations = register_interrupts(Arc::clone(&interrupted))?;
        let status = loop {
            if interrupted.swap(false, Ordering::SeqCst) {
                let _ = self.cancel(operation_id);
            }
            let status = self.operation_status(operation_id)?;
            if status.terminal {
                break status;
            }
            thread::sleep(Duration::from_millis(300));
        };
        unregister_interrupts(registrations);

        let stdout = self.read_operation_stream(operation_id, "stdout", MAX_GUEST_STREAM)?;
        let stderr = self.read_operation_stream(operation_id, "stderr", MAX_GUEST_STREAM)?;
        if outputs.len() != status.output_count {
            bail!(
                "operation {operation_id} requires exactly {} output destinations",
                status.output_count
            );
        }
        self.copy_outputs(operation_id, outputs)?;
        Ok(AttachedOperation {
            operation_id,
            status,
            stdout,
            stderr,
        })
    }

    pub fn complete(&self, operation_id: Uuid) -> Result<()> {
        self.remove_operation(operation_id)
    }

    fn inspect_instance(&self) -> Result<LimaInstance> {
        self.validate_limactl_version()?;
        let output = self.limactl_output([
            OsStr::new("list"),
            OsStr::new(&self.instance),
            OsStr::new("--json"),
        ])?;
        serde_json::from_slice(&output.stdout).context("Lima returned invalid instance metadata")
    }

    fn validate_instance(&self, instance: &LimaInstance) -> Result<()> {
        ensure!(
            instance.name == self.instance,
            "Lima returned a different instance"
        );
        ensure!(instance.vm_type == "vz", "managed VM must use Lima VZ");
        ensure!(
            instance.lima_version == format!("v{LIMA_VERSION}"),
            "managed VM must use Lima {LIMA_VERSION}, found {}",
            instance.lima_version
        );
        ensure!(instance.config.plain, "managed VM must use Lima plain mode");
        ensure!(
            instance.config.mounts.is_empty(),
            "managed VM must not have host filesystem mounts"
        );
        ensure!(
            normalize_architecture(&instance.arch) == normalize_architecture(env::consts::ARCH),
            "managed VM architecture {} does not match host {}",
            instance.arch,
            env::consts::ARCH
        );
        let _ = selected_instance_image(instance)?;
        Ok(())
    }

    fn validate_limactl_version(&self) -> Result<()> {
        let output = self.limactl_output(["--version"])?;
        let version = std::str::from_utf8(&output.stdout)
            .context("limactl version output is not UTF-8")?
            .trim();
        ensure!(
            version == format!("limactl version {LIMA_VERSION}"),
            "managed VM requires limactl {LIMA_VERSION}, found {version}"
        );
        Ok(())
    }

    fn ensure_running(&self) -> Result<()> {
        let status = self.start()?;
        ensure!(
            status.status == "Running",
            "managed VM did not reach Running state"
        );
        Ok(())
    }

    fn ensure_ready(&self) -> Result<VmHandshake> {
        let status = self.start()?;
        ensure!(
            status.status == "Running",
            "managed VM did not reach Running state"
        );
        let handshake = status.handshake.context(
            status
                .handshake_error
                .unwrap_or_else(|| "guest RunLab handshake is unavailable".to_owned()),
        )?;
        let _ = status.runc.context(
            status
                .runc_error
                .unwrap_or_else(|| "guest runc identity is unavailable".to_owned()),
        )?;
        let profile = status.reference_profile.context(
            status
                .reference_profile_error
                .unwrap_or_else(|| "guest reference profile is unavailable".to_owned()),
        )?;
        validate_reference_profile(&profile)?;
        Ok(handshake)
    }

    fn handshake(&self) -> Result<VmHandshake> {
        self.handshake_at(&guest_binary_path())
    }

    fn handshake_at(&self, binary: &str) -> Result<VmHandshake> {
        let handshake: VmHandshake = self.guest_json([binary, "__internal-vm-handshake"])?;
        validate_handshake(&handshake)?;
        Ok(handshake)
    }

    fn runc_identity_at(&self, binary: &str) -> Result<VmRuncIdentity> {
        let output = self.guest_output([binary, "--version"])?;
        let identity = parse_runc_identity(&output.stdout)?;
        validate_runc_identity(&identity)?;
        Ok(identity)
    }

    fn provision_reference_profile(&self, operation_id: Uuid) -> Result<VmReferenceProfile> {
        if self.guest_package_version("conntrack")?.as_deref() != Some(CONNTRACK_PACKAGE_VERSION) {
            self.guest_status(["/usr/bin/sudo", "/usr/bin/apt-get", "update"])?;
            self.guest_status([
                "/usr/bin/sudo",
                "/usr/bin/env",
                "DEBIAN_FRONTEND=noninteractive",
                "/usr/bin/apt-get",
                "install",
                "--yes",
                "--no-install-recommends",
                "--allow-downgrades",
                &format!("conntrack={CONNTRACK_PACKAGE_VERSION}"),
            ])?;
        }

        ensure!(
            self.guest_package_version("conntrack")?.as_deref() == Some(CONNTRACK_PACKAGE_VERSION),
            "managed VM requires conntrack package {CONNTRACK_PACKAGE_VERSION}"
        );

        self.publish_profile_file(operation_id, "modules", MODULES_CONFIG_PATH, MODULES_CONFIG)?;
        self.publish_profile_file(operation_id, "sysctl", SYSCTL_CONFIG_PATH, SYSCTL_CONFIG)?;
        self.guest_status(["/usr/bin/sudo", "/usr/sbin/modprobe", "overlay"])?;
        self.guest_status([
            "/usr/bin/sudo",
            "/usr/sbin/sysctl",
            "-p",
            SYSCTL_CONFIG_PATH,
        ])?;
        let profile = self.reference_profile()?;
        validate_reference_profile(&profile)?;
        Ok(profile)
    }

    fn publish_profile_file(
        &self,
        operation_id: Uuid,
        label: &str,
        target: &str,
        contents: &[u8],
    ) -> Result<()> {
        let mut local = NamedTempFile::new()
            .with_context(|| format!("cannot stage the managed VM {label} policy"))?;
        local.write_all(contents)?;
        local.flush()?;
        local.as_file().sync_all()?;
        let local_identity = file_identity(local.path())?;
        let staged = format!("/var/tmp/runlab-{label}-install-{operation_id}");
        let target_path = Path::new(target);
        let parent = target_path
            .parent()
            .context("managed VM profile target has no parent")?;
        let temporary = parent.join(format!(".runlab-{label}-{operation_id}"));
        let temporary = temporary
            .to_str()
            .context("managed VM profile temporary path is not UTF-8")?;
        let result = (|| -> Result<()> {
            self.copy_to_guest(local.path(), &staged)?;
            let remote_identity = self.remote_file_identity(&staged)?;
            ensure!(
                remote_identity.digest == local_identity.digest
                    && remote_identity.size == local_identity.size,
                "staged managed VM {label} policy failed digest or size verification"
            );
            self.guest_status([
                "/usr/bin/sudo",
                "/usr/bin/install",
                "-m",
                "0644",
                &staged,
                temporary,
            ])?;
            self.guest_status(["/usr/bin/sudo", "/usr/bin/mv", temporary, target])?;
            let persisted = self.guest_output(["/usr/bin/sudo", "/usr/bin/cat", "--", target])?;
            ensure!(
                persisted.stdout == contents,
                "managed VM {label} policy changed during publication"
            );
            Ok(())
        })();
        let _ = self.guest_status(["/usr/bin/rm", "-f", &staged]);
        let _ = self.guest_status(["/usr/bin/sudo", "/usr/bin/rm", "-f", temporary]);
        result
    }

    fn reference_profile(&self) -> Result<VmReferenceProfile> {
        let iproute2 = self.guest_package_version("iproute2")?;
        let nftables = self.guest_package_version("nftables")?;
        let conntrack = self.guest_package_version("conntrack")?;
        let util_linux = self.guest_package_version("util-linux")?;
        let coreutils = self.guest_package_version("coreutils")?;
        let kmod = self.guest_package_version("kmod")?;
        let systemd_version = self.guest_package_version("systemd")?;
        let tools = VmReferenceTools {
            ip: self.executable_fact("/usr/sbin/ip", iproute2.clone())?,
            nft: self.executable_fact("/usr/sbin/nft", nftables)?,
            conntrack: self.executable_fact("/usr/sbin/conntrack", conntrack)?,
            unshare: self.executable_fact("/usr/bin/unshare", util_linux.clone())?,
            nsenter: self.executable_fact("/usr/bin/nsenter", util_linux)?,
            cat: self.executable_fact("/usr/bin/cat", coreutils)?,
            modprobe: self.executable_fact("/usr/sbin/modprobe", kmod)?,
            systemd_run: self.executable_fact("/usr/bin/systemd-run", systemd_version.clone())?,
            systemctl: self.executable_fact("/usr/bin/systemctl", systemd_version)?,
        };
        let cgroup_version = match self
            .guest_trimmed(["/usr/bin/stat", "-fc", "%T", "/sys/fs/cgroup"])?
            .as_str()
        {
            "cgroup2fs" => Some(2),
            "tmpfs" => Some(1),
            _ => None,
        };
        let overlayfs_active =
            self.guest_command_success(["/usr/bin/grep", "-qw", "overlay", "/proc/filesystems"])?;
        let overlayfs_configured = self.guest_file_equals(MODULES_CONFIG_PATH, MODULES_CONFIG)?;
        let systemd = self.guest_command_success(["/usr/bin/test", "-d", "/run/systemd/system"])?;
        let ipv4_forwarding_active =
            self.guest_trimmed(["/usr/bin/cat", "/proc/sys/net/ipv4/ip_forward"])? == "1";
        let ipv4_forwarding_configured =
            self.guest_file_equals(SYSCTL_CONFIG_PATH, SYSCTL_CONFIG)?;
        let ready = tools.all_executable()
            && tools.conntrack.package_version.as_deref() == Some(CONNTRACK_PACKAGE_VERSION)
            && cgroup_version == Some(2)
            && overlayfs_active
            && overlayfs_configured
            && systemd
            && ipv4_forwarding_active
            && ipv4_forwarding_configured;
        Ok(VmReferenceProfile {
            ready,
            tools,
            kernel: VmKernelFacts {
                cgroup_version,
                overlayfs: VmKernelFeatureFacts {
                    active: overlayfs_active,
                    configured: overlayfs_configured,
                },
                ipv4_forwarding: VmKernelFeatureFacts {
                    active: ipv4_forwarding_active,
                    configured: ipv4_forwarding_configured,
                },
            },
            systemd,
        })
    }

    fn executable_fact(
        &self,
        path: &str,
        package_version: Option<String>,
    ) -> Result<VmExecutableFact> {
        Ok(VmExecutableFact {
            path: path.to_owned(),
            executable: self.guest_command_success(["/usr/bin/test", "-x", path])?,
            package_version,
        })
    }

    fn guest_package_version(&self, package: &str) -> Result<Option<String>> {
        let output =
            self.guest_unchecked_output(["/usr/bin/dpkg-query", "-W", "-f=${Version}", package])?;
        if output.status.success() {
            return Ok(Some(
                std::str::from_utf8(&output.stdout)
                    .context("dpkg-query output is not UTF-8")?
                    .trim()
                    .to_owned(),
            ));
        }
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        ensure_status(&output, "dpkg-query")?;
        unreachable!("non-success dpkg-query status was rejected")
    }

    fn guest_trimmed<I, S>(&self, arguments: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.guest_output(arguments)?;
        Ok(std::str::from_utf8(&output.stdout)
            .context("guest fact output is not UTF-8")?
            .trim()
            .to_owned())
    }

    fn guest_command_success<I, S>(&self, arguments: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.guest_unchecked_output(arguments)?;
        if output.status.success() {
            return Ok(true);
        }
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        ensure_status(&output, "Lima guest fact command")?;
        unreachable!("non-success guest fact status was rejected")
    }

    fn guest_file_equals(&self, path: &str, expected: &[u8]) -> Result<bool> {
        let output = self.guest_unchecked_output(["/usr/bin/cat", "--", path])?;
        if output.status.success() {
            return Ok(output.stdout == expected);
        }
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        ensure_status(&output, "Lima guest fact file read")?;
        unreachable!("non-success guest fact file status was rejected")
    }

    fn stage_inputs(
        &self,
        operation_id: Uuid,
        inputs: &[PathBuf],
        identities: &[FileIdentity],
    ) -> Result<()> {
        ensure!(
            inputs.len() == identities.len(),
            "input identity count mismatch"
        );
        for (index, (source, local)) in inputs.iter().zip(identities).enumerate() {
            let remote_path = operation_file(operation_id, "input", index);
            self.copy_to_guest(source, &remote_path)?;
            let remote = self.remote_operation_file_identity(operation_id, "input", index)?;
            ensure!(
                local.digest == remote.digest && local.size == remote.size,
                "staged input {index} failed digest or size verification"
            );
        }
        Ok(())
    }

    fn stage_output(
        &self,
        operation_id: Uuid,
        index: usize,
        destination: &Path,
    ) -> Result<StagedOutput> {
        ensure!(
            !destination.exists(),
            "output already exists: {}",
            destination.display()
        );
        let remote = self.remote_operation_file_identity(operation_id, "output", index)?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temporary = NamedTempFile::new_in(parent)
            .with_context(|| format!("cannot stage output beside {}", destination.display()))?;
        let binary = guest_binary_path();
        let output = Command::new(&self.limactl)
            .args([
                "--tty=false",
                "shell",
                &self.instance,
                &binary,
                "__internal-vm-read-file",
                "--operation-id",
                &operation_id.to_string(),
                "--kind",
                "output",
                "--index",
                &index.to_string(),
            ])
            .stdout(Stdio::from(temporary.reopen()?))
            .stderr(Stdio::piped())
            .output()
            .context("failed to read staged guest output")?;
        ensure_status(&output, "guest output transfer")?;
        temporary.flush()?;
        let local = file_identity(temporary.path())?;
        ensure!(
            local.digest == remote.digest && local.size == remote.size,
            "retrieved output {index} failed digest or size verification"
        );
        Ok(StagedOutput {
            temporary,
            destination: destination.to_owned(),
        })
    }

    fn copy_outputs(&self, operation_id: Uuid, outputs: &[PathBuf]) -> Result<()> {
        let unique = outputs.iter().collect::<BTreeSet<_>>();
        ensure!(
            unique.len() == outputs.len(),
            "output destinations must be distinct"
        );
        let staged = outputs
            .iter()
            .enumerate()
            .map(|(index, destination)| self.stage_output(operation_id, index, destination))
            .collect::<Result<Vec<_>>>()?;
        publish_staged_outputs(staged)
    }

    fn read_operation_stream(
        &self,
        operation_id: Uuid,
        stream: &str,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let binary = guest_binary_path();
        let identity: FileIdentity = self.guest_json([
            binary.as_str(),
            "__internal-vm-stream-info",
            "--operation-id",
            &operation_id.to_string(),
            "--stream",
            stream,
        ])?;
        ensure!(
            identity.size <= u64::try_from(limit).expect("stream limit fits in u64"),
            "guest {stream} exceeds the {limit}-byte transport limit"
        );
        let output = self.guest_output([
            binary.as_str(),
            "__internal-vm-read-stream",
            "--operation-id",
            &operation_id.to_string(),
            "--stream",
            stream,
        ])?;
        ensure!(
            output.stdout.len() == usize::try_from(identity.size)?,
            "guest {stream} changed size during transfer"
        );
        let mut hasher = Sha256::new();
        hasher.update(&output.stdout);
        let digest = finish_sha256(hasher);
        ensure!(
            digest == identity.digest,
            "guest {stream} changed during transfer"
        );
        Ok(output.stdout)
    }

    fn remote_operation_file_identity(
        &self,
        operation_id: Uuid,
        kind: &str,
        index: usize,
    ) -> Result<FileIdentity> {
        let binary = guest_binary_path();
        self.guest_json([
            binary.as_str(),
            "__internal-vm-file-info",
            "--operation-id",
            &operation_id.to_string(),
            "--kind",
            kind,
            "--index",
            &index.to_string(),
        ])
    }

    fn remote_file_identity(&self, path: &str) -> Result<FileIdentity> {
        let output = self.guest_output(["/usr/bin/sha256sum", "--", path])?;
        let text = std::str::from_utf8(&output.stdout).context("sha256sum output is not UTF-8")?;
        let hexadecimal = text
            .split_ascii_whitespace()
            .next()
            .context("sha256sum omitted the digest")?;
        let digest = format!("sha256:{hexadecimal}").parse()?;
        let size_output = self.guest_output(["/usr/bin/stat", "--format=%s", "--", path])?;
        let size = std::str::from_utf8(&size_output.stdout)?
            .trim()
            .parse()
            .context("stat returned an invalid size")?;
        Ok(FileIdentity {
            schema_version: 1,
            digest,
            size,
        })
    }

    fn remove_operation(&self, operation_id: Uuid) -> Result<()> {
        let binary = guest_binary_path();
        self.guest_status([
            binary.as_str(),
            "__internal-vm-remove",
            "--operation-id",
            &operation_id.to_string(),
        ])
    }

    fn abandon_operation(&self, operation_id: Uuid) -> Result<()> {
        let binary = guest_binary_path();
        self.guest_status([
            binary.as_str(),
            "__internal-vm-abandon",
            "--operation-id",
            &operation_id.to_string(),
        ])
    }

    fn copy_to_guest(&self, source: &Path, destination: &str) -> Result<()> {
        let remote = format!("{}:{destination}", self.instance);
        self.limactl_status([
            OsStr::new("copy"),
            OsStr::new("--backend=scp"),
            source.as_os_str(),
            OsStr::new(&remote),
        ])
    }

    fn guest_json<T: for<'de> Deserialize<'de>, I, S>(&self, arguments: I) -> Result<T>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.guest_output(arguments)?;
        ensure!(
            output.stdout.len() <= MAX_CONTROL_OUTPUT,
            "guest control response exceeds the protocol limit"
        );
        serde_json::from_slice(&output.stdout).context("guest returned an invalid control response")
    }

    fn guest_status<I, S>(&self, arguments: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.guest_output(arguments).map(|_| ())
    }

    fn guest_output<I, S>(&self, arguments: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.guest_unchecked_output(arguments)?;
        ensure_status(&output, "Lima guest command")?;
        Ok(output)
    }

    fn guest_unchecked_output<I, S>(&self, arguments: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.limactl);
        command.args(["--tty=false", "shell", &self.instance]);
        command.args(arguments);
        command
            .output()
            .context("failed to invoke Lima guest command")
    }

    fn limactl_output<I, S>(&self, arguments: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new(&self.limactl)
            .args(arguments)
            .output()
            .context("failed to invoke limactl")?;
        ensure_status(&output, "limactl")?;
        Ok(output)
    }

    fn limactl_status<I, S>(&self, arguments: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.limactl_output(arguments).map(|_| ())
    }

    fn limactl_output_with_stdin<I, S>(&self, arguments: I, stdin: &[u8]) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = Command::new(&self.limactl)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to invoke limactl")?;
        child
            .stdin
            .take()
            .context("limactl stdin is unavailable")?
            .write_all(stdin)?;
        let output = child
            .wait_with_output()
            .context("failed to wait for limactl")?;
        ensure_status(&output, "limactl")?;
        Ok(output)
    }
}

fn publish_staged_outputs(staged: Vec<StagedOutput>) -> Result<()> {
    let mut published: Vec<PathBuf> = Vec::new();
    for output in staged {
        let destination = output.destination.clone();
        if let Err(error) = output.temporary.persist_noclobber(&destination) {
            let publication_error = anyhow::Error::new(error.error)
                .context(format!("cannot persist output {}", destination.display()));
            let mut rollback_errors = Vec::new();
            for path in published.iter().rev() {
                if let Err(error) = fs::remove_file(path) {
                    rollback_errors.push(format!("{}: {error}", path.display()));
                }
            }
            if !rollback_errors.is_empty() {
                return Err(publication_error.context(format!(
                    "output publication rollback failed: {}",
                    rollback_errors.join("; ")
                )));
            }
            return Err(publication_error);
        }
        published.push(destination);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(directory: &Path, destination: &str, bytes: &[u8]) -> StagedOutput {
        let mut temporary = NamedTempFile::new_in(directory).unwrap();
        temporary.write_all(bytes).unwrap();
        StagedOutput {
            temporary,
            destination: directory.join(destination),
        }
    }

    #[test]
    fn publication_failure_rolls_back_every_preceding_destination() {
        let directory = tempfile::tempdir().unwrap();
        let collision = directory.path().join("collision");
        fs::write(&collision, b"owner").unwrap();
        let outputs = vec![
            staged(directory.path(), "first", b"one"),
            staged(directory.path(), "second", b"two"),
            staged(directory.path(), "collision", b"three"),
        ];

        assert!(publish_staged_outputs(outputs).is_err());
        assert!(!directory.path().join("first").exists());
        assert!(!directory.path().join("second").exists());
        assert_eq!(fs::read(collision).unwrap(), b"owner");
    }

    #[test]
    fn publication_publishes_every_staged_output() {
        let directory = tempfile::tempdir().unwrap();
        let outputs = vec![
            staged(directory.path(), "first", b"one"),
            staged(directory.path(), "second", b"two"),
        ];

        publish_staged_outputs(outputs).unwrap();
        assert_eq!(fs::read(directory.path().join("first")).unwrap(), b"one");
        assert_eq!(fs::read(directory.path().join("second")).unwrap(), b"two");
    }
}
