use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use tempfile::TempDir;

use crate::integrity::{ensure_private_directory, write_new_output};
use crate::runtime::RuntimeConfig;

#[derive(Debug)]
pub struct OciBundle {
    directory: BundleDirectory,
    rootfs: PathBuf,
    config: PathBuf,
}

#[derive(Debug)]
enum BundleDirectory {
    #[cfg(test)]
    Temporary(TempDir),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    External(PathBuf),
}

impl BundleDirectory {
    fn path(&self) -> &Path {
        match self {
            #[cfg(test)]
            Self::Temporary(directory) => directory.path(),
            Self::External(path) => path,
        }
    }
}

impl OciBundle {
    #[cfg(test)]
    pub fn create(runtime: &RuntimeConfig) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("runlab-bundle-")
            .tempdir()
            .context("failed to create OCI bundle directory")?;
        Self::create_in(BundleDirectory::Temporary(directory), runtime)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn create_at(directory: &Path, runtime: &RuntimeConfig) -> Result<Self> {
        fs::create_dir(directory).with_context(|| {
            format!(
                "failed to create OCI bundle directory {}",
                directory.display()
            )
        })?;
        Self::create_in(BundleDirectory::External(directory.to_path_buf()), runtime)
    }

    fn create_in(directory: BundleDirectory, runtime: &RuntimeConfig) -> Result<Self> {
        ensure_private_directory(directory.path())?;

        let rootfs = directory.path().join("rootfs");
        ensure_private_directory(&rootfs)?;
        sync_directory(directory.path())?;

        let config = directory.path().join("config.json");
        write_new_output(&config, &runtime.encoded()?)?;

        let bundle = Self {
            directory,
            rootfs,
            config,
        };
        bundle.rootfs()?;
        bundle.config_path()?;
        Ok(bundle)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn rootfs(&self) -> Result<&Path> {
        verify_entry(self.path(), &self.rootfs, EntryKind::Directory)?;
        Ok(&self.rootfs)
    }

    pub fn config_path(&self) -> Result<&Path> {
        verify_entry(self.path(), &self.config, EntryKind::RegularFile)?;
        Ok(&self.config)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn preserve(self) -> PathBuf {
        match self.directory {
            #[cfg(test)]
            BundleDirectory::Temporary(directory) => directory.keep(),
            BundleDirectory::External(path) => path,
        }
    }
}

#[derive(Clone, Copy)]
enum EntryKind {
    Directory,
    RegularFile,
}

fn verify_entry(bundle: &Path, entry: &Path, expected: EntryKind) -> Result<()> {
    let metadata = fs::symlink_metadata(entry)
        .with_context(|| format!("failed to inspect OCI bundle path {}", entry.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "OCI bundle path must not be a symbolic link: {}",
            entry.display()
        );
    }
    let valid_type = match expected {
        EntryKind::Directory => metadata.is_dir(),
        EntryKind::RegularFile => metadata.is_file(),
    };
    if !valid_type {
        bail!(
            "OCI bundle path has an unexpected file type: {}",
            entry.display()
        );
    }

    let canonical_bundle = bundle
        .canonicalize()
        .with_context(|| format!("failed to canonicalize OCI bundle {}", bundle.display()))?;
    let canonical_entry = entry
        .canonicalize()
        .with_context(|| format!("failed to canonicalize OCI bundle path {}", entry.display()))?;
    if canonical_entry.parent() != Some(canonical_bundle.as_path()) {
        bail!("OCI bundle path escapes the bundle: {}", entry.display());
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("failed to open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn runtime() -> RuntimeConfig {
        RuntimeConfig::load(
            br#"{
                "ociVersion":"1.2.0",
                "root":{"path":"rootfs","readonly":false},
                "process":{
                    "terminal":false,
                    "user":{"uid":0,"gid":0},
                    "args":["/bin/true"],
                    "env":[],
                    "cwd":"/",
                    "noNewPrivileges":true
                },
                "hostname":"runlab",
                "linux":{"namespaces":[]}
            }"#,
        )
        .expect("runtime config")
    }

    #[test]
    fn creates_private_canonical_bundle_owned_by_its_lifetime() {
        let runtime = runtime();
        let bundle = OciBundle::create(&runtime).expect("bundle");
        let directory = bundle.path().to_path_buf();
        let rootfs = bundle.rootfs().expect("rootfs").to_path_buf();
        let config = bundle.config_path().expect("config").to_path_buf();

        assert!(rootfs.is_dir());
        assert_eq!(
            fs::read(&config).expect("config bytes"),
            runtime.encoded().expect("encoded runtime")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(
                fs::metadata(&directory)
                    .expect("bundle metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&rootfs)
                    .expect("rootfs metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        drop(bundle);
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_replaced_rootfs_symlink() {
        use std::os::unix::fs::symlink;

        let bundle = OciBundle::create(&runtime()).expect("bundle");
        let outside = tempfile::tempdir().expect("outside");
        fs::remove_dir(bundle.path().join("rootfs")).expect("remove rootfs");
        symlink(outside.path(), bundle.path().join("rootfs")).expect("symlink");

        let error = bundle.rootfs().expect_err("symlink must fail");
        assert!(error.to_string().contains("must not be a symbolic link"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_replaced_config_symlink() {
        use std::os::unix::fs::symlink;

        let bundle = OciBundle::create(&runtime()).expect("bundle");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        fs::remove_file(bundle.path().join("config.json")).expect("remove config");
        symlink(outside.path(), bundle.path().join("config.json")).expect("symlink");

        let error = bundle.config_path().expect_err("symlink must fail");
        assert!(error.to_string().contains("must not be a symbolic link"));
    }
}
