use std::ffi::{OsStr, OsString};
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use uuid::Uuid;

use super::host::{
    INSTANCE, ManagedVm, STATE_PATH, ensure_remote_identity, ensure_success, file_identity,
    guest_binary_path,
};

pub(crate) struct ForwardedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
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
    ) -> Result<ForwardedOutput> {
        let archive = archive_if_directory(source)?;
        let source = archive
            .as_ref()
            .map_or(source, tempfile::NamedTempFile::path);
        let staged = self.stage_input(source, "image")?;
        let output = self.state_command([
            OsStr::new("image"),
            OsStr::new("import"),
            OsStr::new(&staged),
            OsStr::new("--name"),
            OsStr::new(name),
        ]);
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
        id: &str,
        image: &str,
        runtime_config: Option<&Path>,
        stdin: Option<&Path>,
        execution_timeout_ms: Option<NonZeroU64>,
        network: &str,
    ) -> Result<ForwardedOutput> {
        self.ensure_ready()?;
        let runtime_config = runtime_config
            .map(|path| self.stage_input(path, "runtime-config"))
            .transpose()?;
        let stdin = stdin
            .map(|path| self.stage_input(path, "stdin"))
            .transpose();
        let stdin = match stdin {
            Ok(stdin) => stdin,
            Err(error) => {
                self.cleanup_inputs(&runtime_config.iter().collect::<Vec<_>>());
                return Err(error);
            }
        };
        let mut arguments: Vec<OsString> = vec![
            "run".into(),
            "start".into(),
            "--id".into(),
            id.into(),
            "--image".into(),
            image.into(),
            "--network".into(),
            network.into(),
        ];
        if let Some(path) = &runtime_config {
            arguments.extend(["--runtime-config".into(), path.into()]);
        }
        if let Some(path) = &stdin {
            arguments.extend(["--stdin".into(), path.into()]);
        }
        if let Some(timeout) = execution_timeout_ms {
            arguments.extend([
                "--execution-timeout-ms".into(),
                timeout.get().to_string().into(),
            ]);
        }
        let staged = runtime_config.iter().chain(&stdin).collect::<Vec<_>>();
        let output = self.systemd_state_command(arguments, &staged);
        output.map(Into::into)
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

    fn systemd_state_command(
        &self,
        arguments: Vec<OsString>,
        cleanup: &[&String],
    ) -> Result<Output> {
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
        self.guest_output(command)
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
