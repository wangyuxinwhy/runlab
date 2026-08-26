use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result as AnyResult, bail};

pub(super) fn current_cgroup_base() -> AnyResult<PathBuf> {
    let current = cgroup_path_from_proc(Path::new("/proc/self/cgroup"))?;
    if current == Path::new("/sys/fs/cgroup") {
        return Ok(current);
    }
    current
        .parent()
        .map(Path::to_path_buf)
        .context("current cgroup path has no parent")
}

fn cgroup_path_from_proc(path: &Path) -> AnyResult<PathBuf> {
    let mut bytes = Vec::with_capacity(4097);
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("{} exceeds 4096 bytes", path.display());
    }
    let text = std::str::from_utf8(&bytes).context("process cgroup data is not UTF-8")?;
    let relative = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("host does not expose a unified cgroup-v2 path")?;
    let relative = Path::new(relative);
    if !relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        bail!(
            "invalid unified cgroup path {}",
            relative.as_os_str().display()
        );
    }
    Ok(Path::new("/sys/fs/cgroup").join(
        relative
            .strip_prefix("/")
            .expect("validated absolute cgroup path"),
    ))
}

pub(super) fn observe_owned_cgroup(pid: u32, expected: &Path) -> AnyResult<PathBuf> {
    let path = PathBuf::from(format!("/proc/{pid}/cgroup"));
    let actual = cgroup_path_from_proc(&path)?;
    if actual != expected {
        bail!(
            "runc selected cgroup {}, expected the private default {}",
            actual.display(),
            expected.display()
        );
    }
    Ok(actual)
}
