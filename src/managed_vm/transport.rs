use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use uuid::Uuid;

use super::host::{
    INSTANCE, ManagedVm, STATE_PATH, ensure_remote_identity, ensure_success, file_identity,
    guest_binary_path,
};
use crate::cli::run::SecretFileArg;
use crate::metadata::Metadata;

pub(crate) struct ForwardedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) struct ForwardRunStart<'a> {
    pub(crate) id: &'a str,
    pub(crate) image: &'a str,
    pub(crate) metadata: &'a Metadata,
    pub(crate) runtime_config: Option<&'a Path>,
    pub(crate) stdin: Option<&'a Path>,
    pub(crate) secret_env: &'a [String],
    pub(crate) secret_files: &'a [SecretFileArg],
    pub(crate) execution_timeout_ms: Option<NonZeroU64>,
    pub(crate) network: &'a str,
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

    pub(crate) fn forward_run_config(&self, image: &str) -> Result<ForwardedOutput> {
        self.state_command(["run", "config", "generate", "--image", image])
            .map(Into::into)
    }

    pub(crate) fn forward_run_start(
        &self,
        request: &ForwardRunStart<'_>,
    ) -> Result<ForwardedOutput> {
        self.ensure_ready()?;
        let mut staged = Vec::new();
        let arguments = (|| {
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

            let mut arguments: Vec<OsString> = vec![
                "run".into(),
                "start".into(),
                "--id".into(),
                request.id.into(),
                "--image".into(),
                request.image.into(),
                "--network".into(),
                request.network.into(),
            ];
            append_metadata_arguments(&mut arguments, request.metadata);
            if let Some(path) = &runtime_config {
                arguments.extend(["--runtime-config".into(), path.into()]);
            }
            if let Some(path) = &stdin {
                arguments.extend(["--stdin".into(), path.into()]);
            }
            for (name, source) in secret_environment {
                arguments.extend([
                    "--secret-env-file".into(),
                    format!("{name}={source}").into(),
                ]);
            }
            for (destination, source) in secret_file_sources {
                arguments.extend([
                    "--secret-file".into(),
                    format!("{source}={destination}").into(),
                ]);
            }
            if let Some(timeout) = request.execution_timeout_ms {
                arguments.extend([
                    "--execution-timeout-ms".into(),
                    timeout.get().to_string().into(),
                ]);
            }
            Ok(arguments)
        })();
        let arguments = match arguments {
            Ok(arguments) => arguments,
            Err(error) => {
                self.cleanup_inputs(&staged.iter().collect::<Vec<_>>());
                return Err(error);
            }
        };
        let references = staged.iter().collect::<Vec<_>>();
        let output = self.systemd_state_command_streaming(arguments, &references);
        self.cleanup_inputs(&references);
        output
    }

    pub(crate) fn forward_run_get(&self, id: &str) -> Result<ForwardedOutput> {
        self.state_command(["run", "get", id]).map(Into::into)
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

    pub(crate) fn forward_filesystem_get(
        &self,
        run: Option<&str>,
        image: Option<&str>,
        program: Option<&str>,
        path: &str,
        destination: &Path,
    ) -> Result<ForwardedOutput> {
        ensure!(
            !destination.exists(),
            "output path already exists: {}",
            destination.display()
        );
        let remote = format!("/var/tmp/runlab-output-{}", Uuid::new_v4());
        let mut arguments: Vec<OsString> = vec!["filesystem".into(), "get".into()];
        match (run, image) {
            (Some(run), None) => arguments.extend(["--run".into(), run.into()]),
            (None, Some(image)) => arguments.extend(["--image".into(), image.into()]),
            _ => bail!("exactly one of --run or --image is required"),
        }
        if let Some(program) = program {
            arguments.extend(["--program".into(), program.into()]);
        }
        arguments.extend([path.into(), "--output".into(), remote.clone().into()]);
        let result = (|| {
            let guest_output = self.state_command(arguments)?;
            ensure!(
                self.guest_test(["-f", &remote]) && !self.guest_test(["-L", &remote]),
                "managed VM filesystem get currently supports regular files only"
            );
            let remote_identity = self.remote_file_identity(&remote)?;
            let mut result: Value = serde_json::from_slice(&guest_output.stdout)
                .context("guest filesystem get returned invalid JSON")?;
            ensure!(
                result.get("digest").and_then(Value::as_str) == Some(&remote_identity.digest)
                    && result.get("size").and_then(Value::as_u64) == Some(remote_identity.size),
                "guest filesystem result does not match the output file"
            );
            self.copy_output(&remote, &remote_identity, destination)?;
            result["output"] = Value::String(destination.display().to_string());
            let mut stdout = serde_json::to_vec(&result)?;
            stdout.push(b'\n');
            Ok(ForwardedOutput {
                stdout,
                stderr: guest_output.stderr,
            })
        })();
        let _ = self.guest_success(["/usr/bin/sudo", "/usr/bin/rm", "-rf", "--", &remote]);
        result
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
    ) -> Result<ForwardedOutput> {
        self.ensure_ready()?;
        let mut command = Command::new(&self.limactl);
        command.args(["shell", "--tty=false", INSTANCE, "--"]);
        command.args(Self::systemd_state_arguments(arguments, cleanup));
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run {} shell", self.limactl.display()))?;
        let mut stderr = child
            .stderr
            .take()
            .context("managed VM stderr is unavailable")?;
        let forward = thread::spawn(move || -> std::io::Result<()> {
            let mut destination = std::io::stderr();
            let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
            loop {
                let count = stderr.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                destination.write_all(&buffer[..count])?;
                destination.flush()?;
            }
            Ok(())
        });
        let output = child
            .wait_with_output()
            .context("failed to wait for managed VM command")?;
        forward
            .join()
            .map_err(|_| anyhow::anyhow!("managed VM stderr forwarding thread panicked"))?
            .context("failed to forward managed VM stderr")?;
        ensure_success(&output, "managed VM command")?;
        Ok(ForwardedOutput {
            stdout: output.stdout,
            stderr: Vec::new(),
        })
    }

    fn systemd_state_arguments(arguments: Vec<OsString>, cleanup: &[&String]) -> Vec<OsString> {
        let mut command = vec![
            OsString::from("/usr/bin/sudo"),
            OsString::from("/usr/bin/systemd-run"),
            OsString::from("--quiet"),
            OsString::from("--wait"),
            OsString::from("--pipe"),
            OsString::from("--collect"),
            OsString::from("--service-type=exec"),
            OsString::from("--unit"),
            OsString::from(format!("runlab-run-{}", Uuid::new_v4())),
        ];
        if !cleanup.is_empty() {
            command.extend([
                OsString::from("--property"),
                OsString::from(format!(
                    "ExecStopPost=/usr/bin/rm -f -- {}",
                    cleanup
                        .iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
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

    fn cleanup_inputs(&self, paths: &[&String]) {
        for path in paths {
            let _ = self.guest_success(["/usr/bin/rm", "-f", "--", path]);
        }
    }

    fn copy_output(
        &self,
        remote: &str,
        identity: &super::host::FileIdentity,
        destination: &Path,
    ) -> Result<()> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        ensure!(
            parent.is_dir(),
            "output parent does not exist: {}",
            parent.display()
        );
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
        temporary.persist_noclobber(destination).map_err(|error| {
            anyhow::anyhow!(
                "cannot publish output {} without overwriting: {}",
                destination.display(),
                error.error
            )
        })?;
        Ok(())
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
