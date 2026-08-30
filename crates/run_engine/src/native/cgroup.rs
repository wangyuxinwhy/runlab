use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result as AnyResult, bail};

pub(super) fn current_cgroup_base() -> AnyResult<PathBuf> {
    current_cgroup_base_at(Path::new("/sys/fs/cgroup"), Path::new("/proc/self/cgroup"))
}

fn current_cgroup_base_at(root: &Path, membership: &Path) -> AnyResult<PathBuf> {
    let controllers = root.join("cgroup.controllers");
    if !fs::metadata(&controllers).is_ok_and(|metadata| metadata.is_file()) {
        bail!(
            "NativeEngine requires the unified cgroup v2 hierarchy mounted at {}; cgroup v1 and hybrid layouts are unsupported",
            root.display()
        );
    }
    let current = cgroup_path_from_proc(root, membership)?;
    if current == root {
        return Ok(current);
    }
    current
        .parent()
        .map(Path::to_path_buf)
        .context("current cgroup path has no parent")
}

fn cgroup_path_from_proc(root: &Path, path: &Path) -> AnyResult<PathBuf> {
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
    Ok(root.join(
        relative
            .strip_prefix("/")
            .expect("validated absolute cgroup path"),
    ))
}

pub(super) fn observe_owned_cgroup(pid: u32, expected: &Path) -> AnyResult<PathBuf> {
    let path = PathBuf::from(format!("/proc/{pid}/cgroup"));
    let actual = cgroup_path_from_proc(Path::new("/sys/fs/cgroup"), &path)?;
    if actual != expected {
        bail!(
            "runc selected cgroup {}, expected the private default {}",
            actual.display(),
            expected.display()
        );
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_v2_root_resolves_the_current_base() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let root = fixture.path().join("cgroup");
        fs::create_dir(&root).expect("cgroup root");
        fs::write(root.join("cgroup.controllers"), b"cpu memory pids\n").expect("controllers");
        let membership = fixture.path().join("self.cgroup");
        fs::write(&membership, b"0::/user.slice/runlab.scope\n").expect("membership");

        assert_eq!(
            current_cgroup_base_at(&root, &membership).expect("cgroup base"),
            root.join("user.slice")
        );
    }

    #[test]
    fn hybrid_layout_is_rejected_even_when_it_has_a_unified_membership() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let root = fixture.path().join("cgroup");
        fs::create_dir_all(root.join("unified")).expect("hybrid root");
        fs::write(
            root.join("unified/cgroup.controllers"),
            b"cpu memory pids\n",
        )
        .expect("nested controllers");
        let membership = fixture.path().join("self.cgroup");
        fs::write(&membership, b"0::/runlab\n1:name=systemd:/runlab\n").expect("membership");

        let error = current_cgroup_base_at(&root, &membership).expect_err("hybrid rejection");
        assert!(
            error
                .to_string()
                .contains("cgroup v1 and hybrid layouts are unsupported")
        );
    }
}
