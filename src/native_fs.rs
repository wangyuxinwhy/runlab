use std::ffi::{CString, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};

const PROFILE_OPTIONS: [&str; 4] = [
    "index=on",
    "metacopy=off",
    "nfs_export=off",
    "redirect_dir=nofollow",
];

#[derive(Debug)]
pub(crate) struct OverlayRootfs {
    target: PathBuf,
    workspace: Option<PathBuf>,
    mounted: bool,
}

impl OverlayRootfs {
    pub(crate) fn preflight_at(parent: &Path) -> Result<()> {
        let workspace = tempfile::Builder::new()
            .prefix("runlab-overlay-preflight-")
            .tempdir_in(parent)
            .context("failed to create OverlayFS preflight workspace")?;
        let lower = workspace.path().join("lower");
        let target = workspace.path().join("merged");
        let overlay = workspace.path().join("overlay");
        fs::create_dir(&lower).context("failed to create OverlayFS preflight lowerdir")?;
        fs::create_dir(&target).context("failed to create OverlayFS preflight mountpoint")?;
        let mut mounted = Self::mount_at(&lower, &target, &overlay)?;
        mounted.unmount()
    }

    pub(crate) fn mount_at(lower: &Path, target: &Path, workspace: &Path) -> Result<Self> {
        validate_option_path(lower, "lowerdir")?;
        validate_option_path(target, "mount target")?;
        fs::create_dir(workspace).with_context(|| {
            format!(
                "failed to create OverlayFS workspace {}",
                workspace.display()
            )
        })?;
        crate::integrity::ensure_private_directory(workspace)?;
        Self::mount_in(lower, target, workspace.to_path_buf())
    }

    pub(crate) fn reconcile(target: &Path) -> Result<bool> {
        let metadata = match fs::symlink_metadata(target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to inspect OverlayFS mountpoint"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("OverlayFS recovery mountpoint is not a real directory");
        }
        let mounts = mounts_at_or_below(target)?;
        if mounts.is_empty() {
            return Ok(false);
        }
        let target = target
            .canonicalize()
            .context("failed to canonicalize OverlayFS recovery mountpoint")?;
        if mounts != [target.clone()] {
            bail!("runtime mounts remain below the OverlayFS recovery mountpoint");
        }
        unmount(&target, UnmountFlags::empty())
            .context("failed to unmount OverlayFS during reconciliation")?;
        if !mounts_at_or_below(&target)?.is_empty() {
            bail!("OverlayFS mount remained visible after reconciliation");
        }
        Ok(true)
    }

    pub(crate) fn recovery_capture_ready(target: &Path) -> Result<bool> {
        let metadata = match fs::symlink_metadata(target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to inspect OverlayFS mountpoint"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("OverlayFS recovery mountpoint is not a real directory");
        }
        let target = target
            .canonicalize()
            .context("failed to canonicalize OverlayFS recovery mountpoint")?;
        let mounts = mounts_at_or_below(&target)?;
        if mounts.is_empty() {
            return Ok(false);
        }
        if mounts != [target] {
            bail!("runtime mounts remain below the OverlayFS recovery mountpoint");
        }
        verify_profile_at(&mounts[0])?;
        Ok(true)
    }

    fn mount_in(lower: &Path, target: &Path, workspace: PathBuf) -> Result<Self> {
        let upper = workspace.join("upper");
        let work = workspace.join("work");
        fs::create_dir(&upper).context("failed to create OverlayFS upperdir")?;
        fs::create_dir(&work).context("failed to create OverlayFS workdir")?;
        let options = CString::new(format!(
            "lowerdir={},upperdir={},workdir={},{}",
            lower.display(),
            upper.display(),
            work.display(),
            PROFILE_OPTIONS.join(",")
        ))
        .context("OverlayFS mount options contain NUL")?;
        mount(
            "overlay",
            target,
            "overlay",
            MountFlags::empty(),
            Some(options.as_c_str()),
        )
        .with_context(|| format!("failed to mount OverlayFS at {}", target.display()))?;
        let mut mounted = Self {
            target: target.to_path_buf(),
            workspace: Some(workspace),
            mounted: true,
        };
        if let Err(error) = mounted.verify_profile() {
            let cleanup = mounted.unmount();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{error:#}; OverlayFS cleanup also failed: {cleanup:#}"
                )),
            };
        }
        Ok(mounted)
    }

    pub(crate) fn verify_runtime_mounts_removed(&self) -> Result<()> {
        let mounts = mounts_at_or_below(&self.target)?;
        if mounts == [self.target.clone()] {
            return Ok(());
        }
        bail!(
            "OCI runtime retained mounts below bundle rootfs: {}",
            mounts
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    pub(crate) fn unmount(&mut self) -> Result<()> {
        if !self.mounted {
            return Ok(());
        }
        unmount(&self.target, UnmountFlags::empty())
            .with_context(|| format!("failed to unmount OverlayFS at {}", self.target.display()))?;
        self.mounted = false;
        if !mounts_at_or_below(&self.target)?.is_empty() {
            bail!(
                "OverlayFS mount remained visible after unmount: {}",
                self.target.display()
            );
        }
        Ok(())
    }

    pub(crate) fn preserve(mut self) -> PathBuf {
        self.mounted = false;
        self.workspace
            .take()
            .expect("OverlayFS workspace is present")
    }

    fn verify_profile(&self) -> Result<()> {
        let target = self
            .target
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", self.target.display()))?;
        verify_profile_at(&target)
    }
}

fn verify_profile_at(target: &Path) -> Result<()> {
    let mount = read_mountinfo()?
        .into_iter()
        .find(|mount| mount.mountpoint == target)
        .with_context(|| {
            format!(
                "OverlayFS mount is absent from /proc/self/mountinfo: {}",
                target.display()
            )
        })?;
    if mount.filesystem != "overlay" {
        bail!(
            "bundle rootfs is mounted as {}, expected overlay",
            mount.filesystem
        );
    }
    require_mount_option(&mount.super_options, "index=on")?;
    require_mount_option(&mount.super_options, "redirect_dir=nofollow")?;
    verify_disabled_option(&mount.super_options, "metacopy")?;
    verify_disabled_option(&mount.super_options, "nfs_export")?;
    Ok(())
}

fn require_mount_option(options: &[String], expected: &str) -> Result<()> {
    if options.iter().any(|actual| actual == expected) {
        Ok(())
    } else {
        bail!("effective OverlayFS profile lacks {expected}")
    }
}

fn verify_disabled_option(options: &[String], name: &str) -> Result<()> {
    if options
        .iter()
        .any(|actual| actual == &format!("{name}=off"))
    {
        return Ok(());
    }
    if options
        .iter()
        .any(|actual| actual.starts_with(&format!("{name}=")))
    {
        bail!("effective OverlayFS profile did not disable {name}");
    }
    let parameter = fs::read_to_string(format!("/sys/module/overlay/parameters/{name}"))
        .with_context(|| format!("failed to read OverlayFS {name} module default"))?;
    if parameter.trim() != "N" {
        bail!("effective OverlayFS profile cannot prove {name}=off");
    }
    Ok(())
}

impl Drop for OverlayRootfs {
    fn drop(&mut self) {
        if !self.mounted {
            return;
        }
        let _ = unmount(&self.target, UnmountFlags::DETACH);
    }
}

#[derive(Debug)]
struct MountInfo {
    mountpoint: PathBuf,
    filesystem: String,
    super_options: Vec<String>,
}

fn read_mountinfo() -> Result<Vec<MountInfo>> {
    fs::read("/proc/self/mountinfo")
        .context("failed to read /proc/self/mountinfo")?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_mountinfo)
        .collect()
}

fn parse_mountinfo(line: &[u8]) -> Result<MountInfo> {
    let fields = line
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let separator = fields
        .iter()
        .position(|field| *field == b"-")
        .context("mountinfo entry lacks field separator")?;
    if separator < 6 || fields.len() <= separator + 3 {
        bail!("mountinfo entry is incomplete");
    }
    Ok(MountInfo {
        mountpoint: PathBuf::from(unescape_mountinfo(fields[4])?),
        filesystem: std::str::from_utf8(fields[separator + 1])
            .context("mountinfo filesystem type is not ASCII")?
            .to_owned(),
        super_options: std::str::from_utf8(fields[separator + 3])
            .context("mountinfo super options are not ASCII")?
            .split(',')
            .map(ToOwned::to_owned)
            .collect(),
    })
}

fn mounts_at_or_below(root: &Path) -> Result<Vec<PathBuf>> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize mount root {}", root.display()))?;
    let mut mounts = read_mountinfo()?
        .into_iter()
        .filter_map(|mount| {
            (mount.mountpoint == root || mount.mountpoint.starts_with(&root))
                .then_some(mount.mountpoint)
        })
        .collect::<Vec<_>>();
    mounts.sort();
    Ok(mounts)
}

pub(crate) fn ensure_no_mounts_at_or_below(root: &Path) -> Result<()> {
    let mounts = mounts_at_or_below(root)?;
    if !mounts.is_empty() {
        bail!(
            "native recovery directory still contains mounts: {}",
            mounts
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn unescape_mountinfo(value: &[u8]) -> Result<OsString> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'\\' {
            output.push(value[index]);
            index += 1;
            continue;
        }
        let digits = value
            .get(index + 1..index + 4)
            .context("mountinfo path has a truncated escape")?;
        if !digits.iter().all(u8::is_ascii_digit) || digits.iter().any(|digit| *digit > b'7') {
            bail!("mountinfo path has an invalid escape");
        }
        output.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + digits[2] - b'0');
        index += 4;
    }
    Ok(OsString::from_vec(output))
}

fn validate_option_path(path: &Path, name: &str) -> Result<()> {
    let value = path
        .to_str()
        .with_context(|| format!("{name} is not UTF-8: {}", path.display()))?;
    if value
        .bytes()
        .any(|byte| matches!(byte, b',' | b':' | b'\\' | b'\n' | b'\r'))
    {
        bail!("{name} cannot be encoded safely in OverlayFS mount options");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mountinfo_escapes_and_profile() {
        let parsed = parse_mountinfo(
            b"35 24 0:31 / /tmp/runlab\\040root rw,relatime - overlay overlay rw,index=on,metacopy=off,nfs_export=off,redirect_dir=nofollow",
        )
        .expect("mountinfo");
        assert_eq!(parsed.mountpoint, Path::new("/tmp/runlab root"));
        assert_eq!(parsed.filesystem, "overlay");
        assert!(parsed.super_options.iter().any(|value| value == "index=on"));
    }

    #[test]
    fn preserves_non_utf8_mountpoints() {
        use std::os::unix::ffi::OsStrExt as _;

        let parsed = parse_mountinfo(
            b"35 24 0:31 / /tmp/runlab-\xff rw,relatime - overlay overlay rw,index=on,metacopy=off,nfs_export=off,redirect_dir=nofollow",
        )
        .expect("mountinfo");
        assert_eq!(
            parsed.mountpoint.as_os_str().as_bytes(),
            b"/tmp/runlab-\xff"
        );
    }

    #[test]
    fn rejects_mount_option_delimiters() {
        let error = validate_option_path(Path::new("/tmp/with,comma"), "lowerdir")
            .expect_err("delimiter must fail");
        assert!(error.to_string().contains("cannot be encoded safely"));
    }
}
