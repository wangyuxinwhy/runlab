use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::integrity::canonical_json;

const CGROUP_MOUNT: &str = "/sys/fs/cgroup";
const CHECKPOINT_SCHEMA_VERSION: u32 = 2;
const MAX_CGROUP_FILE_BYTES: u64 = 64 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCgroupCheckpoint {
    schema_version: u32,
    runtime_id: String,
    relative_path: String,
    device: u64,
    inode: u64,
    baseline_oom_kill: Option<u64>,
    terminal_oom_kill: Option<u64>,
    resources_absent: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedNativeCgroup {
    directory: File,
    path: PathBuf,
    checkpoint_path: PathBuf,
    checkpoint: NativeCgroupCheckpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeCgroupTerminal {
    pub oom_killed: bool,
    pub oom_kill_delta: u64,
}

impl PreparedNativeCgroup {
    pub(crate) fn checkpoint_path(&self) -> &Path {
        &self.checkpoint_path
    }

    pub(crate) fn probe() -> Result<()> {
        let checkpoint_directory = tempfile::Builder::new()
            .prefix("runlab-cgroup-probe-")
            .tempdir()
            .context("failed to create cgroup probe checkpoint directory")?;
        let runtime_id = format!("runlab-cgroup-probe-{}", Uuid::now_v7());
        let checkpoint_path = checkpoint_directory.path().join("cgroup.json");
        let prepared = match Self::prepare(&runtime_id, &checkpoint_path) {
            Ok(prepared) => prepared,
            Err(error) => {
                let retained = checkpoint_directory.keep();
                return Err(error).with_context(|| {
                    format!(
                        "native cgroup probe evidence was retained at {}",
                        retained.display()
                    )
                });
            }
        };
        match prepared.cleanup_owned_empty() {
            Ok(()) => Ok(()),
            Err(error) => {
                let retained = checkpoint_directory.keep();
                Err(error).with_context(|| {
                    format!(
                        "native cgroup probe cleanup evidence was retained at {}",
                        retained.display()
                    )
                })
            }
        }
    }

    pub(crate) fn prepare(runtime_id: &str, checkpoint_path: &Path) -> Result<Self> {
        validate_runtime_id(runtime_id)?;
        if fs::symlink_metadata(checkpoint_path).is_ok() {
            bail!(
                "native cgroup checkpoint already exists: {}",
                checkpoint_path.display()
            );
        }
        let relative_path = default_relative_path(runtime_id)?;
        let path = Path::new(CGROUP_MOUNT).join(relative_path.trim_start_matches('/'));
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("native cgroup already exists: {relative_path}")
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create native cgroup {relative_path}"));
            }
        }
        match Self::open_created(path.clone(), checkpoint_path.to_path_buf(), relative_path) {
            Ok(prepared) => Ok(prepared),
            Err(error) => match fs::remove_dir(&path) {
                Ok(()) => Err(error),
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "native cgroup setup failed: {error:#}; rollback of {} also failed: {cleanup}",
                    path.display()
                )),
            },
        }
    }

    fn open_created(
        path: PathBuf,
        checkpoint_path: PathBuf,
        relative_path: String,
    ) -> Result<Self> {
        let directory = File::from(
            open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| format!("failed to open native cgroup {relative_path}"))?,
        );
        let metadata = directory
            .metadata()
            .context("failed to inspect native cgroup identity")?;
        let checkpoint = NativeCgroupCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            runtime_id: runtime_id_from_path(&relative_path)?.to_owned(),
            relative_path,
            device: metadata.dev(),
            inode: metadata.ino(),
            baseline_oom_kill: None,
            terminal_oom_kill: None,
            resources_absent: false,
        };
        write_checkpoint(&checkpoint_path, &checkpoint, false)?;
        let mut prepared = Self {
            directory,
            path,
            checkpoint_path,
            checkpoint,
        };
        let initialized = (|| {
            if !read_cgroup_file(&prepared.directory, "cgroup.procs")?
                .trim()
                .is_empty()
            {
                bail!("new native cgroup is unexpectedly populated");
            }
            if parse_populated(&read_cgroup_file(&prepared.directory, "cgroup.events")?)? {
                bail!("new native cgroup reports populated=1");
            }
            prepared.checkpoint.baseline_oom_kill = Some(parse_oom_kill(&read_cgroup_file(
                &prepared.directory,
                "memory.events",
            )?)?);
            write_checkpoint(&prepared.checkpoint_path, &prepared.checkpoint, true)
        })();
        match initialized {
            Ok(()) => Ok(prepared),
            Err(error) => match prepared.cleanup_owned_empty() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "native cgroup initialization failed: {error:#}; rollback also failed: {cleanup:#}"
                )),
            },
        }
    }

    pub(crate) fn verify_init_pid(&self, pid: u32) -> Result<bool> {
        let contents = match fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to read runc init cgroup identity"),
        };
        let actual = unified_path(&contents)?;
        if actual != self.checkpoint.relative_path {
            bail!(
                "runc init process entered cgroup {actual}, expected {}",
                self.checkpoint.relative_path
            );
        }
        Ok(true)
    }

    pub(crate) fn has_observed_member(&self) -> Result<bool> {
        let contents = read_cgroup_file(&self.directory, "cgroup.procs")?;
        let mut observed = false;
        for line in contents.lines() {
            line.parse::<u32>()
                .context("native cgroup.procs contains an invalid process identity")?;
            observed = true;
        }
        Ok(observed)
    }

    pub(crate) fn observe_terminal(
        &mut self,
        quiesce_timeout: Duration,
    ) -> Result<NativeCgroupTerminal> {
        self.verify_identity()?;
        if parse_populated(&read_cgroup_file(&self.directory, "cgroup.events")?)? {
            write_cgroup_file(&self.directory, "cgroup.kill", b"1")?;
            let deadline = Instant::now()
                .checked_add(quiesce_timeout)
                .context("native cgroup quiesce deadline overflow")?;
            while parse_populated(&read_cgroup_file(&self.directory, "cgroup.events")?)? {
                if Instant::now() >= deadline {
                    bail!("native cgroup remains populated after cgroup.kill");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        let terminal = parse_oom_kill(&read_cgroup_file(&self.directory, "memory.events")?)?;
        let baseline = self
            .checkpoint
            .baseline_oom_kill
            .context("native cgroup baseline oom_kill counter is unavailable")?;
        let delta = terminal
            .checked_sub(baseline)
            .context("native cgroup oom_kill counter moved backward")?;
        self.checkpoint.terminal_oom_kill = Some(terminal);
        write_checkpoint(&self.checkpoint_path, &self.checkpoint, true)?;
        Ok(NativeCgroupTerminal {
            oom_killed: delta > 0,
            oom_kill_delta: delta,
        })
    }

    pub(crate) fn finish_after_runc_delete(mut self) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.mark_resources_absent()
            }
            Err(error) => Err(error).context("failed to inspect native cgroup after runc delete"),
            Ok(_) => self.cleanup_owned_empty(),
        }
    }

    pub(crate) fn cleanup_owned_empty(mut self) -> Result<()> {
        self.verify_identity()?;
        if parse_populated(&read_cgroup_file(&self.directory, "cgroup.events")?)?
            || !read_cgroup_file(&self.directory, "cgroup.procs")?
                .trim()
                .is_empty()
        {
            bail!("native cgroup cannot be removed while it is populated");
        }
        fs::remove_dir(&self.path).context("failed to remove native cgroup")?;
        self.mark_resources_absent()
    }

    fn mark_resources_absent(&mut self) -> Result<()> {
        self.checkpoint.resources_absent = true;
        write_checkpoint(&self.checkpoint_path, &self.checkpoint, true)
    }

    fn verify_identity(&self) -> Result<()> {
        let open = self
            .directory
            .metadata()
            .context("failed to inspect open native cgroup")?;
        let current = fs::symlink_metadata(&self.path)
            .context("failed to inspect native cgroup path identity")?;
        if current.file_type().is_symlink()
            || !current.is_dir()
            || open.dev() != self.checkpoint.device
            || open.ino() != self.checkpoint.inode
            || current.dev() != self.checkpoint.device
            || current.ino() != self.checkpoint.inode
        {
            bail!("native cgroup identity changed during execution");
        }
        Ok(())
    }
}

pub(crate) fn reconcile_checkpoint(
    checkpoint_path: &Path,
    expected_runtime_id: &str,
) -> Result<bool> {
    validate_runtime_id(expected_runtime_id)?;
    let mut checkpoint = match read_checkpoint(checkpoint_path) {
        Ok(checkpoint) => checkpoint,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if checkpoint.runtime_id != expected_runtime_id {
        bail!("native cgroup checkpoint runtime identity mismatch");
    }
    let path = Path::new(CGROUP_MOUNT).join(checkpoint.relative_path.trim_start_matches('/'));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(_) if checkpoint.resources_absent => {
            bail!("native recovery cgroup reappeared after its cleanup tombstone")
        }
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && checkpoint.resources_absent =>
        {
            return Ok(true);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            checkpoint.resources_absent = true;
            write_checkpoint(checkpoint_path, &checkpoint, true)?;
            return Ok(true);
        }
        Err(error) => return Err(error).context("failed to inspect recovery cgroup"),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.dev() != checkpoint.device
        || metadata.ino() != checkpoint.inode
    {
        bail!("native recovery cgroup identity changed");
    }
    let directory = File::from(open(
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let opened = directory
        .metadata()
        .context("failed to inspect opened recovery cgroup")?;
    if opened.dev() != checkpoint.device || opened.ino() != checkpoint.inode {
        bail!("native recovery cgroup identity changed while opening it");
    }
    if parse_populated(&read_cgroup_file(&directory, "cgroup.events")?)?
        || !read_cgroup_file(&directory, "cgroup.procs")?
            .trim()
            .is_empty()
    {
        bail!("native recovery cgroup remains populated");
    }
    if checkpoint.terminal_oom_kill.is_none() {
        checkpoint.terminal_oom_kill = Some(parse_oom_kill(&read_cgroup_file(
            &directory,
            "memory.events",
        )?)?);
        write_checkpoint(checkpoint_path, &checkpoint, true)?;
    }
    fs::remove_dir(&path).context("failed to remove recovered native cgroup")?;
    checkpoint.resources_absent = true;
    write_checkpoint(checkpoint_path, &checkpoint, true)?;
    Ok(true)
}

fn default_relative_path(runtime_id: &str) -> Result<String> {
    let own = unified_path(&fs::read_to_string("/proc/self/cgroup")?)?;
    let parent = if own == "/" {
        Path::new("/")
    } else {
        Path::new(&own)
            .parent()
            .context("current unified cgroup has no parent")?
    };
    let relative = parent.join(runtime_id);
    validate_cgroup_path(&relative)?;
    relative
        .to_str()
        .map(ToOwned::to_owned)
        .context("native cgroup path is not UTF-8")
}

fn unified_path(contents: &str) -> Result<String> {
    let mut matches = contents.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = matches
        .next()
        .context("unified cgroup path is unavailable")?;
    if matches.next().is_some() {
        bail!("multiple unified cgroup paths were reported");
    }
    validate_cgroup_path(Path::new(path))?;
    Ok(path.to_owned())
}

fn validate_cgroup_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        bail!("unified cgroup path is not normalized and absolute");
    }
    Ok(())
}

fn validate_runtime_id(runtime_id: &str) -> Result<()> {
    if runtime_id.is_empty()
        || runtime_id.len() > 128
        || !runtime_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("native runtime identity is not a safe cgroup name");
    }
    Ok(())
}

fn read_cgroup_file(directory: &File, name: &str) -> Result<String> {
    let mut file = File::from(openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CGROUP_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CGROUP_FILE_BYTES {
        bail!("native cgroup file {name} exceeds its read limit");
    }
    String::from_utf8(bytes).with_context(|| format!("native cgroup file {name} is not UTF-8"))
}

fn write_cgroup_file(directory: &File, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = File::from(openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    file.write_all(bytes)
        .with_context(|| format!("failed to write native cgroup file {name}"))
}

fn parse_oom_kill(contents: &str) -> Result<u64> {
    parse_counter(contents, "oom_kill", "memory.events")
}

fn parse_populated(contents: &str) -> Result<bool> {
    match parse_counter(contents, "populated", "cgroup.events")? {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!("cgroup.events populated is not 0 or 1"),
    }
}

fn parse_counter(contents: &str, key: &str, source: &str) -> Result<u64> {
    let mut found = None;
    for line in contents.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(candidate) = fields.next() else {
            continue;
        };
        let value = fields
            .next()
            .with_context(|| format!("{source} contains an incomplete counter"))?;
        if fields.next().is_some() {
            bail!("{source} contains an invalid counter line");
        }
        if candidate == key {
            if found.is_some() {
                bail!("{source} contains duplicate {key}");
            }
            found = Some(
                value
                    .parse::<u64>()
                    .with_context(|| format!("{source} {key} is not an unsigned integer"))?,
            );
        }
    }
    found.with_context(|| format!("{source} is missing {key}"))
}

fn write_checkpoint(path: &Path, checkpoint: &NativeCgroupCheckpoint, replace: bool) -> Result<()> {
    validate_checkpoint(checkpoint)?;
    let bytes = canonical_json(checkpoint)?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        bail!("native cgroup checkpoint exceeds its size limit");
    }
    let parent = path
        .parent()
        .context("native cgroup checkpoint has no parent")?;
    if !replace && fs::symlink_metadata(path).is_ok() {
        bail!("native cgroup checkpoint already exists");
    }
    let mut temporary = NamedTempFile::new_in(parent)
        .context("failed to create native cgroup checkpoint staging file")?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(&bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("failed to publish native cgroup checkpoint")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_checkpoint(path: &Path) -> Result<NativeCgroupCheckpoint> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > MAX_CHECKPOINT_BYTES
    {
        bail!("native cgroup checkpoint is not a private bounded regular file");
    }
    let checkpoint: NativeCgroupCheckpoint = serde_json::from_slice(&fs::read(path)?)
        .context("native cgroup checkpoint is invalid JSON")?;
    validate_checkpoint(&checkpoint)?;
    Ok(checkpoint)
}

fn validate_checkpoint(checkpoint: &NativeCgroupCheckpoint) -> Result<()> {
    if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION {
        bail!("unsupported native cgroup checkpoint schema version");
    }
    validate_runtime_id(&checkpoint.runtime_id)?;
    validate_cgroup_path(Path::new(&checkpoint.relative_path))?;
    if runtime_id_from_path(&checkpoint.relative_path)? != checkpoint.runtime_id {
        bail!("native cgroup checkpoint path does not match its runtime identity");
    }
    if checkpoint.device == 0 || checkpoint.inode == 0 {
        bail!("native cgroup checkpoint identity is invalid");
    }
    if checkpoint
        .baseline_oom_kill
        .zip(checkpoint.terminal_oom_kill)
        .is_some_and(|(baseline, terminal)| terminal < baseline)
    {
        bail!("native cgroup checkpoint counter moved backward");
    }
    Ok(())
}

fn runtime_id_from_path(path: &str) -> Result<&str> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .context("native cgroup checkpoint path has no UTF-8 runtime identity")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_memory_events_by_key() {
        assert_eq!(
            parse_oom_kill("low 2\noom 4\noom_kill 7\nhigh 9\n").unwrap(),
            7
        );
        assert!(parse_oom_kill("oom 4\n").is_err());
        assert!(parse_oom_kill("oom_kill 1\noom_kill 2\n").is_err());
        assert!(parse_oom_kill("oom_kill -1\n").is_err());
        assert!(parse_oom_kill("oom_kill 1 extra\n").is_err());
    }

    #[test]
    fn parses_populated_as_boolean_counter() {
        assert!(!parse_populated("populated 0\nfrozen 0\n").unwrap());
        assert!(parse_populated("frozen 0\npopulated 1\n").unwrap());
        assert!(parse_populated("populated 2\n").is_err());
    }

    #[test]
    fn rejects_unsafe_runtime_identity() {
        for value in ["", "../other", "a/b", "value with space"] {
            assert!(validate_runtime_id(value).is_err());
        }
    }

    #[test]
    #[ignore = "requires writable cgroup v2"]
    fn recovery_rejects_a_checkpoint_for_another_runtime() {
        let first_directory = tempfile::tempdir().expect("first checkpoint directory");
        let second_directory = tempfile::tempdir().expect("second checkpoint directory");
        let first_id = format!("runlab-{}", Uuid::now_v7());
        let second_id = format!("runlab-{}", Uuid::now_v7());
        let first_checkpoint = first_directory.path().join("cgroup.json");
        let first = PreparedNativeCgroup::prepare(&first_id, &first_checkpoint).expect("first");
        let second =
            PreparedNativeCgroup::prepare(&second_id, &second_directory.path().join("cgroup.json"))
                .expect("second");

        let error = reconcile_checkpoint(&first_checkpoint, &second_id)
            .expect_err("cross-runtime checkpoint must fail closed");

        assert!(error.to_string().contains("runtime identity mismatch"));
        assert!(first.path.exists());
        first.cleanup_owned_empty().expect("first cleanup");
        second.cleanup_owned_empty().expect("second cleanup");
    }
}
