use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use oci_spec::image::DigestAlgorithm;

use super::*;
use crate::{ContentErrorKind, OciContent, OciContentStore};

#[derive(Default)]
struct MemoryStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    opened: Mutex<Vec<String>>,
    published_media_types: Mutex<Vec<MediaType>>,
    fail_read_media_type: Mutex<Option<MediaType>>,
    fail_reads_after_publish: Mutex<Option<MediaType>>,
}

impl MemoryStore {
    fn insert_unchecked(&self, descriptor: &Descriptor, bytes: impl AsRef<[u8]>) {
        self.blobs
            .lock()
            .expect("blobs lock")
            .insert(descriptor.digest().to_string(), bytes.as_ref().to_vec());
    }

    fn fail_reads_after_publish(&self, media_type: MediaType) {
        *self
            .fail_reads_after_publish
            .lock()
            .expect("publish failure lock") = Some(media_type);
    }

    fn published(&self, media_type: &MediaType) -> bool {
        self.published_media_types
            .lock()
            .expect("publications lock")
            .contains(media_type)
    }

    fn clear_publications(&self) {
        self.published_media_types
            .lock()
            .expect("publications lock")
            .clear();
    }
}

impl OciContentStore for MemoryStore {
    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        self.opened
            .lock()
            .expect("opened lock")
            .push(descriptor.digest().to_string());
        if self
            .fail_read_media_type
            .lock()
            .expect("failure lock")
            .as_ref()
            == Some(descriptor.media_type())
        {
            return Err(ContentError::new(
                ContentErrorKind::Unavailable,
                "injected read failure",
            ));
        }
        self.blobs
            .lock()
            .expect("blobs lock")
            .get(&descriptor.digest().to_string())
            .cloned()
            .map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn OciContent>)
            .ok_or_else(|| ContentError::new(ContentErrorKind::Unavailable, "content is absent"))
    }

    fn publish(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<(), ContentError> {
        let mut bytes = Vec::new();
        content
            .read_to_end(&mut bytes)
            .map_err(|error| ContentError::new(ContentErrorKind::Internal, error.to_string()))?;
        let mut blobs = self.blobs.lock().expect("blobs lock");
        match blobs.get(&descriptor.digest().to_string()) {
            Some(existing) if existing != &bytes => Err(ContentError::new(
                ContentErrorKind::Rejected,
                "conflicting content",
            )),
            Some(_) => Ok(()),
            None => {
                blobs.insert(descriptor.digest().to_string(), bytes);
                self.published_media_types
                    .lock()
                    .expect("publications lock")
                    .push(descriptor.media_type().clone());
                if self
                    .fail_reads_after_publish
                    .lock()
                    .expect("publish failure lock")
                    .as_ref()
                    == Some(descriptor.media_type())
                {
                    *self.fail_read_media_type.lock().expect("failure lock") =
                        Some(descriptor.media_type().clone());
                }
                Ok(())
            }
        }
    }
}

struct EndlessStore;

impl OciContentStore for EndlessStore {
    fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        Ok(Box::new(EndlessContent))
    }

    fn publish(
        &self,
        _descriptor: &Descriptor,
        _content: &mut dyn Read,
    ) -> Result<(), ContentError> {
        Err(ContentError::new(
            ContentErrorKind::Rejected,
            "test store is read-only",
        ))
    }
}

struct EndlessContent;

impl Read for EndlessContent {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        buffer.fill(0);
        Ok(buffer.len())
    }
}

impl Seek for EndlessContent {
    fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
        Ok(0)
    }
}

struct CountingReader<R> {
    inner: R,
    bytes_read: usize,
    largest_request: usize,
}

impl<R> CountingReader<R> {
    const fn new(inner: R) -> Self {
        Self {
            inner,
            bytes_read: 0,
            largest_request: 0,
        }
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.largest_request = self.largest_request.max(buffer.len());
        let count = self.inner.read(buffer)?;
        self.bytes_read += count;
        Ok(count)
    }
}

#[test]
fn read_rejects_media_type_size_and_digest_mismatches() {
    let store = MemoryStore::default();
    let bytes = b"content";
    let valid = descriptor_for_bytes(MediaType::ImageConfig, bytes);
    store.insert_unchecked(&valid, bytes.as_slice());

    let media_error = read_small_verified(
        &store,
        &valid,
        &[MediaType::ImageManifest],
        MAX_CONFIG_BYTES,
        "subject",
    )
    .expect_err("media type mismatch");
    assert_eq!(media_error.kind(), OciErrorKind::Descriptor);

    let wrong_size = Descriptor::new(
        MediaType::ImageConfig,
        valid.size() + 1,
        valid.digest().clone(),
    );
    store.insert_unchecked(&wrong_size, bytes.as_slice());
    assert!(matches!(
        read_small_verified(
            &store,
            &wrong_size,
            &[MediaType::ImageConfig],
            MAX_CONFIG_BYTES,
            "subject"
        ),
        Err(OciError::Size { .. })
    ));

    let wrong_digest = Descriptor::new(
        MediaType::ImageConfig,
        u64::try_from(bytes.len()).expect("size"),
        Digest::try_from(format!("sha256:{}", "0".repeat(64))).expect("digest"),
    );
    store.insert_unchecked(&wrong_digest, bytes.as_slice());
    assert!(matches!(
        read_small_verified(
            &store,
            &wrong_digest,
            &[MediaType::ImageConfig],
            MAX_CONFIG_BYTES,
            "subject"
        ),
        Err(OciError::Digest { .. })
    ));
}

#[test]
fn complete_descriptors_and_exact_json_bytes_are_preserved() {
    let store = MemoryStore::default();
    let config_bytes = config_bytes(&[]);
    let mut config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
    config.set_urls(Some(vec!["https://example.test/config".to_owned()]));
    config.set_annotations(Some(HashMap::from([(
        "config".to_owned(),
        "preserved".to_owned(),
    )])));
    config.set_platform(Some(linux_platform_value("amd64")));
    config.set_artifact_type(Some(MediaType::ImageConfig));
    config.set_data(Some(base64_encode(&config_bytes)));
    store.insert_unchecked(&config, &config_bytes);

    let manifest_bytes = format!(
        "{{\n  \"schemaVersion\": 2, \"config\": {}, \"layers\": []\n}}",
        serde_json::to_string(&config).expect("descriptor JSON")
    )
    .into_bytes();
    let mut descriptor = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
    descriptor.set_urls(Some(vec!["https://example.test/manifest".to_owned()]));
    descriptor.set_annotations(Some(HashMap::from([(
        "example".to_owned(),
        "value".to_owned(),
    )])));
    descriptor.set_platform(Some(linux_platform_value("amd64")));
    descriptor.set_artifact_type(Some(MediaType::ImageConfig));
    descriptor.set_data(Some(base64_encode(&manifest_bytes)));
    store.insert_unchecked(&descriptor, manifest_bytes.clone());
    let image_descriptor = ImageDescriptor::new(descriptor.clone()).expect("image descriptor");

    let image = inspect_image(&store, &image_descriptor).expect("verified image");
    assert_eq!(image.manifest().descriptor(), &descriptor);
    assert_eq!(image.config().descriptor(), &config);
    assert_eq!(image.manifest().bytes(), manifest_bytes);
    assert_eq!(image.platform().os(), &Os::Linux);
}

#[test]
fn embedded_descriptor_data_must_equal_target_bytes() {
    let store = MemoryStore::default();
    let bytes = b"target";
    let mut descriptor = descriptor_for_bytes(MediaType::ImageConfig, bytes);
    descriptor.set_data(Some(base64_encode(b"other!")));
    store.insert_unchecked(&descriptor, bytes);

    let error = verify_content(&store, &descriptor, &[MediaType::ImageConfig], "config")
        .expect_err("embedded data mismatch");

    assert_eq!(error.kind(), OciErrorKind::Image);
    assert!(error.to_string().contains("config.data"));
}

#[test]
fn config_descriptor_platform_must_match_config() {
    let store = MemoryStore::default();
    let config_bytes = config_bytes_for_platform("arm64", Some("v8"), &[]);
    let mut config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
    config.set_platform(Some(
        serde_json::from_value(json!({"architecture": "arm64", "os": "linux", "variant": "v9"}))
            .expect("platform"),
    ));
    store.insert_unchecked(&config, config_bytes);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "config": config,
        "layers": []
    }))
    .expect("manifest JSON");
    let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
    store.insert_unchecked(&manifest, manifest_bytes);
    let image = ImageDescriptor::new(manifest).expect("image descriptor");

    let error = inspect_image(&store, &image).expect_err("platform mismatch");

    assert_eq!(error.kind(), OciErrorKind::Image);
    assert!(error.to_string().contains("manifest.config.platform"));
}

#[test]
fn manifest_rejects_duplicate_object_keys() {
    let store = MemoryStore::default();
    let config_bytes = config_bytes(&[]);
    let config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
    store.insert_unchecked(&config, config_bytes);
    let manifest_bytes = format!(
        "{{\"schemaVersion\":2,\"schemaVersion\":2,\"config\":{},\"layers\":[]}}",
        serde_json::to_string(&config).expect("descriptor JSON")
    )
    .into_bytes();
    let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
    store.insert_unchecked(&manifest, manifest_bytes);
    let image = ImageDescriptor::new(manifest).expect("image descriptor");

    let error = inspect_image(&store, &image).expect_err("duplicate manifest key");

    assert_eq!(error.kind(), OciErrorKind::Json);
    assert!(
        error
            .to_string()
            .contains("duplicate JSON key: schemaVersion")
    );
}

#[test]
fn config_rejects_duplicate_object_keys() {
    let store = MemoryStore::default();
    let config_bytes = br#"{
            "architecture":"amd64",
            "os":"linux",
            "os":"linux",
            "rootfs":{"type":"layers","diff_ids":[]}
        }"#;
    let config = descriptor_for_bytes(MediaType::ImageConfig, config_bytes);
    store.insert_unchecked(&config, config_bytes);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "config": config,
        "layers": []
    }))
    .expect("manifest JSON");
    let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
    store.insert_unchecked(&manifest, manifest_bytes);
    let image = ImageDescriptor::new(manifest).expect("image descriptor");

    let error = inspect_image(&store, &image).expect_err("duplicate config key");

    assert_eq!(error.kind(), OciErrorKind::Json);
    assert!(error.to_string().contains("duplicate JSON key: os"));
}

#[test]
fn verifies_all_six_oci_layer_media_types_against_uncompressed_diff_ids() {
    let store = MemoryStore::default();
    let raw = b"deterministic tar bytes";
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(raw).expect("gzip write");
    let gzip = gzip.finish().expect("gzip finish");
    let zstd = zstd::stream::encode_all(raw.as_slice(), 3).expect("zstd");
    let diff_id = digest_for(raw);
    let layers = [
        (MediaType::ImageLayer, raw.to_vec()),
        (MediaType::ImageLayerGzip, gzip.clone()),
        (MediaType::ImageLayerZstd, zstd.clone()),
        (MediaType::ImageLayerNonDistributable, raw.to_vec()),
        (MediaType::ImageLayerNonDistributableGzip, gzip),
        (MediaType::ImageLayerNonDistributableZstd, zstd),
    ];

    for (media_type, bytes) in layers {
        let layer = publish_bytes(&store, media_type, &bytes, "layer");
        verify_layer(
            &store,
            &layer,
            &diff_id,
            "layer",
            MAX_TOTAL_UNCOMPRESSED_LAYER_BYTES,
        )
        .expect("DiffID");
    }

    let bad = digest_for(b"different");
    let layer = publish_bytes(&store, MediaType::ImageLayer, raw, "bad layer");
    assert!(matches!(
        verify_layer(
            &store,
            &layer,
            &bad,
            "bad layer",
            MAX_TOTAL_UNCOMPRESSED_LAYER_BYTES
        ),
        Err(OciError::DiffId { .. })
    ));
}

#[test]
fn store_readers_are_bounded_at_descriptor_size_plus_one() {
    let descriptor = descriptor_for_bytes(MediaType::ImageLayer, b"abc");
    let extra_store = MemoryStore::default();
    extra_store.insert_unchecked(&descriptor, b"abcd");
    assert!(matches!(
        verify_content(&extra_store, &descriptor, &[MediaType::ImageLayer], "extra"),
        Err(OciError::Size {
            expected: 3,
            actual: 4,
            ..
        })
    ));

    let endless = EndlessStore;
    assert!(matches!(
        verify_content(&endless, &descriptor, &[MediaType::ImageLayer], "endless"),
        Err(OciError::Size {
            expected: 3,
            actual: 4,
            ..
        })
    ));
}

#[test]
fn cumulative_compressed_layer_size_is_rejected_before_any_layer_open() {
    let store = MemoryStore::default();
    let diff_ids = [digest_for(b"one"), digest_for(b"two")];
    let config_bytes = config_bytes(&diff_ids);
    let config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
    store.insert_unchecked(&config, config_bytes);
    let first = Descriptor::new(
        MediaType::ImageLayer,
        MAX_TOTAL_COMPRESSED_LAYER_BYTES,
        digest_for(b"first"),
    );
    let second = Descriptor::new(MediaType::ImageLayer, 1, digest_for(b"second"));
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "config": config,
        "layers": [&first, &second]
    }))
    .expect("manifest JSON");
    let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
    store.insert_unchecked(&manifest, manifest_bytes);
    let image = ImageDescriptor::new(manifest).expect("image descriptor");

    let error = inspect_image(&store, &image).expect_err("compressed size limit");

    assert!(matches!(error, OciError::CompressedLayerLimit { .. }));
    let opened = store.opened.lock().expect("opened lock");
    assert!(!opened.contains(&first.digest().to_string()));
    assert!(!opened.contains(&second.digest().to_string()));
}

#[test]
fn later_layer_stops_at_remaining_uncompressed_budget_plus_one() {
    let store = MemoryStore::default();
    let image = publish_image(
        &store,
        &[
            (MediaType::ImageLayer, b"one".to_vec()),
            (MediaType::ImageLayer, b"two".to_vec()),
        ],
    );
    let limits = ImageLimits {
        uncompressed_layer_bytes: 5,
        ..IMAGE_LIMITS
    };

    let error = inspect_image_with_limits(&store, &image, limits).expect_err("remaining budget");

    assert!(matches!(
        error,
        OciError::LayerLimit {
            limit: 2,
            actual: 3,
            ..
        }
    ));
}

#[test]
fn final_image_rejects_layer_1025_before_publication() {
    let store = MemoryStore::default();
    let parent_layers = vec![(MediaType::ImageLayer, Vec::new()); MAX_IMAGE_LAYERS];
    let parent = publish_image(&store, &parent_layers);
    let added = publish_bytes(&store, MediaType::ImageLayer, b"", "added layer");
    store.clear_publications();

    let error = publish_final_image(&store, &parent, Some((added, digest_for(b""))))
        .expect_err("Layer 1025");

    assert_eq!(error.kind(), OciErrorKind::Image);
    assert!(error.to_string().contains("contains 1025 entries"));
    assert!(!store.published(&MediaType::ImageConfig));
    assert!(!store.published(&MediaType::ImageManifest));
}

#[test]
fn final_layer_uses_parent_remaining_uncompressed_budget() {
    let store = MemoryStore::default();
    let parent = publish_image(&store, &[(MediaType::ImageLayer, b"one".to_vec())]);
    let added = publish_bytes(&store, MediaType::ImageLayer, b"two", "added layer");
    store.clear_publications();
    let limits = ImageLimits {
        uncompressed_layer_bytes: 5,
        ..IMAGE_LIMITS
    };

    let error =
        publish_final_image_with_limits(&store, &parent, Some((added, digest_for(b"two"))), limits)
            .expect_err("remaining final budget");

    assert!(matches!(
        error,
        OciError::LayerLimit {
            limit: 2,
            actual: 3,
            ..
        }
    ));
    assert!(!store.published(&MediaType::ImageConfig));
    assert!(!store.published(&MediaType::ImageManifest));
}

#[test]
fn generated_config_and_manifest_limits_fail_before_config_publication() {
    let store = MemoryStore::default();
    let parent = publish_image(&store, &[]);
    let parent_image = inspect_image(&store, &parent).expect("parent image");
    let added = publish_bytes(&store, MediaType::ImageLayer, b"new", "added layer");
    store.clear_publications();
    let config_limits = ImageLimits {
        config_bytes: u64::try_from(parent_image.config().bytes().len()).expect("config size"),
        ..IMAGE_LIMITS
    };

    let config_error = publish_final_image_with_limits(
        &store,
        &parent,
        Some((added.clone(), digest_for(b"new"))),
        config_limits,
    )
    .expect_err("generated Config limit");

    assert!(matches!(
        config_error,
        OciError::JsonLimit { ref path, .. } if path == "final.config"
    ));
    assert!(!store.published(&MediaType::ImageConfig));
    assert!(!store.published(&MediaType::ImageManifest));

    let manifest_limits = ImageLimits {
        manifest_bytes: u64::try_from(parent_image.manifest().bytes().len())
            .expect("manifest size"),
        ..IMAGE_LIMITS
    };
    let manifest_error = publish_final_image_with_limits(
        &store,
        &parent,
        Some((added, digest_for(b"new"))),
        manifest_limits,
    )
    .expect_err("generated Manifest limit");

    assert!(matches!(
        manifest_error,
        OciError::JsonLimit { ref path, .. } if path == "final.manifest"
    ));
    assert!(!store.published(&MediaType::ImageConfig));
    assert!(!store.published(&MediaType::ImageManifest));
}

#[test]
fn image_rejects_layer_and_diff_id_count_mismatch() {
    let store = MemoryStore::default();
    let layer = publish_bytes(&store, MediaType::ImageLayer, b"layer", "layer");
    let config_bytes = config_bytes(&[]);
    let config = publish_bytes(&store, MediaType::ImageConfig, &config_bytes, "config");
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "config": config,
        "layers": [layer]
    }))
    .expect("manifest JSON");
    let manifest = publish_bytes(
        &store,
        MediaType::ImageManifest,
        &manifest_bytes,
        "manifest",
    );
    let image = ImageDescriptor::new(manifest).expect("image descriptor");

    let error = inspect_image(&store, &image).expect_err("count mismatch");

    assert_eq!(error.kind(), OciErrorKind::Image);
    assert!(error.to_string().contains("contains 1 entries"));
    assert!(error.to_string().contains("diff_ids contains 0"));
}

#[test]
fn manifest_is_published_last_and_not_published_when_reference_check_fails() {
    let store = MemoryStore::default();
    let parent = publish_image(&store, &[]);
    let raw = b"new layer";
    let layer = publish_bytes(&store, MediaType::ImageLayer, raw, "new layer");
    store.fail_reads_after_publish(MediaType::ImageConfig);

    let error = publish_final_image(&store, &parent, Some((layer, digest_for(raw))))
        .expect_err("injected config failure");

    assert_eq!(error.kind(), OciErrorKind::Content);
    assert!(store.published(&MediaType::ImageConfig));
    assert!(!store.published(&MediaType::ImageManifest));
}

#[test]
fn unchanged_final_image_revalidates_and_returns_complete_parent_without_publication() {
    let store = MemoryStore::default();
    let parent = publish_image(&store, &[]);
    let bytes = store
        .blobs
        .lock()
        .expect("blobs lock")
        .get(&parent.as_oci().digest().to_string())
        .expect("parent manifest")
        .clone();
    let mut descriptor = descriptor_for_bytes(MediaType::ImageManifest, &bytes);
    descriptor.set_urls(Some(vec!["https://example.test/image".to_owned()]));
    descriptor.set_annotations(Some(HashMap::from([(
        "identity".to_owned(),
        "preserved".to_owned(),
    )])));
    descriptor.set_platform(Some(linux_platform_value("amd64")));
    descriptor.set_data(Some(base64_encode(&bytes)));
    store.insert_unchecked(&descriptor, bytes);
    let parent = ImageDescriptor::new(descriptor.clone()).expect("image descriptor");

    let final_image = publish_final_image(&store, &parent, None).expect("unchanged image");

    assert_eq!(final_image.into_oci(), descriptor);
    assert!(
        store
            .published_media_types
            .lock()
            .expect("publications lock")
            .is_empty()
    );
}

#[test]
fn unchanged_final_image_does_not_bypass_missing_parent_content() {
    let store = MemoryStore::default();
    let descriptor = descriptor_for_bytes(MediaType::ImageManifest, b"absent");
    let parent = ImageDescriptor::new(descriptor).expect("image descriptor");

    let error = publish_final_image(&store, &parent, None).expect_err("missing parent");

    assert_eq!(error.kind(), OciErrorKind::Content);
    assert!(!store.published(&MediaType::ImageManifest));
}

#[test]
fn final_image_is_deterministic_and_readable_with_manifest_last() {
    let store = MemoryStore::default();
    let parent = publish_image(&store, &[]);
    let raw = b"new layer";
    let layer = publish_bytes(&store, MediaType::ImageLayer, raw, "new layer");
    let first = publish_final_image(&store, &parent, Some((layer.clone(), digest_for(raw))))
        .expect("final image");
    let second = publish_final_image(&store, &parent, Some((layer, digest_for(raw))))
        .expect("same final image");

    assert_eq!(first, second);
    let image = inspect_image(&store, &first).expect("readable final image");
    assert_eq!(image.layers().len(), 1);
    assert_eq!(image.diff_ids(), &[digest_for(raw)]);
    assert_eq!(
        store
            .published_media_types
            .lock()
            .expect("publications lock")
            .last(),
        Some(&MediaType::ImageManifest)
    );
}

fn publish_image(store: &MemoryStore, layers: &[(MediaType, Vec<u8>)]) -> ImageDescriptor {
    let mut descriptors = Vec::new();
    let mut diff_ids = Vec::new();
    for (index, (media_type, bytes)) in layers.iter().enumerate() {
        let layer = publish_bytes(store, media_type.clone(), bytes, format!("layer[{index}]"));
        descriptors.push(layer);
        diff_ids.push(digest_for(bytes));
    }
    let config_bytes = config_bytes(&diff_ids);
    let config = publish_bytes(store, MediaType::ImageConfig, &config_bytes, "config");
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MediaType::ImageManifest,
        "config": config,
        "layers": descriptors,
        "x-extension": {"preserved": true}
    }))
    .expect("manifest JSON");
    let manifest = publish_bytes(store, MediaType::ImageManifest, &manifest_bytes, "manifest");
    // The fixture's publication should not count as the operation under test.
    store
        .published_media_types
        .lock()
        .expect("publications lock")
        .clear();
    ImageDescriptor::new(manifest).expect("image descriptor")
}

fn config_bytes(diff_ids: &[Digest]) -> Vec<u8> {
    config_bytes_for_platform("amd64", None, diff_ids)
}

fn config_bytes_for_platform(
    architecture: &str,
    variant: Option<&str>,
    diff_ids: &[Digest],
) -> Vec<u8> {
    let diff_ids = diff_ids.iter().map(ToString::to_string).collect::<Vec<_>>();
    let mut value = json!({
        "architecture": architecture,
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": diff_ids},
        "config": {"Env": ["A=B"]},
        "x-extension": {"preserved": true}
    });
    if let Some(variant) = variant {
        value
            .as_object_mut()
            .expect("config object")
            .insert("variant".to_owned(), Value::String(variant.to_owned()));
    }
    serde_json::to_vec(&value).expect("config JSON")
}

fn digest_for(bytes: &[u8]) -> Digest {
    Digest::try_from(format!("sha256:{}", hex_sha256(bytes))).expect("digest")
}

fn linux_platform_value(architecture: &str) -> Platform {
    serde_json::from_value(json!({"architecture": architecture, "os": "linux"})).expect("platform")
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[usize::from(third & 0x3f)]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

#[test]
fn digest_algorithm_error_is_typed() {
    let store = MemoryStore::default();
    let bytes = b"content";
    let descriptor = Descriptor::new(
        MediaType::ImageConfig,
        u64::try_from(bytes.len()).expect("size"),
        Digest::try_from(format!("sha512:{}", "0".repeat(128))).expect("digest"),
    );
    store.insert_unchecked(&descriptor, bytes.as_slice());
    let error = read_small_verified(
        &store,
        &descriptor,
        &[MediaType::ImageConfig],
        MAX_CONFIG_BYTES,
        "config",
    )
    .expect_err("unsupported algorithm");
    assert!(matches!(error, OciError::DigestAlgorithm { .. }));
    assert_eq!(descriptor.digest().algorithm(), &DigestAlgorithm::Sha512);
}

#[test]
fn decompressed_layer_limit_is_enforced_while_streaming() {
    let error = digest_stream_limited(&mut Cursor::new(b"too large"), "layer", 3)
        .expect_err("stream must stop at the declared limit");

    assert!(matches!(
        error,
        OciError::LayerLimit {
            limit: 3,
            actual: 4,
            ..
        }
    ));
}

#[test]
fn layer_budget_observes_at_most_remaining_plus_one_raw_and_decoded_bytes() {
    const REMAINING: u64 = 3;
    let raw = vec![b'x'; 128 * 1024];
    let mut raw_reader = CountingReader::new(Cursor::new(&raw));

    let raw_error = digest_stream_limited(&mut raw_reader, "raw layer", REMAINING)
        .expect_err("raw Layer exceeds remaining budget");

    assert!(matches!(
        raw_error,
        OciError::LayerLimit {
            limit: REMAINING,
            actual: 4,
            ..
        }
    ));
    assert_eq!(raw_reader.bytes_read, 4);
    assert_eq!(raw_reader.largest_request, 4);

    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&raw).expect("gzip write");
    let gzip = gzip.finish().expect("gzip finish");
    let decoder = MultiGzDecoder::new(Cursor::new(gzip));
    let mut decoded_reader = CountingReader::new(decoder);

    let decoded_error = digest_stream_limited(&mut decoded_reader, "gzip layer", REMAINING)
        .expect_err("decoded Layer exceeds remaining budget");

    assert!(matches!(
        decoded_error,
        OciError::LayerLimit {
            limit: REMAINING,
            actual: 4,
            ..
        }
    ));
    assert_eq!(decoded_reader.bytes_read, 4);
    assert_eq!(decoded_reader.largest_request, 4);
}

#[test]
fn generated_json_is_stable_for_unknown_object_fields() {
    let value = Value::Object(Map::from_iter(BTreeMap::from([
        ("z".to_owned(), Value::from(1)),
        ("a".to_owned(), Value::from(2)),
    ])));
    assert_eq!(
        json_bytes(&value, "value").expect("JSON"),
        br#"{"a":2,"z":1}"#
    );
}

fn publish_bytes(
    store: &MemoryStore,
    media_type: MediaType,
    bytes: &[u8],
    path: impl Into<String>,
) -> Descriptor {
    let mut reader = Cursor::new(bytes);
    publish_content(store, media_type, &mut reader, path).expect("published content")
}
