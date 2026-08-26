use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use oci_spec::image::{Descriptor, Digest, MediaType};
use run_protocol::{EngineError, ImageDescriptor, ProgramId};
use rustix::process::geteuid;
use serde_json::json;
use tempfile::TempDir;

use super::fixtures::*;
use crate::native::budget::{BudgetedStore, OperationBudget};
use crate::rootfs::{MaterializationFault, Rootfs, with_materialization_fault};
use crate::{ContentError, ContentErrorKind, OciContent, OciContentStore};

struct OpenFailureStore {
    kind: ContentErrorKind,
}

impl OciContentStore for OpenFailureStore {
    fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        Err(ContentError::new(self.kind, "injected open failure"))
    }

    fn publish(
        &self,
        _descriptor: &Descriptor,
        _content: &mut dyn std::io::Read,
    ) -> Result<(), ContentError> {
        Err(ContentError::new(
            ContentErrorKind::Rejected,
            "test store is read-only",
        ))
    }
}

struct FixedBytesStore(Vec<u8>);

impl OciContentStore for FixedBytesStore {
    fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        Ok(Box::new(std::io::Cursor::new(self.0.clone())))
    }

    fn publish(
        &self,
        _descriptor: &Descriptor,
        _content: &mut dyn std::io::Read,
    ) -> Result<(), ContentError> {
        Err(ContentError::new(
            ContentErrorKind::Rejected,
            "test store is read-only",
        ))
    }
}

struct DisappearingLayerStore {
    inner: Arc<MemoryStore>,
    layer_digest: String,
    layer_opens: AtomicUsize,
    failure_kind: ContentErrorKind,
}

impl OciContentStore for DisappearingLayerStore {
    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        if descriptor.digest().to_string() == self.layer_digest
            && self.layer_opens.fetch_add(1, Ordering::Relaxed) >= 1
        {
            return Err(ContentError::new(
                self.failure_kind,
                "layer disappeared after OCI inspection",
            ));
        }
        self.inner.open(descriptor)
    }

    fn publish(
        &self,
        descriptor: &Descriptor,
        content: &mut dyn std::io::Read,
    ) -> Result<(), ContentError> {
        self.inner.publish(descriptor, content)
    }
}

fn image_with_one_layer(store: &MemoryStore) -> (ImageDescriptor, Descriptor) {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, "payload", b"x".as_slice())
        .expect("append layer file");
    builder.finish().expect("finish layer tar");
    let layer_bytes = builder.into_inner().expect("layer bytes");
    image_with_layer_bytes(store, &layer_bytes)
}

fn image_with_layer_bytes(
    store: &MemoryStore,
    layer_bytes: &[u8],
) -> (ImageDescriptor, Descriptor) {
    let layer = descriptor_for_test_bytes(MediaType::ImageLayer, layer_bytes);
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    let config_bytes = serde_json::to_vec(&json!({
        "architecture": architecture,
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": [layer.digest()]},
        "config": {}
    }))
    .expect("config bytes");
    let config = descriptor_for_test_bytes(MediaType::ImageConfig, &config_bytes);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": &config,
        "layers": [&layer]
    }))
    .expect("manifest bytes");
    let manifest = descriptor_for_test_bytes(MediaType::ImageManifest, &manifest_bytes);
    publish_test_blob(store, &layer, layer_bytes.to_vec());
    publish_test_blob(store, &config, config_bytes);
    publish_test_blob(store, &manifest, manifest_bytes);
    (
        ImageDescriptor::new(manifest).expect("image descriptor"),
        layer,
    )
}

#[test]
fn oci_preflight_preserves_protocol_error_categories() {
    let id = ProgramId::primary();
    for kind in [ContentErrorKind::Unavailable, ContentErrorKind::Rejected] {
        let error = crate::native::prepare::inspect_program_image(
            &id,
            &OpenFailureStore { kind },
            &test_image(),
        )
        .expect_err("required content is unavailable");
        assert!(matches!(error, EngineError::InputUnavailable { .. }));
    }

    let error = crate::native::prepare::inspect_program_image(
        &id,
        &OpenFailureStore {
            kind: ContentErrorKind::Internal,
        },
        &test_image(),
    )
    .expect_err("store failure is internal");
    assert!(matches!(error, EngineError::Internal { .. }));

    let malformed = b"{".to_vec();
    let descriptor = descriptor_for_test_bytes(MediaType::ImageManifest, &malformed);
    let error = crate::native::prepare::inspect_program_image(
        &id,
        &FixedBytesStore(malformed),
        &ImageDescriptor::new(descriptor).expect("image descriptor"),
    )
    .expect_err("invalid manifest bytes");
    assert!(matches!(error, EngineError::InvalidInput { .. }));

    let unsupported_digest =
        Digest::try_from(format!("sha512:{}", "0".repeat(128))).expect("sha512 digest");
    let descriptor = Descriptor::new(MediaType::ImageManifest, 0, unsupported_digest);
    let error = crate::native::prepare::inspect_program_image(
        &id,
        &FixedBytesStore(Vec::new()),
        &ImageDescriptor::new(descriptor).expect("image descriptor"),
    )
    .expect_err("unsupported digest algorithm");
    assert!(matches!(error, EngineError::UnsupportedInput { .. }));
}

#[test]
fn expired_store_budget_maps_to_internal_engine_error() {
    let budget = OperationBudget::new(Duration::ZERO, "category test").expect("deadline");
    let store = BudgetedStore::new(Arc::new(UnavailableStore), budget);
    let error =
        crate::native::prepare::inspect_program_image(&ProgramId::primary(), &store, &test_image())
            .expect_err("expired store budget");
    assert!(matches!(error, EngineError::Internal { .. }));
}

#[test]
fn materialize_second_open_disappearance_remains_input_unavailable() {
    let inner = Arc::new(MemoryStore::default());
    let (image, layer) = image_with_one_layer(&inner);
    let store = DisappearingLayerStore {
        inner,
        layer_digest: layer.digest().to_string(),
        layer_opens: AtomicUsize::new(0),
        failure_kind: ContentErrorKind::Unavailable,
    };
    let id = ProgramId::primary();
    let verified = crate::native::prepare::inspect_program_image(&id, &store, &image)
        .expect("first layer open verifies image");
    let workspace = tempfile::tempdir().expect("materialize workspace");
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private workspace");
    let bundle = workspace.path().join("bundle");
    fs::create_dir(&bundle).expect("bundle");
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700)).expect("private bundle");

    let error = crate::native::prepare::materialize_program_rootfs(&id, &bundle, &verified, &store)
        .expect_err("second layer open disappears");
    assert!(matches!(error, EngineError::InputUnavailable { .. }));
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs[\"primary\"].initial_environment")
    );
}

#[test]
fn materialize_internal_content_failure_remains_internal() {
    let inner = Arc::new(MemoryStore::default());
    let (image, layer) = image_with_one_layer(&inner);
    let store = DisappearingLayerStore {
        inner,
        layer_digest: layer.digest().to_string(),
        layer_opens: AtomicUsize::new(0),
        failure_kind: ContentErrorKind::Internal,
    };
    let id = ProgramId::primary();
    let verified = crate::native::prepare::inspect_program_image(&id, &store, &image)
        .expect("first layer open verifies image");
    let (_workspace, bundle) = private_test_bundle();
    let error = crate::native::prepare::materialize_program_rootfs(&id, &bundle, &verified, &store)
        .expect_err("second layer open fails internally");
    assert!(matches!(error, EngineError::Internal { .. }));
}

#[test]
fn materialize_maps_layer_validation_and_host_failures_without_text_matching() {
    if !geteuid().is_root() {
        return;
    }
    let invalid_layers = [
        layer_tar_with_replaced_paths(&[b"safe"], &[b"../escape"]),
        layer_tar_with_replaced_paths(&[b"first", b"second"], &[b"same", b"./same"]),
        truncated_layer_tar(),
    ];
    for layer_bytes in invalid_layers {
        let store = MemoryStore::default();
        let (image, _) = image_with_layer_bytes(&store, &layer_bytes);
        let (_workspace, bundle) = private_test_bundle();
        let error = inspect_and_materialize_test_image(&store, &image, &bundle)
            .expect_err("invalid Layer must be rejected");
        assert!(matches!(error, EngineError::InvalidInput { .. }), "{error}");
        assert_eq!(
            error.path().map(ToString::to_string).as_deref(),
            Some("programs[\"primary\"].initial_environment")
        );
    }

    let store = MemoryStore::default();
    let sparse = gnu_sparse_layer_tar();
    let (image, _) = image_with_layer_bytes(&store, &sparse);
    let (_workspace, bundle) = private_test_bundle();
    let error = inspect_and_materialize_test_image(&store, &image, &bundle)
        .expect_err("GNU sparse Layer is unsupported");
    assert!(matches!(error, EngineError::UnsupportedInput { .. }));
    assert_eq!(
        error.path().map(ToString::to_string).as_deref(),
        Some("programs[\"primary\"].initial_environment")
    );

    let store = MemoryStore::default();
    let (image, _) = image_with_one_layer(&store);
    let (_workspace, bundle) = private_test_bundle();
    fs::write(bundle.join("rootfs"), b"host collision").expect("rootfs collision");
    let error = inspect_and_materialize_test_image(&store, &image, &bundle)
        .expect_err("local rootfs creation failure is internal");
    assert!(matches!(error, EngineError::Internal { .. }), "{error}");
    assert!(error.path().is_none());
}

#[test]
fn engine_owned_materialization_io_and_apply_faults_are_internal() {
    if !geteuid().is_root() {
        return;
    }
    for fault in [
        MaterializationFault::CompressedRead,
        MaterializationFault::DecodedRead,
        MaterializationFault::ApplySyscall,
    ] {
        let store = MemoryStore::default();
        let (image, _) = image_with_one_layer(&store);
        let id = ProgramId::primary();
        let verified = crate::native::prepare::inspect_program_image(&id, &store, &image)
            .expect("verify image before fault injection");
        let (_workspace, bundle) = private_test_bundle();
        let error = with_materialization_fault(fault, || {
            crate::native::prepare::materialize_program_rootfs(&id, &bundle, &verified, &store)
        })
        .expect_err("Engine-owned materialization fault");
        assert!(
            matches!(error, EngineError::Internal { .. }),
            "{fault:?}: {error}"
        );
        assert!(error.path().is_none(), "{fault:?}");
    }
}

fn inspect_and_materialize_test_image(
    store: &MemoryStore,
    image: &ImageDescriptor,
    bundle: &Path,
) -> Result<Rootfs, EngineError> {
    let id = ProgramId::primary();
    let verified = crate::native::prepare::inspect_program_image(&id, store, image)?;
    crate::native::prepare::materialize_program_rootfs(&id, bundle, &verified, store)
}

fn private_test_bundle() -> (TempDir, PathBuf) {
    let workspace = tempfile::tempdir().expect("materialize workspace");
    fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
        .expect("private workspace");
    let bundle = workspace.path().join("bundle");
    fs::create_dir(&bundle).expect("bundle");
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700)).expect("private bundle");
    (workspace, bundle)
}

fn layer_tar_with_replaced_paths(original: &[&[u8]], replacement: &[&[u8]]) -> Vec<u8> {
    assert_eq!(original.len(), replacement.len());
    let mut builder = tar::Builder::new(Vec::new());
    for path in original {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                Path::new(std::ffi::OsStr::from_bytes(path)),
                &[][..],
            )
            .expect("append Layer entry");
    }
    builder.finish().expect("finish Layer");
    let mut bytes = builder.into_inner().expect("Layer bytes");
    for (index, path) in replacement.iter().enumerate() {
        replace_tar_header_path(&mut bytes[index * 512..][..512], path);
    }
    bytes
}

fn replace_tar_header_path(header: &mut [u8], path: &[u8]) {
    assert_eq!(header.len(), 512);
    assert!(path.len() <= 100);
    header[..100].fill(0);
    header[..path.len()].copy_from_slice(path);
    header[148..156].fill(b' ');
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
}

fn gnu_sparse_layer_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::GNUSparse);
    header.set_path("sparse").expect("sparse path");
    header.set_cksum();
    builder
        .append(&header, &[][..])
        .expect("append sparse entry");
    builder.finish().expect("finish sparse Layer");
    builder.into_inner().expect("sparse Layer bytes")
}

fn truncated_layer_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(10);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, "truncated", b"0123456789".as_slice())
        .expect("append complete source entry");
    builder.finish().expect("finish source Layer");
    let mut bytes = builder.into_inner().expect("source Layer bytes");
    bytes.truncate(513);
    bytes
}
