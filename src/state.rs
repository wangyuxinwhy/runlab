use std::fs;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::storage::{Database, LocalOciStore};

pub(crate) struct State {
    #[cfg(target_os = "linux")]
    root: PathBuf,
    oci: Arc<LocalOciStore>,
    database: Database,
}

impl State {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create State directory {}", root.display()))?;
        set_private_permissions(root)?;
        #[cfg(target_os = "linux")]
        {
            let engine = root.join("engine");
            fs::create_dir_all(&engine).with_context(|| {
                format!("failed to create Engine workspace {}", engine.display())
            })?;
            set_private_permissions(&engine)?;
        }
        let oci = Arc::new(LocalOciStore::open(root.join("oci"))?);
        let database = Database::open(&root.join("runlab.sqlite3"))?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            root: root.to_owned(),
            oci,
            database,
        })
    }

    pub(crate) fn oci(&self) -> Arc<LocalOciStore> {
        Arc::clone(&self.oci)
    }

    pub(crate) fn database(&self) -> &Database {
        &self.database
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn engine_workspace(&self) -> PathBuf {
        self.root.join("engine")
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect State directory {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
