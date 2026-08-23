use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::core::{ServiceName, TcpReadinessCondition};
use crate::integrity::{canonical_json, read_bounded_file};

const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedServiceFile {
    pub name: ServiceName,
    pub initial_image: String,
    pub runtime_config_file: PathBuf,
    pub readiness: TcpReadinessCondition,
}

impl ManagedServiceFile {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = read_bounded_file(path, MAX_FILE_BYTES)?;
        let document: ManagedServiceDocument = serde_json::from_slice(&bytes)
            .with_context(|| format!("Managed Service file is invalid: {}", path.display()))?;
        if document.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported Managed Service file schema version: expected {SCHEMA_VERSION}, received {}",
                document.schema_version
            );
        }
        validate_runtime_config_path(&document.runtime_config_file)?;
        let runtime_config_file = if document.runtime_config_file.is_absolute() {
            document.runtime_config_file
        } else {
            path.parent()
                .unwrap_or_else(|| Path::new(""))
                .join(document.runtime_config_file)
        };
        let readiness = match document.readiness {
            ReadinessDocument::Tcp {
                port,
                timeout_seconds,
            } => TcpReadinessCondition {
                port,
                timeout_seconds,
            },
        };
        readiness.validate()?;
        Ok(Self {
            name: document.name,
            initial_image: document.initial_manifest,
            runtime_config_file,
            readiness,
        })
    }
}

pub(crate) fn rewrite_runtime_config_reference(
    bytes: &[u8],
    rewrite: impl FnOnce(&str) -> Result<PathBuf>,
) -> Result<Vec<u8>> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("Managed Service file exceeds {MAX_FILE_BYTES} bytes");
    }
    let mut document: ManagedServiceDocument =
        serde_json::from_slice(bytes).context("Managed Service file is invalid")?;
    if document.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported Managed Service file schema version: expected {SCHEMA_VERSION}, received {}",
            document.schema_version
        );
    }
    validate_runtime_config_path(&document.runtime_config_file)?;
    document.readiness.validate()?;
    let reference = document
        .runtime_config_file
        .to_str()
        .context("Managed Service Runtime Config path is not valid Unicode")?;
    document.runtime_config_file = rewrite(reference)?;
    let mut encoded = canonical_json(&document)?;
    encoded.push(b'\n');
    Ok(encoded)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManagedServiceDocument {
    schema_version: u32,
    name: ServiceName,
    initial_manifest: String,
    runtime_config_file: PathBuf,
    readiness: ReadinessDocument,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ReadinessDocument {
    Tcp { port: u16, timeout_seconds: u64 },
}

impl ReadinessDocument {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Tcp {
                port,
                timeout_seconds,
            } => TcpReadinessCondition {
                port: *port,
                timeout_seconds: *timeout_seconds,
            }
            .validate(),
        }
    }
}

fn validate_runtime_config_path(path: &Path) -> Result<()> {
    let Some(value) = path.as_os_str().to_str() else {
        bail!("Managed Service Runtime Config path is not valid Unicode");
    };
    if value.is_empty() {
        bail!("Managed Service Runtime Config path must not be empty");
    }
    if value.contains('\0') {
        bail!("Managed Service Runtime Config path must not contain NUL");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn digest() -> String {
        format!("sha256:{}", "1".repeat(64))
    }

    fn document(runtime_config_file: &str) -> serde_json::Value {
        json!({
            "schema_version": 1,
            "name": "postgres",
            "initial_manifest": digest(),
            "runtime_config_file": runtime_config_file,
            "readiness": {
                "kind": "tcp",
                "port": 5432,
                "timeout_seconds": 30
            }
        })
    }

    fn write_document(directory: &Path, value: &serde_json::Value) -> PathBuf {
        let path = directory.join("service.json");
        fs::write(
            &path,
            serde_json::to_vec(value).expect("encode service file"),
        )
        .expect("write service file");
        path
    }

    #[test]
    fn loads_valid_file_without_reading_runtime_config() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write_document(directory.path(), &document("runtime/service.json"));

        let service = ManagedServiceFile::load(&path).expect("Managed Service file");

        assert_eq!(service.name.to_string(), "postgres");
        assert_eq!(service.initial_image, digest());
        assert_eq!(
            service.runtime_config_file,
            directory.path().join("runtime/service.json")
        );
        assert_eq!(
            service.readiness,
            TcpReadinessCondition {
                port: 5432,
                timeout_seconds: 30
            }
        );
        assert!(!service.runtime_config_file.exists());
    }

    #[test]
    fn preserves_absolute_runtime_config_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let runtime = directory.path().join("elsewhere/config.json");
        let path = write_document(
            directory.path(),
            &document(runtime.to_str().expect("Unicode path")),
        );

        let service = ManagedServiceFile::load(&path).expect("Managed Service file");

        assert_eq!(service.runtime_config_file, runtime);
    }

    #[test]
    fn rejects_duplicate_fields() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("service.json");
        fs::write(
            &path,
            format!(
                r#"{{"schema_version":1,"schema_version":1,"name":"postgres","initial_manifest":"{}","runtime_config_file":"runtime.json","readiness":{{"kind":"tcp","port":5432,"timeout_seconds":30}}}}"#,
                digest()
            ),
        )
        .expect("write service file");

        let error = ManagedServiceFile::load(&path).expect_err("duplicate field");

        assert!(format!("{error:#}").contains("duplicate field"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut value = document("runtime.json");
        value["unexpected"] = json!(true);
        let path = write_document(directory.path(), &value);

        let error = ManagedServiceFile::load(&path).expect_err("unknown field");

        assert!(format!("{error:#}").contains("unknown field"));
    }

    #[test]
    fn rejects_unknown_readiness_fields() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut value = document("runtime.json");
        value["readiness"]["command"] = json!("pg_isready");
        let path = write_document(directory.path(), &value);

        let error = ManagedServiceFile::load(&path).expect_err("unknown readiness field");

        assert!(format!("{error:#}").contains("unknown field"));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut value = document("runtime.json");
        value["schema_version"] = json!(2);
        let path = write_document(directory.path(), &value);

        let error = ManagedServiceFile::load(&path).expect_err("schema version");

        assert!(error.to_string().contains("expected 1, received 2"));
    }

    #[test]
    fn rejects_zero_readiness_port() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut value = document("runtime.json");
        value["readiness"]["port"] = json!(0);
        let path = write_document(directory.path(), &value);

        let error = ManagedServiceFile::load(&path).expect_err("zero port");

        assert!(error.to_string().contains("port must be nonzero"));
    }

    #[test]
    fn rejects_zero_readiness_timeout() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut value = document("runtime.json");
        value["readiness"]["timeout_seconds"] = json!(0);
        let path = write_document(directory.path(), &value);

        let error = ManagedServiceFile::load(&path).expect_err("zero timeout");

        assert!(error.to_string().contains("timeout must be nonzero"));
    }

    #[test]
    fn rejects_empty_and_nul_runtime_config_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let empty = write_document(directory.path(), &document(""));
        let empty_error = ManagedServiceFile::load(&empty).expect_err("empty path");
        assert!(empty_error.to_string().contains("must not be empty"));

        let nul = write_document(directory.path(), &document("runtime\0.json"));
        let nul_error = ManagedServiceFile::load(&nul).expect_err("NUL path");
        assert!(nul_error.to_string().contains("must not contain NUL"));
    }

    #[test]
    fn rejects_files_larger_than_one_mibibyte() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("service.json");
        let oversized = usize::try_from(MAX_FILE_BYTES).expect("test limit fits usize") + 1;
        fs::write(&path, vec![b' '; oversized]).expect("write oversized file");

        let error = ManagedServiceFile::load(&path).expect_err("oversized file");

        assert!(format!("{error:#}").contains("exceeds the 1048576-byte limit"));
    }

    #[test]
    fn rewrites_only_the_runtime_config_reference() {
        let bytes = serde_json::to_vec(&document("@input/3")).unwrap();
        let rewritten = rewrite_runtime_config_reference(&bytes, |reference| {
            assert_eq!(reference, "@input/3");
            Ok(PathBuf::from(
                "/var/lib/runlab/vm-inputs/op/runtime-config-3.json",
            ))
        })
        .expect("rewritten declaration");
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(
            value["runtime_config_file"],
            "/var/lib/runlab/vm-inputs/op/runtime-config-3.json"
        );
        assert_eq!(value["initial_manifest"], digest());
    }
}
