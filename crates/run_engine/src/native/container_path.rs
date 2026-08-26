use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result as AnyResult, bail};

pub(super) fn safe_container_path(path: &str) -> AnyResult<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        bail!("mount destination must be absolute");
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => result.push(value),
            _ => bail!("mount destination is not normalized"),
        }
    }
    if result.as_os_str().is_empty() {
        bail!("mounting over / is unsupported");
    }
    Ok(result)
}

pub(super) fn reject_symlink_ancestor(rootfs: &Path, relative: &Path) -> AnyResult<()> {
    let mut current = rootfs.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("mount destination traverses symlink {}", current.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
