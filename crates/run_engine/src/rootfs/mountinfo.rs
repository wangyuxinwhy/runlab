use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::display_bytes;

pub(super) fn ensure_no_mounts(root: &OwnedFd, allow_root: bool) -> Result<()> {
    let root_path = std::fs::read_link(proc_fd_path(root))?;
    let root_bytes = root_path.as_os_str().as_bytes();
    let mountinfo = std::fs::read("/proc/self/mountinfo")?;
    ensure_mountinfo_clear(root_bytes, &mountinfo, allow_root)
}

pub(super) fn ensure_path_no_mounts(root: &Path) -> Result<()> {
    let mountinfo = std::fs::read("/proc/self/mountinfo")?;
    ensure_mountinfo_clear(root.as_os_str().as_bytes(), &mountinfo, false)
}

pub(super) fn ensure_mountinfo_clear(
    root_bytes: &[u8],
    mountinfo: &[u8],
    allow_root: bool,
) -> Result<()> {
    if let Some(mountpoint) = mount_below(root_bytes, mountinfo, allow_root)? {
        bail!(
            "rootfs still contains a mount at {}; capture requires all runtime mounts to be removed",
            display_bytes(&mountpoint)
        );
    }
    Ok(())
}

pub(super) fn mount_below(
    root_bytes: &[u8],
    mountinfo: &[u8],
    allow_root: bool,
) -> Result<Option<Vec<u8>>> {
    for line in mountinfo
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.len() < 5 {
            bail!("malformed /proc/self/mountinfo record");
        }
        let mountpoint = decode_mountinfo_path(fields[4])?;
        if (!allow_root && mountpoint == root_bytes)
            || (mountpoint.len() > root_bytes.len()
                && mountpoint.starts_with(root_bytes)
                && mountpoint[root_bytes.len()] == b'/')
        {
            return Ok(Some(mountpoint));
        }
    }
    Ok(None)
}

fn decode_mountinfo_path(raw: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'\\' {
            let escape = raw
                .get(index + 1..index + 4)
                .context("truncated mountinfo escape")?;
            let value = match escape {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => bail!("unknown mountinfo path escape"),
            };
            decoded.push(value);
            index += 4;
        } else {
            decoded.push(raw[index]);
            index += 1;
        }
    }
    Ok(decoded)
}

fn proc_fd_path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}
