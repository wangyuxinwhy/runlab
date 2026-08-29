use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::num::NonZeroU64;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde_json::Value;
use uuid::Uuid;

use super::host::{
    INSTANCE, ManagedVm, STATE_PATH, ensure_remote_identity, ensure_success, file_identity,
    guest_binary_path,
};
use crate::cli::run::SecretFileArg;
use crate::metadata::Metadata;

const ERROR_TAIL_LIMIT: usize = 256 * 1024;

pub(crate) struct ForwardedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) struct ForwardRunStart<'a> {
    pub(crate) id: &'a str,
    pub(crate) detached_worker: bool,
    pub(crate) image: &'a str,
    pub(crate) metadata: &'a Metadata,
    pub(crate) runtime_config: Option<&'a Path>,
    pub(crate) stdin: Option<&'a Path>,
    pub(crate) secret_env: &'a [String],
    pub(crate) secret_files: &'a [SecretFileArg],
    pub(crate) execution_timeout_ms: Option<NonZeroU64>,
    pub(crate) network: &'a str,
}

pub(crate) struct ForwardExecution<'a> {
    pub(crate) image: &'a str,
    pub(crate) runtime_config: Option<&'a Path>,
    pub(crate) stdin: Option<&'a Path>,
    pub(crate) secret_env: &'a [String],
    pub(crate) secret_files: &'a [SecretFileArg],
    pub(crate) execution_timeout_ms: Option<NonZeroU64>,
    pub(crate) network: &'a str,
}

struct ForwardedMount {
    source: String,
    destination: String,
}

impl From<Output> for ForwardedOutput {
    fn from(output: Output) -> Self {
        Self {
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

impl ManagedVm {
    pub(crate) fn forward_storage(&self, arguments: &[&str]) -> Result<ForwardedOutput> {
        self.state_command(arguments.iter().copied())
            .map(Into::into)
    }

    pub(crate) fn forward_image_import(
        &self,
        source: &Path,
        name: &str,
        metadata: &Metadata,
    ) -> Result<ForwardedOutput> {
        let archive = archive_if_directory(source)?;
        let source = archive
            .as_ref()
            .map_or(source, tempfile::NamedTempFile::path);
        let staged = self.stage_input(source, "image")?;
        let mut arguments = vec![
            OsString::from("image"),
            OsString::from("import"),
            OsString::from(&staged),
            OsString::from("--name"),
            OsString::from(name),
        ];
        append_metadata_arguments(&mut arguments, metadata);
        let output = self.state_command(arguments);
        self.cleanup_inputs(&[&staged]);
        output.map(Into::into)
    }

    pub(crate) fn forward_image_list(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<ForwardedOutput> {
        let mut arguments: Vec<OsString> = vec![
            "image".into(),
            "list".into(),
            "--limit".into(),
            limit.to_string().into(),
        ];
        if let Some(after) = after {
            arguments.extend(["--after".into(), after.into()]);
        }
        self.state_command(arguments).map(Into::into)
    }

    pub(crate) fn forward_image_get(&self, image: &str) -> Result<ForwardedOutput> {
        self.state_command(["image", "get", image]).map(Into::into)
    }

    pub(crate) fn forward_image_export(
        &self,
        image: Option<&str>,
        run: Option<&str>,
        program: Option<&str>,
        output: &Path,
    ) -> Result<ForwardedOutput> {
        let destination = new_output_path(output)?;
        let remote = format!("/var/tmp/runlab-image-export-{}.tar", Uuid::new_v4());
        let mut arguments: Vec<OsString> = vec!["image".into(), "export".into()];
        match (image, run) {
            (Some(image), None) => arguments.extend(["--image".into(), image.into()]),
            (None, Some(run)) => arguments.extend(["--run".into(), run.into()]),
            _ => bail!("exactly one of --image or --run is required"),
        }
        if let Some(program) = program {
            arguments.extend(["--program".into(), program.into()]);
        }
        arguments.extend(["--output".into(), remote.clone().into()]);
        let result = (|| {
            let guest_output = self.state_command(arguments)?;
            let mut value: Value = serde_json::from_slice(&guest_output.stdout)
                .context("guest image export returned invalid JSON")?;
            self.transfer_remote_file_ownership(&remote)?;
            let identity = self.remote_file_identity(&remote)?;
            let archive = self.copy_output_archive(
                &remote,
                &identity,
                destination
                    .parent()
                    .expect("normalized output has a parent"),
            )?;
            archive.persist_noclobber(&destination).map_err(|error| {
                anyhow::anyhow!(
                    "cannot publish OCI archive {} without overwriting: {}",
                    destination.display(),
                    error.error
                )
            })?;
            value["output"] = Value::String(destination.display().to_string());
            let mut stdout = serde_json::to_vec(&value)?;
            stdout.push(b'\n');
            Ok(ForwardedOutput {
                stdout,
                stderr: guest_output.stderr,
            })
        })();
        let _ = self.guest_success(["/usr/bin/sudo", "/usr/bin/rm", "-f", "--", &remote]);
        result
    }

    pub(crate) fn forward_run_config(&self, image: &str) -> Result<ForwardedOutput> {
        self.state_command(["run", "config", "generate", "--image", image])
            .map(Into::into)
    }

    pub(crate) fn forward_run_start(
        &self,
        request: &ForwardRunStart<'_>,
    ) -> Result<ForwardedOutput> {
        let mut arguments = vec![
            "run".into(),
            "start".into(),
            "--id".into(),
            request.id.into(),
        ];
        if request.detached_worker {
            arguments.push("--detached-worker".into());
        }
        self.forward_execution(
            arguments,
            &ForwardExecution {
                image: request.image,
                runtime_config: request.runtime_config,
                stdin: request.stdin,
                secret_env: request.secret_env,
                secret_files: request.secret_files,
                execution_timeout_ms: request.execution_timeout_ms,
                network: request.network,
            },
            Some(request.metadata),
        )
    }

    pub(crate) fn forward_exec(&self, request: &ForwardExecution<'_>) -> Result<ForwardedOutput> {
        self.forward_execution(vec!["exec".into()], request, None)
    }

    fn forward_execution(
        &self,
        mut command_arguments: Vec<OsString>,
        request: &ForwardExecution<'_>,
        metadata: Option<&Metadata>,
    ) -> Result<ForwardedOutput> {
        self.ensure_ready()?;
        let mut staged = Vec::new();
        let arguments = (|| {
            let mounts = self.stage_host_mounts(request.runtime_config, &mut staged)?;
            let runtime_config = request
                .runtime_config
                .map(|path| self.stage_input(path, "runtime-config"))
                .transpose()?;
            if let Some(path) = &runtime_config {
                staged.push(path.clone());
            }
            let stdin = request
                .stdin
                .map(|path| self.stage_input(path, "stdin"))
                .transpose()?;
            if let Some(path) = &stdin {
                staged.push(path.clone());
            }

            let mut secret_environment = Vec::new();
            for name in request.secret_env {
                let value = env::var(name).with_context(|| {
                    format!("Secret environment variable is unavailable: {name}")
                })?;
                let mut temporary = tempfile::NamedTempFile::new()
                    .context("failed to stage Secret environment value")?;
                temporary
                    .write_all(value.as_bytes())
                    .context("failed to stage Secret environment value")?;
                temporary
                    .flush()
                    .context("failed to stage Secret environment value")?;
                let remote = self.stage_secret_input(temporary.path(), "secret-env")?;
                staged.push(remote.clone());
                secret_environment.push((name, remote));
            }

            let mut secret_file_sources = Vec::new();
            for secret in request.secret_files {
                let remote = self.stage_secret_input(&secret.source, "secret-file")?;
                staged.push(remote.clone());
                secret_file_sources.push((&secret.destination, remote));
            }

            command_arguments.extend([
                "--image".into(),
                request.image.into(),
                "--network".into(),
                request.network.into(),
            ]);
            if let Some(metadata) = metadata {
                append_metadata_arguments(&mut command_arguments, metadata);
            }
            if let Some(path) = &runtime_config {
                command_arguments.extend(["--runtime-config".into(), path.into()]);
            }
            if let Some(path) = &stdin {
                command_arguments.extend(["--stdin".into(), path.into()]);
            }
            for (name, source) in secret_environment {
                command_arguments.extend([
                    "--secret-env-file".into(),
                    format!("{name}={source}").into(),
                ]);
            }
            for (destination, source) in secret_file_sources {
                command_arguments.extend([
                    "--secret-file".into(),
                    format!("{source}={destination}").into(),
                ]);
            }
            if let Some(timeout) = request.execution_timeout_ms {
                command_arguments.extend([
                    "--execution-timeout-ms".into(),
                    timeout.get().to_string().into(),
                ]);
            }
            Ok((command_arguments, mounts))
        })();
        let (arguments, mounts) = match arguments {
            Ok(arguments) => arguments,
            Err(error) => {
                self.cleanup_staged(&staged.iter().collect::<Vec<_>>());
                return Err(error);
            }
        };
        let references = staged.iter().collect::<Vec<_>>();
        let output = self.systemd_state_command_streaming(arguments, &references, &mounts);
        self.cleanup_staged(&references);
        output
    }

    pub(crate) fn forward_run_get(&self, id: &str) -> Result<ForwardedOutput> {
        self.state_command(["run", "get", id]).map(Into::into)
    }

    pub(crate) fn forward_run_cancel(&self, id: &str) -> Result<ForwardedOutput> {
        self.state_command(["run", "cancel", id]).map(Into::into)
    }

    pub(crate) fn forward_run_reconcile(&self, id: &str) -> Result<ForwardedOutput> {
        self.state_command(["run", "reconcile", id]).map(Into::into)
    }

    pub(crate) fn forward_run_list(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<ForwardedOutput> {
        let mut arguments: Vec<OsString> = vec![
            "run".into(),
            "list".into(),
            "--limit".into(),
            limit.to_string().into(),
        ];
        if let Some(after) = after {
            arguments.extend(["--after".into(), after.into()]);
        }
        self.state_command(arguments).map(Into::into)
    }

    pub(crate) fn forward_schema_list(&self) -> Result<ForwardedOutput> {
        self.state_command(["schema", "list"]).map(Into::into)
    }

    pub(crate) fn forward_schema_get(
        &self,
        object: &str,
        compact: bool,
    ) -> Result<ForwardedOutput> {
        let mut arguments: Vec<OsString> = vec!["schema".into(), "get".into(), object.into()];
        if compact {
            arguments.push("--compact".into());
        }
        self.state_command(arguments).map(Into::into)
    }

    pub(crate) fn forward_query_run(
        &self,
        sql: &str,
        limit: usize,
        max_cell_bytes: usize,
        max_output_bytes: usize,
        timeout_seconds: u64,
    ) -> Result<ForwardedOutput> {
        let mut local = tempfile::NamedTempFile::new().context("failed to stage SQL input")?;
        local
            .write_all(sql.as_bytes())
            .context("failed to stage SQL input")?;
        local.flush().context("failed to stage SQL input")?;
        let remote = self.stage_input(local.path(), "query")?;
        let arguments: Vec<OsString> = vec![
            "query".into(),
            "run".into(),
            "--file".into(),
            remote.clone().into(),
            "--limit".into(),
            limit.to_string().into(),
            "--max-cell-bytes".into(),
            max_cell_bytes.to_string().into(),
            "--max-output-bytes".into(),
            max_output_bytes.to_string().into(),
            "--timeout-seconds".into(),
            timeout_seconds.to_string().into(),
        ];
        let output = self.state_command(arguments);
        self.cleanup_inputs(&[&remote]);
        output.map(Into::into)
    }

    pub(crate) fn forward_filesystem_get(
        &self,
        run: Option<&str>,
        image: Option<&str>,
        program: Option<&str>,
        path: &str,
        destination: &Path,
    ) -> Result<ForwardedOutput> {
        let destination = new_output_path(destination)?;
        let remote_root = format!("/var/tmp/runlab-output-{}", Uuid::new_v4());
        let remote_output = format!("{remote_root}/payload");
        let remote_archive = format!("{remote_root}.tar");
        let mut arguments: Vec<OsString> = vec!["filesystem".into(), "get".into()];
        match (run, image) {
            (Some(run), None) => arguments.extend(["--run".into(), run.into()]),
            (None, Some(image)) => arguments.extend(["--image".into(), image.into()]),
            _ => bail!("exactly one of --run or --image is required"),
        }
        if let Some(program) = program {
            arguments.extend(["--program".into(), program.into()]);
        }
        arguments.extend([path.into(), "--output".into(), remote_output.clone().into()]);
        let result = (|| {
            self.guest_success([
                "/usr/bin/sudo",
                "/usr/bin/install",
                "-d",
                "-m",
                "0700",
                &remote_root,
            ])?;
            let guest_output = self.state_command(arguments)?;
            let mut result: Value = serde_json::from_slice(&guest_output.stdout)
                .context("guest filesystem get returned invalid JSON")?;
            self.guest_success([
                "/usr/bin/sudo",
                "/usr/bin/tar",
                "--format=pax",
                "-cf",
                &remote_archive,
                "-C",
                &remote_root,
                "--",
                "payload",
            ])?;
            let archive_identity = self.remote_file_identity(&remote_archive)?;
            let archive = self.copy_output_archive(
                &remote_archive,
                &archive_identity,
                destination
                    .parent()
                    .expect("normalized output has a parent"),
            )?;
            publish_filesystem_archive(archive.path(), &result, &destination)?;
            result["output"] = Value::String(destination.display().to_string());
            let mut stdout = serde_json::to_vec(&result)?;
            stdout.push(b'\n');
            Ok(ForwardedOutput {
                stdout,
                stderr: guest_output.stderr,
            })
        })();
        let _ = self.guest_success([
            "/usr/bin/sudo",
            "/usr/bin/rm",
            "-rf",
            "--",
            &remote_root,
            &remote_archive,
        ]);
        result
    }

    pub(crate) fn forward_filesystem_changes(
        &self,
        run: &str,
        program: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<ForwardedOutput> {
        let mut arguments: Vec<OsString> = vec![
            "filesystem".into(),
            "changes".into(),
            "--run".into(),
            run.into(),
            "--program".into(),
            program.into(),
            "--limit".into(),
            limit.to_string().into(),
        ];
        if let Some(after) = after {
            arguments.extend(["--after".into(), after.into()]);
        }
        self.state_command(arguments).map(Into::into)
    }

    fn state_command<I, S>(&self, arguments: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.ensure_ready()?;
        let mut command = vec![
            OsString::from("/usr/bin/sudo"),
            OsString::from(guest_binary_path()),
            OsString::from("--state"),
            OsString::from(STATE_PATH),
        ];
        command.extend(arguments.into_iter().map(|value| value.as_ref().to_owned()));
        self.guest_output(command)
    }

    fn systemd_state_command_streaming(
        &self,
        arguments: Vec<OsString>,
        cleanup: &[&String],
        mounts: &[ForwardedMount],
    ) -> Result<ForwardedOutput> {
        self.ensure_ready()?;
        let unit = format!("runlab-execution-{}", Uuid::new_v4());
        let forwarding = GuestSignalForwarding::install(&self.limactl, &unit)?;
        let mut command = Command::new(&self.limactl);
        command.args(["shell", "--tty=false", INSTANCE, "--"]);
        command.args(Self::systemd_state_arguments(
            arguments, cleanup, mounts, &unit,
        ));
        command.process_group(0);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                forwarding.close()?;
                return Err(error)
                    .with_context(|| format!("failed to run {} shell", self.limactl.display()));
            }
        };
        let mut stderr = child
            .stderr
            .take()
            .context("managed VM stderr is unavailable")?;
        let forward = thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut destination = std::io::stderr();
            let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
            let mut observed = Vec::new();
            loop {
                let count = stderr.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                destination.write_all(&buffer[..count])?;
                destination.flush()?;
                observed.extend_from_slice(&buffer[..count]);
                if observed.len() > ERROR_TAIL_LIMIT {
                    observed.drain(..observed.len() - ERROR_TAIL_LIMIT);
                }
            }
            Ok(observed)
        });
        let output = child
            .wait_with_output()
            .context("failed to wait for managed VM command");
        let signal_result = forwarding.close();
        let streamed_stderr = forward
            .join()
            .map_err(|_| anyhow::anyhow!("managed VM stderr forwarding thread panicked"))?
            .context("failed to forward managed VM stderr")?;
        let mut output = output?;
        signal_result?;
        output.stderr = streamed_stderr;
        if !output.status.success()
            && let Some(error) = crate::error::parse_remote(&output.stderr, true)
        {
            return Err(error.into());
        }
        ensure_success(&output, "managed VM command")?;
        Ok(ForwardedOutput {
            stdout: output.stdout,
            stderr: Vec::new(),
        })
    }

    fn systemd_state_arguments(
        arguments: Vec<OsString>,
        cleanup: &[&String],
        mounts: &[ForwardedMount],
        unit: &str,
    ) -> Vec<OsString> {
        let mut command = vec![
            OsString::from("/usr/bin/sudo"),
            OsString::from("/usr/bin/systemd-run"),
            OsString::from("--quiet"),
            OsString::from("--wait"),
            OsString::from("--pipe"),
            OsString::from("--collect"),
            OsString::from("--service-type=exec"),
            OsString::from("--unit"),
            OsString::from(unit),
        ];
        if !cleanup.is_empty() {
            command.extend([
                OsString::from("--property"),
                OsString::from(format!(
                    "ExecStopPost=/usr/bin/rm -rf -- {}",
                    cleanup
                        .iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                )),
            ]);
        }
        for mount in mounts {
            command.extend([
                OsString::from("--property"),
                OsString::from(format!(
                    "BindReadOnlyPaths={}:{}",
                    mount.source, mount.destination
                )),
            ]);
        }
        command.extend([
            OsString::from("--"),
            OsString::from(guest_binary_path()),
            OsString::from("--state"),
            OsString::from(STATE_PATH),
        ]);
        command.extend(arguments);
        command
    }

    fn stage_input(&self, source: &Path, label: &str) -> Result<String> {
        ensure!(
            source.is_file(),
            "managed VM input is not a regular file: {}",
            source.display()
        );
        let remote = format!("/var/tmp/runlab-{label}-{}", Uuid::new_v4());
        let identity = file_identity(source)?;
        self.copy_checked(source, &identity, &remote)?;
        Ok(remote)
    }

    fn stage_secret_input(&self, source: &Path, label: &str) -> Result<String> {
        let remote = self.stage_input(source, label)?;
        if let Err(error) = self.guest_success(["chmod", "0600", &remote]) {
            self.cleanup_inputs(&[&remote]);
            return Err(error);
        }
        Ok(remote)
    }

    fn stage_host_mount(&self, source: &Path) -> Result<(ForwardedMount, Vec<String>)> {
        let destination = source
            .to_str()
            .context("host mount source path is not valid UTF-8")?
            .to_owned();
        if source.is_file() {
            let remote = self.stage_input(source, "mount")?;
            return Ok((
                ForwardedMount {
                    source: remote.clone(),
                    destination,
                },
                vec![remote],
            ));
        }
        ensure!(
            source.is_dir(),
            "host mount source is not a regular file or directory: {}",
            source.display()
        );
        let archive =
            tempfile::NamedTempFile::new().context("failed to stage host mount directory")?;
        let mut builder = tar::Builder::new(archive.reopen()?);
        builder
            .append_dir_all(".", source)
            .with_context(|| format!("failed to archive host mount {}", source.display()))?;
        builder.finish()?;
        drop(builder);
        let remote_archive = self.stage_input(archive.path(), "mount-archive")?;
        let remote_root = format!("/var/tmp/runlab-mount-{}", Uuid::new_v4());
        let setup = (|| {
            self.guest_success([
                "/usr/bin/sudo",
                "/usr/bin/install",
                "-d",
                "-m",
                "0700",
                &remote_root,
            ])?;
            self.guest_success([
                "/usr/bin/sudo",
                "/usr/bin/tar",
                "-xf",
                &remote_archive,
                "-C",
                &remote_root,
            ])
        })();
        if let Err(error) = setup {
            self.cleanup_staged(&[&remote_archive, &remote_root]);
            return Err(error);
        }
        Ok((
            ForwardedMount {
                source: remote_root.clone(),
                destination,
            },
            vec![remote_archive, remote_root],
        ))
    }

    fn stage_host_mounts(
        &self,
        runtime_config: Option<&Path>,
        staged: &mut Vec<String>,
    ) -> Result<Vec<ForwardedMount>> {
        let sources = runtime_config
            .map(host_mount_sources)
            .transpose()
            .map_err(|error| {
                crate::error::classify(
                    error,
                    crate::error::ErrorFacts::before_run(
                        crate::error::ErrorCategory::InvalidInput,
                        "mount_staging",
                    ),
                )
            })?;
        let mut mounts = Vec::new();
        for source in sources.into_iter().flatten() {
            let (mount, cleanup) = self.stage_host_mount(&source)?;
            staged.extend(cleanup);
            mounts.push(mount);
        }
        Ok(mounts)
    }

    fn cleanup_inputs(&self, paths: &[&String]) {
        for path in paths {
            let _ = self.guest_success(["/usr/bin/rm", "-f", "--", path]);
        }
    }

    fn cleanup_staged(&self, paths: &[&String]) {
        for path in paths {
            let _ = self.guest_success(["/usr/bin/sudo", "/usr/bin/rm", "-rf", "--", path]);
        }
    }

    fn copy_output_archive(
        &self,
        remote: &str,
        identity: &super::host::FileIdentity,
        parent: &Path,
    ) -> Result<tempfile::NamedTempFile> {
        let temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("cannot stage output in {}", parent.display()))?;
        let remote = format!("{INSTANCE}:{remote}");
        let output = Command::new(&self.limactl)
            .args([
                OsStr::new("copy"),
                OsStr::new("--backend=scp"),
                OsStr::new(&remote),
            ])
            .arg(temporary.path())
            .output()
            .with_context(|| format!("failed to run {} copy", self.limactl.display()))?;
        ensure_success(&output, "limactl copy")?;
        ensure_remote_identity(&file_identity(temporary.path())?, identity)?;
        Ok(temporary)
    }
}

fn publish_filesystem_archive(archive: &Path, result: &Value, destination: &Path) -> Result<()> {
    validate_filesystem_archive(archive)?;
    let parent = destination
        .parent()
        .expect("normalized output has a parent");
    let staging = tempfile::tempdir_in(parent)
        .with_context(|| format!("cannot stage output in {}", parent.display()))?;
    tar::Archive::new(fs::File::open(archive)?)
        .unpack(staging.path())
        .context("managed VM filesystem archive is invalid")?;
    let payload = staging.path().join("payload");
    ensure_archive_payload_matches(&payload, result)?;
    let entries = fs::read_dir(staging.path())?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() == 1 && entries[0].file_name() == OsStr::new("payload"),
        "managed VM filesystem archive must contain exactly one payload"
    );
    renameat_with(CWD, &payload, CWD, destination, RenameFlags::NOREPLACE).with_context(|| {
        format!(
            "cannot publish output {} without overwriting",
            destination.display()
        )
    })?;
    Ok(())
}

fn validate_filesystem_archive(path: &Path) -> Result<()> {
    let mut has_payload = false;
    let mut archive = tar::Archive::new(fs::File::open(path)?);
    for entry in archive
        .entries()
        .context("failed to read managed VM filesystem archive")?
    {
        let entry = entry.context("failed to read managed VM filesystem archive entry")?;
        let path = entry.path()?;
        let mut components = path.components();
        ensure!(
            components.next() == Some(Component::Normal(OsStr::new("payload"))),
            "managed VM filesystem archive contains a path outside payload: {}",
            path.display()
        );
        for component in components {
            ensure!(
                matches!(component, Component::Normal(_) | Component::CurDir),
                "managed VM filesystem archive contains an invalid path"
            );
        }
        has_payload = true;
    }
    ensure!(has_payload, "managed VM filesystem archive has no payload");
    Ok(())
}

fn ensure_archive_payload_matches(payload: &Path, result: &Value) -> Result<()> {
    let metadata = fs::symlink_metadata(payload)
        .context("managed VM filesystem archive has no materialized payload")?;
    match result.get("kind").and_then(Value::as_str) {
        Some("file") => {
            ensure!(
                metadata.is_file(),
                "managed VM filesystem payload kind does not match"
            );
            let identity = file_identity(payload)?;
            ensure!(
                result.get("digest").and_then(Value::as_str) == Some(&identity.digest)
                    && result.get("size").and_then(Value::as_u64) == Some(identity.size),
                "managed VM filesystem result does not match the payload"
            );
        }
        Some("directory") => ensure!(
            metadata.is_dir(),
            "managed VM filesystem payload kind does not match"
        ),
        Some("symlink") => {
            ensure!(
                metadata.file_type().is_symlink(),
                "managed VM filesystem payload kind does not match"
            );
            let target = fs::read_link(payload)?;
            ensure!(
                result.get("target").and_then(Value::as_str) == target.to_str(),
                "managed VM filesystem result does not match the payload"
            );
        }
        _ => bail!("guest filesystem get returned an unknown node kind"),
    }
    Ok(())
}

fn new_output_path(output: &Path) -> Result<PathBuf> {
    let name = output
        .file_name()
        .context("--output must identify a new local path")?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .with_context(|| format!("failed to resolve output parent for {}", output.display()))?;
    let output = parent.join(name);
    match fs::symlink_metadata(&output) {
        Ok(_) => bail!("output path already exists: {}", output.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(output),
        Err(error) => Err(error.into()),
    }
}

fn host_mount_sources(runtime_config: &Path) -> Result<Vec<PathBuf>> {
    let bytes = fs::read(runtime_config).with_context(|| {
        format!(
            "failed to read Runtime Configuration {}",
            runtime_config.display()
        )
    })?;
    let value: Value =
        serde_json::from_slice(&bytes).context("Runtime Configuration is not valid JSON")?;
    let Some(mounts) = value.get("mounts").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut sources = BTreeSet::new();
    for mount in mounts {
        if mount.get("type").and_then(Value::as_str) != Some("bind")
            || is_managed_resolver_mount(mount)
        {
            continue;
        }
        let source = mount
            .get("source")
            .and_then(Value::as_str)
            .context("OCI bind mount source must be a string")?;
        let path = PathBuf::from(source);
        ensure!(
            path.is_absolute(),
            "macOS host bind mount source must be absolute: {source}"
        );
        ensure!(
            !source.contains([':', '\n', '\r']),
            "macOS host bind mount source contains an unsupported character: {source}"
        );
        let read_only = mount
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|options| options.iter().any(|value| value.as_str() == Some("ro")));
        ensure!(
            read_only,
            "macOS Managed VM supports only read-only host bind mounts: {source}"
        );
        ensure!(
            path.is_file() || path.is_dir(),
            "macOS host bind mount source does not exist or is unsupported: {source}"
        );
        sources.insert(path);
    }
    Ok(sources.into_iter().collect())
}

fn is_managed_resolver_mount(mount: &Value) -> bool {
    mount.get("destination").and_then(Value::as_str) == Some("/etc/resolv.conf")
        && matches!(
            mount.get("source").and_then(Value::as_str),
            Some(
                "/run/systemd/resolve/stub-resolv.conf"
                    | "/run/systemd/resolve/resolv.conf"
                    | "/etc/resolv.conf"
            )
        )
}

struct GuestSignalForwarding {
    finished: Arc<AtomicBool>,
    signal_handle: signal_hook::iterator::Handle,
    thread: thread::JoinHandle<Result<()>>,
}

impl GuestSignalForwarding {
    fn install(limactl: &Path, unit: &str) -> Result<Self> {
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])?;
        let signal_handle = signals.handle();
        let finished = Arc::new(AtomicBool::new(false));
        let thread_finished = Arc::clone(&finished);
        let limactl = limactl.to_owned();
        let unit = unit.to_owned();
        let thread = thread::spawn(move || -> Result<()> {
            let Some(signal) = signals.forever().next() else {
                return Ok(());
            };
            let signal = match signal {
                signal_hook::consts::SIGINT => "SIGINT",
                signal_hook::consts::SIGTERM => "SIGTERM",
                _ => unreachable!("only installed signals can be observed"),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut last_error = None;
            while !thread_finished.load(Ordering::Acquire) {
                let output = Command::new(&limactl)
                    .args([
                        "shell",
                        "--tty=false",
                        INSTANCE,
                        "--",
                        "/usr/bin/sudo",
                        "/usr/bin/systemctl",
                        "kill",
                        &format!("--signal={signal}"),
                        "--kill-whom=main",
                        &unit,
                    ])
                    .process_group(0)
                    .output()
                    .with_context(|| format!("failed to run {} shell", limactl.display()))?;
                if output.status.success() {
                    return Ok(());
                }
                last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_owned());
                if Instant::now() >= deadline {
                    break;
                }
                thread::park_timeout(Duration::from_millis(50));
            }
            if thread_finished.load(Ordering::Acquire) {
                return Ok(());
            }
            bail!(
                "failed to deliver {signal} to managed VM execution {unit}: {}",
                last_error.unwrap_or_else(|| "managed VM control command failed".to_owned())
            )
        });
        Ok(Self {
            finished,
            signal_handle,
            thread,
        })
    }

    fn close(self) -> Result<()> {
        self.finished.store(true, Ordering::Release);
        self.signal_handle.close();
        self.thread.thread().unpark();
        self.thread
            .join()
            .map_err(|_| anyhow::anyhow!("managed VM signal forwarding thread panicked"))?
    }
}

fn append_metadata_arguments(arguments: &mut Vec<OsString>, metadata: &Metadata) {
    if let Some(description) = metadata.description() {
        arguments.extend(["--description".into(), description.into()]);
    }
    for (key, value) in metadata.labels() {
        arguments.extend(["--label".into(), format!("{key}={value}").into()]);
    }
}

fn archive_if_directory(source: &Path) -> Result<Option<tempfile::NamedTempFile>> {
    if source.is_file() {
        return Ok(None);
    }
    ensure!(
        source.is_dir(),
        "OCI Image source does not exist: {}",
        source.display()
    );
    let archive = tempfile::NamedTempFile::new().context("failed to stage OCI Image Layout")?;
    let file = archive.reopen()?;
    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", source)
        .with_context(|| format!("failed to archive OCI Image Layout {}", source.display()))?;
    builder.finish()?;
    drop(builder);
    Ok(Some(archive))
}
