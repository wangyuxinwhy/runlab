use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags, open, openat};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tar::{Archive, EntryType, Header};

use crate::core::{
    Architecture, Digest, OCI_IMAGE_CONFIG, OCI_IMAGE_INDEX, OCI_IMAGE_MANIFEST, OCI_LAYER_GZIP,
    OCI_LAYER_TAR, OCI_LAYER_ZSTD, OciDescriptor, Platform,
};
use crate::integrity::digest_bytes;
use crate::oci::OciLayout;

const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: u64 = 1_000_000;
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;
const MAX_ARCHIVE_PATH_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_DEPTH: u32 = 32;
const MAX_GRAPH_DESCRIPTORS: usize = 100_000;
const MAX_IMAGE_MANIFESTS: usize = 1024;
const MAX_IMAGE_LAYERS: usize = 1024;
const TAR_BLOCK_BYTES: u64 = 512;
const TAR_BLOCK_LENGTH: usize = 512;
const REFERENCE_ANNOTATION: &str = "org.opencontainers.image.ref.name";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImportSourceKind {
    #[serde(rename = "oci_layout")]
    Layout,
    #[serde(rename = "oci_archive")]
    Archive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IngestedImage {
    pub(crate) source_kind: ImportSourceKind,
    pub(crate) source_index: OciDescriptor,
    pub(crate) selected_manifest: OciDescriptor,
    pub(crate) verified_blobs: u64,
    pub(crate) verified_bytes: u64,
}

pub(crate) fn ingest_image(
    destination: &OciLayout,
    source_path: &Path,
    platform: Platform,
    manifest_digest: Option<&Digest>,
    source_reference: Option<&str>,
) -> Result<IngestedImage> {
    validate_source_destination(source_path, destination.root_path())?;
    let mut source = Source::open(source_path)?;
    let layout_bytes = source.read_member(b"oci-layout", MAX_JSON_BYTES)?;
    validate_layout_marker(&layout_bytes)?;
    let index_bytes = source.read_member(b"index.json", MAX_JSON_BYTES)?;
    let source_index = OciDescriptor {
        digest: digest_bytes(&index_bytes),
        size: u64::try_from(index_bytes.len()).context("OCI index size overflow")?,
        media_type: OCI_IMAGE_INDEX.to_owned(),
    };
    let index = parse_index(&index_bytes, "OCI Layout index.json")?;
    let roots = select_root_entries(
        index_entries(&index, "OCI Layout index.json")?,
        source_reference,
    )?;
    let mut candidates = discover_manifests(&mut source, roots)?;
    if let Some(requested) = manifest_digest {
        let candidate = candidates.get_mut(requested).with_context(|| {
            format!("requested OCI Image Manifest is not reachable from index.json: {requested}")
        })?;
        resolve_candidate_platform(&mut source, candidate)?;
    } else {
        resolve_missing_platforms(&mut source, &mut candidates)?;
    }
    let selected = select_manifest(&candidates, platform, manifest_digest)?;

    let manifest_bytes = source.read_descriptor(&selected, MAX_JSON_BYTES)?;
    let content = parse_image_manifest(&manifest_bytes)?;
    let config_bytes = source.read_descriptor(&content.config, MAX_JSON_BYTES)?;
    let selected_platform = parse_config_platform(&config_bytes)?;
    if selected_platform != platform {
        bail!(
            "selected OCI Image platform mismatch: expected {platform}, received {selected_platform}"
        );
    }
    if let Some(descriptor_platform) = candidates
        .get(&selected.digest)
        .and_then(|candidate| candidate.declared_platform.as_ref())
        && descriptor_platform.supported() != Some(selected_platform)
    {
        bail!(
            "OCI Image Manifest descriptor platform mismatch: expected {}/{}, received {selected_platform}",
            descriptor_platform.os,
            descriptor_platform.architecture
        );
    }

    let mut unique = BTreeMap::new();
    insert_unique_descriptor(&mut unique, selected.clone())?;
    insert_unique_descriptor(&mut unique, content.config.clone())?;
    for layer in &content.layers {
        insert_unique_descriptor(&mut unique, layer.clone())?;
    }
    let verified_bytes = unique.values().try_fold(0_u64, |total, descriptor| {
        total
            .checked_add(descriptor.size)
            .context("OCI Image graph size overflow")
    })?;
    if verified_bytes > MAX_ARCHIVE_BYTES {
        bail!("OCI Image graph exceeds the {MAX_ARCHIVE_BYTES}-byte compressed-content limit");
    }
    for layer in unique.values().filter(|descriptor| {
        matches!(
            descriptor.media_type.as_str(),
            OCI_LAYER_TAR | OCI_LAYER_GZIP | OCI_LAYER_ZSTD
        )
    }) {
        let imported = source.copy_descriptor(destination, layer)?;
        if imported != *layer {
            bail!("OCI Image Layer changed during ingestion: {}", layer.digest);
        }
    }
    let imported_config = destination.put_reader(
        Cursor::new(config_bytes),
        &content.config.media_type,
        Some(&content.config),
    )?;
    if imported_config != content.config {
        bail!("OCI Image Config changed during ingestion");
    }
    let imported_manifest = destination.put_reader(
        Cursor::new(manifest_bytes),
        &selected.media_type,
        Some(&selected),
    )?;
    if imported_manifest != selected {
        bail!("OCI Image Manifest changed during ingestion");
    }
    let verified_blobs =
        u64::try_from(unique.len()).context("OCI Image graph blob count overflow")?;
    Ok(IngestedImage {
        source_kind: source.kind(),
        source_index,
        selected_manifest: selected,
        verified_blobs,
        verified_bytes,
    })
}

pub(crate) fn validate_source_destination(
    source_path: &Path,
    destination_path: &Path,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source_path).with_context(|| {
        format!(
            "failed to inspect OCI import source {}",
            source_path.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "OCI import source must not be a symlink: {}",
            source_path.display()
        );
    }
    let source = source_path.canonicalize().with_context(|| {
        format!(
            "failed to resolve OCI import source {}",
            source_path.display()
        )
    })?;
    let destination = resolve_existing_or_future_path(destination_path)?;
    let overlaps = if metadata.is_dir() {
        source.starts_with(&destination) || destination.starts_with(&source)
    } else {
        source == destination
    };
    if overlaps {
        bail!("OCI import source and destination Layout must not overlap");
    }
    Ok(())
}

fn resolve_existing_or_future_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path)
    };
    let mut cursor = path.as_path();
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(_) => {
                let mut resolved = cursor.canonicalize().with_context(|| {
                    format!(
                        "failed to resolve destination ancestor {}",
                        cursor.display()
                    )
                })?;
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(normalize_absolute_path(&resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let leaf = cursor.file_name().with_context(|| {
                    format!(
                        "destination Layout has no existing ancestor: {}",
                        path.display()
                    )
                })?;
                suffix.push(leaf.to_os_string());
                cursor = cursor.parent().with_context(|| {
                    format!(
                        "destination Layout has no existing ancestor: {}",
                        path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect destination ancestor {}",
                        cursor.display()
                    )
                });
            }
        }
    }
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[derive(Debug)]
enum Source {
    Directory(DirectorySource),
    Archive(ArchiveSource),
}

impl Source {
    fn open(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect OCI import source {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "OCI import source must not be a symlink: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            Ok(Self::Directory(DirectorySource::open(path)?))
        } else if metadata.is_file() {
            Ok(Self::Archive(ArchiveSource::open(path)?))
        } else {
            bail!(
                "OCI import source must be a Layout directory or plain tar archive: {}",
                path.display()
            )
        }
    }

    const fn kind(&self) -> ImportSourceKind {
        match self {
            Self::Directory(_) => ImportSourceKind::Layout,
            Self::Archive(_) => ImportSourceKind::Archive,
        }
    }

    fn read_member(&mut self, name: &[u8], limit: u64) -> Result<Vec<u8>> {
        match self {
            Self::Directory(source) => {
                let file = source.open_member(name)?;
                read_bounded(file, limit, &String::from_utf8_lossy(name))
            }
            Self::Archive(source) => source.read_member(name, limit),
        }
    }

    fn read_descriptor(&mut self, descriptor: &OciDescriptor, limit: u64) -> Result<Vec<u8>> {
        if descriptor.size > limit {
            bail!(
                "OCI blob {} exceeds the {limit}-byte JSON limit",
                descriptor.digest
            );
        }
        let name = blob_member(&descriptor.digest);
        let bytes = self.read_member(&name, limit)?;
        verify_descriptor_bytes(&bytes, descriptor)?;
        Ok(bytes)
    }

    fn copy_descriptor(
        &mut self,
        destination: &OciLayout,
        descriptor: &OciDescriptor,
    ) -> Result<OciDescriptor> {
        let name = blob_member(&descriptor.digest);
        match self {
            Self::Directory(source) => destination.put_reader(
                source.open_member(&name)?,
                &descriptor.media_type,
                Some(descriptor),
            ),
            Self::Archive(source) => source.copy_member(destination, &name, descriptor),
        }
    }
}

#[derive(Debug)]
struct DirectorySource {
    root: OwnedFd,
    blobs: OwnedFd,
}

impl DirectorySource {
    fn open(path: &Path) -> Result<Self> {
        let root = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open OCI Layout {}", path.display()))?;
        let blobs = openat(
            &root,
            "blobs",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("failed to open OCI Layout blobs directory")?;
        let sha256 = openat(
            &blobs,
            "sha256",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("failed to open OCI Layout blobs/sha256 directory")?;
        Ok(Self {
            root,
            blobs: sha256,
        })
    }

    fn open_member(&self, name: &[u8]) -> Result<File> {
        let (directory, leaf) = if name == b"oci-layout" || name == b"index.json" {
            (&self.root, name)
        } else {
            let digest = name
                .strip_prefix(b"blobs/sha256/")
                .context("invalid OCI Layout member request")?;
            validate_hex(digest)?;
            (&self.blobs, digest)
        };
        let leaf = std::str::from_utf8(leaf).context("OCI Layout member name is not UTF-8")?;
        let file = File::from(
            openat(
                directory,
                leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| format!("failed to open OCI Layout member {leaf}"))?,
        );
        if !file
            .metadata()
            .with_context(|| format!("failed to inspect OCI Layout member {leaf}"))?
            .is_file()
        {
            bail!("OCI Layout member is not a regular file: {leaf}");
        }
        Ok(file)
    }
}

#[derive(Debug, Clone, Copy)]
struct ArchiveMember {
    offset: u64,
    size: u64,
}

#[derive(Debug)]
struct ArchiveSource {
    file: File,
    members: BTreeMap<Vec<u8>, ArchiveMember>,
}

impl ArchiveSource {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::from(
            open(
                path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| format!("failed to open OCI archive {}", path.display()))?,
        );
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect OCI archive {}", path.display()))?;
        if !metadata.is_file() {
            bail!(
                "OCI archive source is not a regular file: {}",
                path.display()
            );
        }
        if metadata.len() > MAX_ARCHIVE_BYTES {
            bail!(
                "OCI archive exceeds the {MAX_ARCHIVE_BYTES}-byte input limit: {}",
                path.display()
            );
        }
        preflight_archive(&mut file, metadata.len())?;
        file.seek(SeekFrom::Start(0))
            .context("failed to rewind OCI archive")?;
        let mut archive = Archive::new(file);
        let mut members = BTreeMap::new();
        let mut paths = BTreeSet::new();
        let mut entries_seen = 0_u64;
        let mut path_bytes_seen = 0_u64;
        {
            let entries = archive
                .entries_with_seek()
                .context("failed to read OCI archive")?;
            for entry in entries {
                let entry = entry.context("failed to read OCI archive entry")?;
                entries_seen = entries_seen
                    .checked_add(1)
                    .context("OCI archive entry count overflow")?;
                if entries_seen > MAX_ARCHIVE_ENTRIES {
                    bail!("OCI archive exceeds the {MAX_ARCHIVE_ENTRIES}-entry limit");
                }
                let raw_path = entry.path_bytes();
                path_bytes_seen = path_bytes_seen
                    .checked_add(
                        u64::try_from(raw_path.len()).context("OCI archive path size overflow")?,
                    )
                    .context("OCI archive path budget overflow")?;
                if path_bytes_seen > MAX_ARCHIVE_PATH_TOTAL_BYTES {
                    bail!(
                        "OCI archive paths exceed the {MAX_ARCHIVE_PATH_TOTAL_BYTES}-byte aggregate limit"
                    );
                }
                let path = normalize_archive_path(&raw_path)?;
                if path.is_empty() && entry.header().entry_type() == EntryType::Directory {
                    continue;
                }
                if !paths.insert(path.clone()) {
                    bail!(
                        "OCI archive contains duplicate member {}",
                        String::from_utf8_lossy(&path)
                    );
                }
                if entry.header().entry_type() == EntryType::Directory {
                    continue;
                }
                if entry.header().entry_type() != EntryType::Regular {
                    bail!(
                        "OCI archive member is not a regular file: {}",
                        String::from_utf8_lossy(&path)
                    );
                }
                let size = entry
                    .header()
                    .size()
                    .context("OCI archive member size is invalid")?;
                let end = entry
                    .raw_file_position()
                    .checked_add(size)
                    .context("OCI archive member range overflow")?;
                if end > metadata.len() {
                    bail!(
                        "OCI archive member exceeds the source file: {}",
                        String::from_utf8_lossy(&path)
                    );
                }
                members.insert(
                    path,
                    ArchiveMember {
                        offset: entry.raw_file_position(),
                        size,
                    },
                );
            }
        }
        Ok(Self {
            file: archive.into_inner(),
            members,
        })
    }

    fn member(&self, name: &[u8]) -> Result<ArchiveMember> {
        self.members
            .get(name)
            .copied()
            .with_context(|| format!("OCI archive is missing {}", String::from_utf8_lossy(name)))
    }

    fn read_member(&mut self, name: &[u8], limit: u64) -> Result<Vec<u8>> {
        let member = self.member(name)?;
        if member.size > limit {
            bail!(
                "OCI archive member {} exceeds the {limit}-byte limit",
                String::from_utf8_lossy(name)
            );
        }
        self.file
            .seek(SeekFrom::Start(member.offset))
            .context("failed to seek OCI archive member")?;
        read_bounded(
            Read::by_ref(&mut self.file).take(member.size),
            limit,
            &String::from_utf8_lossy(name),
        )
    }

    fn copy_member(
        &mut self,
        destination: &OciLayout,
        name: &[u8],
        descriptor: &OciDescriptor,
    ) -> Result<OciDescriptor> {
        let member = self.member(name)?;
        if member.size != descriptor.size {
            bail!(
                "OCI archive blob size mismatch for {}: expected {}, received {}",
                descriptor.digest,
                descriptor.size,
                member.size
            );
        }
        self.file
            .seek(SeekFrom::Start(member.offset))
            .context("failed to seek OCI archive blob")?;
        destination.put_reader(
            Read::by_ref(&mut self.file).take(member.size),
            &descriptor.media_type,
            Some(descriptor),
        )
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    descriptor: OciDescriptor,
    platform: Option<Platform>,
    declared_platform: Option<DeclaredPlatform>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredPlatform {
    os: String,
    architecture: String,
}

impl DeclaredPlatform {
    fn supported(&self) -> Option<Platform> {
        if self.os != "linux" {
            return None;
        }
        let architecture = match self.architecture.as_str() {
            "amd64" => Architecture::Amd64,
            "arm64" => Architecture::Arm64,
            _ => return None,
        };
        Some(Platform::linux(architecture))
    }
}

fn discover_manifests(
    source: &mut Source,
    roots: Vec<IndexEntry>,
) -> Result<BTreeMap<Digest, Candidate>> {
    let mut queue = VecDeque::new();
    for entry in roots {
        queue.push_back((entry, 0_u32));
    }
    let mut seen_indexes = BTreeSet::new();
    let mut seen_descriptors = BTreeMap::new();
    let mut candidates: BTreeMap<Digest, Candidate> = BTreeMap::new();
    while let Some((entry, depth)) = queue.pop_front() {
        if seen_descriptors.len() >= MAX_GRAPH_DESCRIPTORS {
            bail!("OCI Image graph exceeds the {MAX_GRAPH_DESCRIPTORS}-descriptor limit");
        }
        if let Some(existing) = seen_descriptors.get(&entry.descriptor.digest) {
            if existing != &entry.descriptor {
                bail!(
                    "OCI Image graph has conflicting descriptors for {}",
                    entry.descriptor.digest
                );
            }
        } else {
            seen_descriptors.insert(entry.descriptor.digest.clone(), entry.descriptor.clone());
        }
        match entry.descriptor.media_type.as_str() {
            OCI_IMAGE_MANIFEST => merge_candidate(&mut candidates, entry)?,
            OCI_IMAGE_INDEX => {
                if depth >= MAX_INDEX_DEPTH {
                    bail!("OCI Image Index nesting exceeds the {MAX_INDEX_DEPTH}-level limit");
                }
                if !seen_indexes.insert(entry.descriptor.digest.clone()) {
                    continue;
                }
                let bytes = source.read_descriptor(&entry.descriptor, MAX_JSON_BYTES)?;
                let index = parse_index(&bytes, "nested OCI Image Index")?;
                for child in index_entries(&index, "nested OCI Image Index")? {
                    queue.push_back((child, depth + 1));
                }
            }
            _ => {}
        }
    }
    if candidates.is_empty() {
        bail!("OCI Layout index does not reach an OCI Image Manifest");
    }
    if candidates.len() > MAX_IMAGE_MANIFESTS {
        bail!("OCI Layout exceeds the {MAX_IMAGE_MANIFESTS}-Manifest limit");
    }
    Ok(candidates)
}

#[derive(Debug, Clone)]
struct IndexEntry {
    descriptor: OciDescriptor,
    platform: Option<DeclaredPlatform>,
    reference: Option<String>,
}

fn index_entries(index: &Value, name: &str) -> Result<Vec<IndexEntry>> {
    index
        .as_object()
        .and_then(|object| object.get("manifests"))
        .and_then(Value::as_array)
        .with_context(|| format!("{name} manifests must be an array"))?
        .iter()
        .enumerate()
        .map(|(position, value)| {
            let descriptor = parse_descriptor(value, &format!("{name} manifests[{position}]"))?;
            let platform = if descriptor.media_type == OCI_IMAGE_MANIFEST {
                parse_descriptor_platform(value, name)?
            } else {
                None
            };
            let reference = parse_source_reference(value, name)?;
            Ok(IndexEntry {
                descriptor,
                platform,
                reference,
            })
        })
        .collect()
}

fn select_root_entries(
    entries: Vec<IndexEntry>,
    source_reference: Option<&str>,
) -> Result<Vec<IndexEntry>> {
    let Some(reference) = source_reference else {
        return Ok(entries);
    };
    if reference.is_empty() || reference.len() > 1024 || reference.contains(['\0', '\r', '\n']) {
        bail!("invalid OCI source reference");
    }
    let matching = entries
        .into_iter()
        .filter(|entry| entry.reference.as_deref() == Some(reference))
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => bail!("OCI Layout source reference is unknown: {reference}"),
        [_] => Ok(matching),
        _ => bail!("OCI Layout source reference is duplicated: {reference}"),
    }
}

fn parse_source_reference(value: &Value, name: &str) -> Result<Option<String>> {
    let Some(annotations) = value
        .as_object()
        .and_then(|object| object.get("annotations"))
    else {
        return Ok(None);
    };
    let annotations = annotations
        .as_object()
        .with_context(|| format!("{name} descriptor annotations must be an object"))?;
    annotations
        .get(REFERENCE_ANNOTATION)
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .with_context(|| format!("{name} source reference annotation must be a string"))
        })
        .transpose()
}

fn merge_candidate(candidates: &mut BTreeMap<Digest, Candidate>, entry: IndexEntry) -> Result<()> {
    if let Some(existing) = candidates.get_mut(&entry.descriptor.digest) {
        if existing.descriptor != entry.descriptor {
            bail!(
                "OCI Image graph has conflicting Manifest descriptors for {}",
                entry.descriptor.digest
            );
        }
        match (&existing.declared_platform, &entry.platform) {
            (Some(left), Some(right)) if left != right => bail!(
                "OCI Image Manifest {} has conflicting platform descriptors",
                entry.descriptor.digest
            ),
            (None, Some(platform)) => {
                existing.platform = platform.supported();
                existing.declared_platform = Some(platform.clone());
            }
            _ => {}
        }
    } else {
        let platform = entry
            .platform
            .as_ref()
            .and_then(DeclaredPlatform::supported);
        candidates.insert(
            entry.descriptor.digest.clone(),
            Candidate {
                descriptor: entry.descriptor,
                platform,
                declared_platform: entry.platform,
            },
        );
    }
    Ok(())
}

fn select_manifest(
    candidates: &BTreeMap<Digest, Candidate>,
    platform: Platform,
    requested: Option<&Digest>,
) -> Result<OciDescriptor> {
    if let Some(requested) = requested {
        return candidates
            .get(requested)
            .map(|candidate| candidate.descriptor.clone())
            .with_context(|| {
                format!(
                    "requested OCI Image Manifest is not reachable from index.json: {requested}"
                )
            });
    }
    let matching = candidates
        .values()
        .filter(|candidate| candidate.platform == Some(platform))
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [candidate] => Ok(candidate.descriptor.clone()),
        [] if candidates.len() == 1 => {
            let candidate = candidates.values().next().expect("one candidate");
            if candidate.platform.is_none() {
                Ok(candidate.descriptor.clone())
            } else {
                bail!("OCI Layout has no Image Manifest for {platform}")
            }
        }
        [] => bail!("OCI Layout has no unambiguous Image Manifest for {platform}; use --manifest"),
        _ => bail!("OCI Layout has multiple Image Manifests for {platform}; use --manifest"),
    }
}

struct ManifestContent {
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
}

fn parse_image_manifest(bytes: &[u8]) -> Result<ManifestContent> {
    let value: Value =
        serde_json::from_slice(bytes).context("OCI Image Manifest is invalid JSON")?;
    let object = value
        .as_object()
        .context("OCI Image Manifest must be an object")?;
    if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        bail!("OCI Image Manifest schemaVersion must be 2");
    }
    if let Some(media_type) = object.get("mediaType")
        && media_type.as_str() != Some(OCI_IMAGE_MANIFEST)
    {
        bail!("selected OCI Image Manifest mediaType is invalid");
    }
    let config = parse_descriptor(
        object
            .get("config")
            .context("OCI Image Manifest lacks config")?,
        "OCI Image Manifest config",
    )?;
    if config.media_type != OCI_IMAGE_CONFIG {
        bail!("OCI Image Manifest config has an unsupported mediaType");
    }
    let layers = object
        .get("layers")
        .and_then(Value::as_array)
        .context("OCI Image Manifest layers must be an array")?
        .iter()
        .enumerate()
        .map(|(position, value)| {
            let descriptor =
                parse_descriptor(value, &format!("OCI Image Manifest layers[{position}]"))?;
            if !matches!(
                descriptor.media_type.as_str(),
                OCI_LAYER_TAR | OCI_LAYER_GZIP | OCI_LAYER_ZSTD
            ) {
                bail!(
                    "OCI Image Layer has an unsupported mediaType: {}",
                    descriptor.media_type
                );
            }
            Ok(descriptor)
        })
        .collect::<Result<Vec<_>>>()?;
    if layers.len() > MAX_IMAGE_LAYERS {
        bail!("OCI Image exceeds the {MAX_IMAGE_LAYERS}-Layer limit");
    }
    Ok(ManifestContent { config, layers })
}

fn resolve_missing_platforms(
    source: &mut Source,
    candidates: &mut BTreeMap<Digest, Candidate>,
) -> Result<()> {
    for candidate in candidates.values_mut() {
        resolve_candidate_platform(source, candidate)?;
    }
    Ok(())
}

fn resolve_candidate_platform(source: &mut Source, candidate: &mut Candidate) -> Result<()> {
    if candidate.declared_platform.is_some() || candidate.platform.is_some() {
        return Ok(());
    }
    let manifest = source.read_descriptor(&candidate.descriptor, MAX_JSON_BYTES)?;
    let content = parse_image_manifest(&manifest)?;
    let config = source.read_descriptor(&content.config, MAX_JSON_BYTES)?;
    candidate.platform = parse_supported_config_platform(&config)?;
    Ok(())
}

fn parse_supported_config_platform(bytes: &[u8]) -> Result<Option<Platform>> {
    let value: Value = serde_json::from_slice(bytes).context("OCI Image Config is invalid JSON")?;
    let object = value
        .as_object()
        .context("OCI Image Config must be an object")?;
    let Some(os) = object.get("os").and_then(Value::as_str) else {
        bail!("OCI Image Config lacks operating system");
    };
    let Some(architecture) = object.get("architecture").and_then(Value::as_str) else {
        bail!("OCI Image Config lacks architecture");
    };
    if os != "linux" {
        return Ok(None);
    }
    let architecture = match architecture {
        "amd64" => Architecture::Amd64,
        "arm64" => Architecture::Arm64,
        _ => return Ok(None),
    };
    Ok(Some(Platform::linux(architecture)))
}

fn insert_unique_descriptor(
    descriptors: &mut BTreeMap<Digest, OciDescriptor>,
    descriptor: OciDescriptor,
) -> Result<()> {
    if let Some(existing) = descriptors.get(&descriptor.digest)
        && existing != &descriptor
    {
        bail!(
            "OCI Image graph has conflicting descriptors for {}",
            descriptor.digest
        );
    }
    descriptors.insert(descriptor.digest.clone(), descriptor);
    Ok(())
}

fn parse_config_platform(bytes: &[u8]) -> Result<Platform> {
    let value: Value = serde_json::from_slice(bytes).context("OCI Image Config is invalid JSON")?;
    let object = value
        .as_object()
        .context("OCI Image Config must be an object")?;
    if object.get("os").and_then(Value::as_str) != Some("linux") {
        bail!("OCI Image has an unsupported operating system");
    }
    let architecture = object
        .get("architecture")
        .and_then(Value::as_str)
        .context("OCI Image Config lacks architecture")?
        .parse::<Architecture>()?;
    Ok(Platform::linux(architecture))
}

fn parse_index(bytes: &[u8], name: &str) -> Result<Value> {
    let value: Value =
        serde_json::from_slice(bytes).with_context(|| format!("{name} is invalid JSON"))?;
    let object = value
        .as_object()
        .with_context(|| format!("{name} must be an object"))?;
    if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        bail!("{name} schemaVersion must be 2");
    }
    if let Some(media_type) = object.get("mediaType")
        && media_type.as_str() != Some(OCI_IMAGE_INDEX)
    {
        bail!("{name} mediaType is invalid");
    }
    if !object.get("manifests").is_some_and(Value::is_array) {
        bail!("{name} manifests must be an array");
    }
    Ok(value)
}

fn parse_descriptor(value: &Value, name: &str) -> Result<OciDescriptor> {
    let object = value
        .as_object()
        .with_context(|| format!("{name} must be a descriptor"))?;
    Ok(OciDescriptor {
        media_type: object
            .get("mediaType")
            .and_then(Value::as_str)
            .with_context(|| format!("{name} mediaType is invalid"))?
            .to_owned(),
        digest: Digest::parse(
            object
                .get("digest")
                .and_then(Value::as_str)
                .with_context(|| format!("{name} digest is invalid"))?,
        )?,
        size: object
            .get("size")
            .and_then(Value::as_u64)
            .with_context(|| format!("{name} size is invalid"))?,
    })
}

fn parse_descriptor_platform(value: &Value, name: &str) -> Result<Option<DeclaredPlatform>> {
    let Some(platform) = value.as_object().and_then(|object| object.get("platform")) else {
        return Ok(None);
    };
    let platform = platform
        .as_object()
        .with_context(|| format!("{name} descriptor platform must be an object"))?;
    let os = platform
        .get("os")
        .and_then(Value::as_str)
        .with_context(|| format!("{name} descriptor platform os must be a string"))?;
    let architecture = platform
        .get("architecture")
        .and_then(Value::as_str)
        .with_context(|| format!("{name} descriptor platform architecture must be a string"))?;
    Ok(Some(DeclaredPlatform {
        os: os.to_owned(),
        architecture: architecture.to_owned(),
    }))
}

fn validate_layout_marker(bytes: &[u8]) -> Result<()> {
    let value: Value =
        serde_json::from_slice(bytes).context("OCI Layout oci-layout is invalid JSON")?;
    if value
        .as_object()
        .and_then(|object| object.get("imageLayoutVersion"))
        .and_then(Value::as_str)
        != Some("1.0.0")
    {
        bail!("OCI Layout imageLayoutVersion must be 1.0.0");
    }
    Ok(())
}

fn verify_descriptor_bytes(bytes: &[u8], descriptor: &OciDescriptor) -> Result<()> {
    let size = u64::try_from(bytes.len()).context("OCI blob size overflow")?;
    if size != descriptor.size {
        bail!(
            "OCI blob size mismatch for {}: expected {}, received {size}",
            descriptor.digest,
            descriptor.size
        );
    }
    let digest = digest_bytes(bytes);
    if digest != descriptor.digest {
        bail!(
            "OCI blob digest mismatch: expected {}, received {digest}",
            descriptor.digest
        );
    }
    Ok(())
}

fn read_bounded(mut reader: impl Read, limit: u64, name: &str) -> Result<Vec<u8>> {
    let capacity = usize::try_from(limit.min(1024 * 1024)).context("input limit overflow")?;
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {name}"))?;
    if u64::try_from(bytes.len()).context("input size overflow")? > limit {
        bail!("{name} exceeds the {limit}-byte limit");
    }
    Ok(bytes)
}

fn blob_member(digest: &Digest) -> Vec<u8> {
    format!("blobs/sha256/{}", digest.hex()).into_bytes()
}

fn validate_hex(value: &[u8]) -> Result<()> {
    if value.len() != 64
        || !value
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        bail!("invalid sha256 OCI blob path");
    }
    Ok(())
}

fn normalize_archive_path(path: &[u8]) -> Result<Vec<u8>> {
    if path.len() > MAX_ARCHIVE_PATH_BYTES {
        bail!("OCI archive member path exceeds the {MAX_ARCHIVE_PATH_BYTES}-byte limit");
    }
    if path.contains(&0) || path.contains(&b'\\') || path.starts_with(b"/") {
        bail!("OCI archive contains an unsafe member path");
    }
    let mut path = path;
    while let Some(stripped) = path.strip_prefix(b"./") {
        path = stripped;
    }
    while let Some(stripped) = path.strip_suffix(b"/") {
        path = stripped;
    }
    if path == b"." || path.is_empty() {
        return Ok(Vec::new());
    }
    if path
        .split(|byte| *byte == b'/')
        .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        bail!("OCI archive contains an unsafe member path");
    }
    Ok(path.to_vec())
}

fn preflight_archive(file: &mut File, archive_size: u64) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .context("failed to rewind OCI archive")?;
    let mut position = 0_u64;
    let mut entries = 0_u64;
    let mut extension_bytes = 0_u64;
    let mut header_bytes = [0_u8; TAR_BLOCK_LENGTH];
    loop {
        let header_end = position
            .checked_add(TAR_BLOCK_BYTES)
            .context("OCI archive position overflow")?;
        if header_end > archive_size {
            bail!("OCI archive ends in a partial tar header");
        }
        file.read_exact(&mut header_bytes)
            .context("failed to read OCI archive header")?;
        if header_bytes.iter().all(|byte| *byte == 0) {
            let second_end = header_end
                .checked_add(TAR_BLOCK_BYTES)
                .context("OCI archive position overflow")?;
            if second_end > archive_size {
                bail!("OCI archive lacks the second zero end marker");
            }
            file.read_exact(&mut header_bytes)
                .context("failed to read OCI archive end marker")?;
            if header_bytes.iter().any(|byte| *byte != 0) {
                bail!("OCI archive has a malformed second end marker");
            }
            let mut trailing = [0_u8; 8192];
            loop {
                let read = file
                    .read(&mut trailing)
                    .context("failed to read OCI archive trailing bytes")?;
                if read == 0 {
                    return Ok(());
                }
                if trailing[..read].iter().any(|byte| *byte != 0) {
                    bail!("OCI archive contains nonzero trailing data");
                }
            }
        }
        entries = entries
            .checked_add(1)
            .context("OCI archive entry count overflow")?;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("OCI archive exceeds the {MAX_ARCHIVE_ENTRIES}-entry limit");
        }
        let size = validate_archive_header(file, &header_bytes, &mut extension_bytes)?;
        let padded_size = size
            .checked_add(TAR_BLOCK_BYTES - 1)
            .context("OCI archive member size overflow")?
            / TAR_BLOCK_BYTES
            * TAR_BLOCK_BYTES;
        position = header_end
            .checked_add(padded_size)
            .context("OCI archive member position overflow")?;
        if position > archive_size {
            bail!("OCI archive member exceeds the source file");
        }
        file.seek(SeekFrom::Start(position))
            .context("failed to seek OCI archive member")?;
    }
}

fn validate_archive_header(
    file: &mut File,
    header_bytes: &[u8; TAR_BLOCK_LENGTH],
    extension_bytes: &mut u64,
) -> Result<u64> {
    let header = Header::from_byte_slice(header_bytes);
    let checksum = header.as_bytes()[..148]
        .iter()
        .chain(&header.as_bytes()[156..])
        .fold(0_u32, |sum, byte| sum + u32::from(*byte))
        + 8 * 32;
    if header
        .cksum()
        .context("OCI archive header checksum is invalid")?
        != checksum
    {
        bail!("OCI archive header checksum mismatch");
    }
    let entry_type = header.entry_type();
    if entry_type.is_gnu_sparse() {
        bail!("GNU sparse OCI archive entries are unsupported");
    }
    if entry_type.is_pax_global_extensions() {
        bail!("global PAX headers are unsupported in OCI archives");
    }
    let size = header
        .entry_size()
        .context("OCI archive header has an invalid entry size")?;
    if entry_type.is_pax_local_extensions() && size > crate::pax::DEFAULT_MAX_PAX_BYTES {
        bail!(
            "OCI archive PAX payload exceeds the {}-byte limit",
            crate::pax::DEFAULT_MAX_PAX_BYTES
        );
    }
    if entry_type.is_pax_local_extensions() {
        let size = usize::try_from(size).context("OCI archive PAX payload size overflow")?;
        let mut payload = vec![0_u8; size];
        file.read_exact(&mut payload)
            .context("failed to read OCI archive PAX payload")?;
        let records = crate::pax::PaxRecords::parse(&payload, crate::pax::DEFAULT_MAX_PAX_BYTES)?;
        if records.contains_key(b"size") {
            bail!("OCI archive PAX size overrides are unsupported");
        }
    }
    if (entry_type.is_gnu_longname() || entry_type.is_gnu_longlink())
        && size > u64::try_from(MAX_ARCHIVE_PATH_BYTES)?
    {
        bail!("OCI archive GNU path payload exceeds the {MAX_ARCHIVE_PATH_BYTES}-byte limit");
    }
    if entry_type.is_pax_local_extensions()
        || entry_type.is_gnu_longname()
        || entry_type.is_gnu_longlink()
    {
        *extension_bytes = extension_bytes
            .checked_add(size)
            .context("OCI archive extension payload budget overflow")?;
        if *extension_bytes > MAX_ARCHIVE_PATH_TOTAL_BYTES {
            bail!(
                "OCI archive extension payloads exceed the {MAX_ARCHIVE_PATH_TOTAL_BYTES}-byte aggregate limit"
            );
        }
    }
    Ok(size)
}
