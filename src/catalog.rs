//! The local Image reference grammar and the catalog that maps references to
//! OCI Manifests.
//!
//! A reference is `name:tag`; the catalog records which Manifest a reference
//! resolves to along with the metadata a person needs to recognize it. The
//! catalog is an index over the OCI Layout, never a second copy of it: removing
//! a reference never removes content.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Serialize;

use crate::core::{Digest, OciDescriptor, Platform};
use crate::oci::{ManifestReference, OciLayout};

const DESCRIPTION_ANNOTATION: &str = "io.runlab.catalog.description";
const SOURCE_ANNOTATION: &str = "io.runlab.catalog.source";
const MAINTAINER_ANNOTATION: &str = "io.runlab.catalog.maintainer";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct CatalogMetadata {
    pub(crate) description: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) maintainer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct CatalogEntry {
    pub(crate) reference: String,
    pub(crate) manifest: OciDescriptor,
    pub(crate) platform: Option<Platform>,
    pub(crate) metadata: CatalogMetadata,
}

pub(crate) struct CatalogUpdate {
    pub(crate) previous: Option<CatalogEntry>,
    pub(crate) entry: CatalogEntry,
    pub(crate) changed: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum CatalogDescriptionUpdate<'a> {
    Preserve,
    Set(&'a str),
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageSelector {
    Digest(Digest),
    Reference(String),
}

impl FromStr for ImageSelector {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.starts_with("sha256:") {
            return Ok(Self::Digest(Digest::parse(value)?));
        }
        Ok(Self::Reference(normalize_reference(value)?))
    }
}

pub(crate) struct LocalImageCatalog<'layout> {
    layout: &'layout OciLayout,
}

impl<'layout> LocalImageCatalog<'layout> {
    pub(crate) const fn new(layout: &'layout OciLayout) -> Self {
        Self { layout }
    }

    pub(crate) fn resolve(&self, reference: &str) -> Result<Option<CatalogEntry>> {
        let reference = normalize_reference(reference)?;
        let mut matching = self
            .layout
            .manifest_references()?
            .into_iter()
            .filter(|entry| entry.reference == reference);
        let Some(reference) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            bail!("duplicate local OCI reference: {}", reference.reference);
        }
        self.layout.verify_manifest_content(&reference.descriptor)?;
        Ok(Some(catalog_entry(reference)?))
    }

    pub(crate) fn list(&self) -> Result<Vec<CatalogEntry>> {
        let mut seen = BTreeSet::new();
        let mut entries = self
            .layout
            .manifest_references()?
            .into_iter()
            .map(|entry| {
                if !seen.insert(entry.reference.clone()) {
                    bail!("duplicate local OCI reference: {}", entry.reference);
                }
                catalog_entry(entry)
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort_by(|left, right| left.reference.cmp(&right.reference));
        Ok(entries)
    }

    pub(crate) fn upsert(
        &self,
        reference: &str,
        manifest: &OciDescriptor,
        platform: Platform,
        metadata: &CatalogMetadata,
    ) -> Result<()> {
        let reference = normalize_reference(reference)?;
        let updates = BTreeMap::from([
            (DESCRIPTION_ANNOTATION, metadata.description.as_deref()),
            (SOURCE_ANNOTATION, metadata.source.as_deref()),
            (MAINTAINER_ANNOTATION, metadata.maintainer.as_deref()),
        ]);
        self.layout
            .upsert_manifest_reference(manifest, Some(platform), &reference, &updates)?;
        Ok(())
    }

    pub(crate) fn set(
        &self,
        reference: &str,
        manifest: &OciDescriptor,
        platform: Platform,
        description: CatalogDescriptionUpdate<'_>,
    ) -> Result<CatalogUpdate> {
        let reference = normalize_reference(reference)?;
        let update = self.layout.update_manifest_reference(
            manifest,
            Some(platform),
            &reference,
            |previous| {
                let existing = previous.cloned().map(catalog_entry).transpose()?;
                let preserves_provenance = existing
                    .as_ref()
                    .is_some_and(|entry| entry.manifest == *manifest);
                let metadata = CatalogMetadata {
                    description: match description {
                        CatalogDescriptionUpdate::Preserve => existing
                            .as_ref()
                            .and_then(|entry| entry.metadata.description.clone()),
                        CatalogDescriptionUpdate::Set(description) => Some(description.to_owned()),
                        CatalogDescriptionUpdate::Clear => None,
                    },
                    source: preserves_provenance
                        .then(|| {
                            existing
                                .as_ref()
                                .and_then(|entry| entry.metadata.source.clone())
                        })
                        .flatten()
                        .or_else(|| Some(format!("local@{}", manifest.digest))),
                    maintainer: preserves_provenance
                        .then(|| {
                            existing
                                .as_ref()
                                .and_then(|entry| entry.metadata.maintainer.clone())
                        })
                        .flatten()
                        .or_else(|| Some("local".to_owned())),
                };
                Ok(BTreeMap::from([
                    (DESCRIPTION_ANNOTATION.to_owned(), metadata.description),
                    (SOURCE_ANNOTATION.to_owned(), metadata.source),
                    (MAINTAINER_ANNOTATION.to_owned(), metadata.maintainer),
                ]))
            },
        )?;
        let previous = update.previous.map(catalog_entry).transpose()?;
        let entry = catalog_entry(update.current)?;
        Ok(CatalogUpdate {
            previous,
            entry,
            changed: update.changed,
        })
    }

    pub(crate) fn remove(&self, reference: &str) -> Result<Option<CatalogEntry>> {
        self.layout
            .remove_manifest_reference(&normalize_reference(reference)?)
            .and_then(|removed| removed.map(catalog_entry).transpose())
    }
}

pub(crate) fn normalize_reference(value: &str) -> Result<String> {
    if value.is_empty()
        || value.contains(char::is_whitespace)
        || value.contains(['@', '?', '#'])
        || value.contains("://")
    {
        bail!("invalid local OCI reference: {value}");
    }
    let (name, tag) = split_name_and_tag(value)?;
    validate_name(name, value)?;
    validate_tag(tag, value)?;
    Ok(format!("{name}:{tag}"))
}

fn split_name_and_tag(value: &str) -> Result<(&str, &str)> {
    let slash = value.rfind('/');
    let colon = value.rfind(':');
    match colon {
        Some(colon) if slash.is_none_or(|slash| colon > slash) => {
            let (name, tag) = value.split_at(colon);
            Ok((name, &tag[1..]))
        }
        Some(_) => bail!("local OCI reference must not contain a registry port: {value}"),
        None => Ok((value, "latest")),
    }
}

fn validate_name(name: &str, original: &str) -> Result<()> {
    let mut components = name.split('/');
    let Some(first) = components.next() else {
        bail!("invalid local OCI reference: {original}");
    };
    if first == "localhost" || first.contains('.') || first.contains(':') {
        bail!("local OCI reference must not contain a registry host: {original}");
    }
    if !valid_name_component(first) || !components.all(valid_name_component) {
        bail!("invalid local OCI reference: {original}");
    }
    Ok(())
}

fn valid_name_component(component: &str) -> bool {
    let bytes = component.as_bytes();
    !bytes.is_empty()
        && is_lowercase_or_digit(bytes[0])
        && is_lowercase_or_digit(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|byte| is_lowercase_or_digit(*byte) || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_lowercase_or_digit(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn validate_tag(tag: &str, original: &str) -> Result<()> {
    let bytes = tag.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 128
        || !(bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        bail!("invalid local OCI reference: {original}");
    }
    Ok(())
}

fn catalog_entry(reference: ManifestReference) -> Result<CatalogEntry> {
    let normalized = normalize_reference(&reference.reference)?;
    if normalized != reference.reference {
        bail!(
            "local OCI reference is not normalized: {}",
            reference.reference
        );
    }
    Ok(CatalogEntry {
        metadata: CatalogMetadata {
            description: annotation(&reference, DESCRIPTION_ANNOTATION),
            source: annotation(&reference, SOURCE_ANNOTATION),
            maintainer: annotation(&reference, MAINTAINER_ANNOTATION),
        },
        reference: reference.reference,
        manifest: reference.descriptor,
        platform: reference.platform,
    })
}

fn annotation(reference: &ManifestReference, key: &str) -> Option<String> {
    reference.annotations.get(key).cloned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use serde_json::json;

    use super::*;
    use crate::core::{Architecture, OCI_IMAGE_CONFIG, OCI_IMAGE_MANIFEST};
    use crate::integrity::canonical_json;

    #[test]
    fn selector_distinguishes_exact_digests_from_normalized_local_references() {
        let digest = format!("sha256:{}", "1".repeat(64));
        assert_eq!(
            digest.parse::<ImageSelector>().expect("digest selector"),
            ImageSelector::Digest(Digest::parse(&digest).expect("digest"))
        );
        assert_eq!(
            "runlab/agent"
                .parse::<ImageSelector>()
                .expect("reference selector"),
            ImageSelector::Reference("runlab/agent:latest".to_owned())
        );
        assert_eq!(
            "runlab/agent:3.14"
                .parse::<ImageSelector>()
                .expect("tagged selector"),
            ImageSelector::Reference("runlab/agent:3.14".to_owned())
        );
    }

    #[test]
    fn selector_rejects_remote_and_ambiguous_references() {
        for invalid in [
            "registry.example/team/agent:latest",
            "localhost/team/agent:latest",
            "registry:5000/team/agent:latest",
            "team/agent@sha256:deadbeef",
            "Team/agent:latest",
            "team//agent:latest",
            "team/agent:",
            "sha256:deadbeef",
        ] {
            assert!(
                invalid.parse::<ImageSelector>().is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn untagged_catalog_operations_use_latest() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let catalog = LocalImageCatalog::new(&layout);
        let manifest = manifest(&layout, "latest");
        catalog
            .upsert(
                "runlab/agent",
                &manifest,
                platform(),
                &CatalogMetadata::default(),
            )
            .expect("upsert");

        let entry = catalog
            .resolve("runlab/agent")
            .expect("resolve")
            .expect("entry");
        assert_eq!(entry.reference, "runlab/agent:latest");
        assert!(catalog.remove("runlab/agent").expect("remove").is_some());
    }

    #[test]
    fn tag_move_preserves_aliases_and_catalog_metadata() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let catalog = LocalImageCatalog::new(&layout);
        let first = manifest(&layout, "first");
        let second = manifest(&layout, "second");
        let first_bytes = layout.get_descriptor_bytes(&first).expect("first Manifest");
        catalog
            .upsert(
                "runlab/agent:latest",
                &first,
                platform(),
                &CatalogMetadata {
                    description: Some("first description".to_owned()),
                    source: Some("registry.example/agent@sha256:first".to_owned()),
                    maintainer: Some("local".to_owned()),
                },
            )
            .expect("latest");
        catalog
            .upsert(
                "runlab/agent:stable",
                &first,
                platform(),
                &CatalogMetadata {
                    description: Some("stable alias".to_owned()),
                    ..CatalogMetadata::default()
                },
            )
            .expect("alias");
        catalog
            .upsert(
                "runlab/agent:latest",
                &second,
                platform(),
                &CatalogMetadata {
                    description: Some("second description".to_owned()),
                    maintainer: Some("official".to_owned()),
                    ..CatalogMetadata::default()
                },
            )
            .expect("tag move");

        let entries = catalog.list().expect("list");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.reference.as_str())
                .collect::<Vec<_>>(),
            ["runlab/agent:latest", "runlab/agent:stable"]
        );
        assert_eq!(entries[0].manifest, second);
        assert_eq!(entries[0].platform, Some(platform()));
        assert_eq!(
            entries[0].metadata,
            CatalogMetadata {
                description: Some("second description".to_owned()),
                source: None,
                maintainer: Some("official".to_owned()),
            }
        );
        assert_eq!(entries[1].manifest, first);
        assert_eq!(
            entries[1].metadata.description.as_deref(),
            Some("stable alias")
        );
        assert_eq!(
            layout
                .get_descriptor_bytes(&first)
                .expect("first Manifest after metadata updates"),
            first_bytes
        );

        assert!(
            catalog
                .remove("runlab/agent:latest")
                .expect("remove")
                .is_some()
        );
        assert!(
            catalog
                .remove("runlab/agent:latest")
                .expect("missing")
                .is_none()
        );
        assert_eq!(
            catalog
                .resolve("runlab/agent:stable")
                .expect("resolve alias")
                .expect("alias")
                .manifest,
            first
        );
        assert_eq!(
            layout
                .get_descriptor_bytes(&second)
                .expect("unreferenced Manifest remains published"),
            canonical_json(&manifest_json("second")).expect("Manifest bytes")
        );
    }

    #[test]
    fn resolve_rejects_missing_and_corrupt_manifest_content() {
        let missing_state = tempfile::tempdir().expect("missing state");
        let missing_layout = OciLayout::open(missing_state.path()).expect("missing layout");
        let missing_catalog = LocalImageCatalog::new(&missing_layout);
        let missing = manifest(&missing_layout, "missing");
        missing_catalog
            .upsert(
                "runlab/missing:latest",
                &missing,
                platform(),
                &CatalogMetadata::default(),
            )
            .expect("missing reference");
        fs::remove_file(
            missing_layout
                .get_descriptor_path(&missing)
                .expect("missing target path"),
        )
        .expect("remove target");
        assert!(
            missing_catalog
                .resolve("runlab/missing:latest")
                .expect_err("missing target")
                .to_string()
                .contains("unavailable")
        );

        let corrupt_state = tempfile::tempdir().expect("corrupt state");
        let corrupt_layout = OciLayout::open(corrupt_state.path()).expect("corrupt layout");
        let corrupt_catalog = LocalImageCatalog::new(&corrupt_layout);
        let corrupt = manifest(&corrupt_layout, "corrupt");
        corrupt_catalog
            .upsert(
                "runlab/corrupt:latest",
                &corrupt,
                platform(),
                &CatalogMetadata::default(),
            )
            .expect("corrupt reference");
        fs::write(
            corrupt_layout
                .get_descriptor_path(&corrupt)
                .expect("corrupt target path"),
            b"corrupt",
        )
        .expect("corrupt target");
        assert!(
            corrupt_catalog
                .resolve("runlab/corrupt:latest")
                .expect_err("corrupt target")
                .to_string()
                .contains("failed digest verification")
        );
    }

    #[test]
    fn index_failure_does_not_remove_published_manifest() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let catalog = LocalImageCatalog::new(&layout);
        let manifest = manifest(&layout, "published");
        let expected = layout
            .get_descriptor_bytes(&manifest)
            .expect("published Manifest");
        fs::write(state.path().join("index.json"), b"{").expect("corrupt index");
        assert!(
            catalog
                .upsert(
                    "runlab/published:latest",
                    &manifest,
                    platform(),
                    &CatalogMetadata::default(),
                )
                .expect_err("index failure")
                .to_string()
                .contains("index.json is invalid")
        );
        assert_eq!(
            layout
                .get_descriptor_bytes(&manifest)
                .expect("published Manifest survives"),
            expected
        );
    }

    #[test]
    fn concurrent_upserts_preserve_every_reference() {
        let state = tempfile::tempdir().expect("state");
        let layout = Arc::new(OciLayout::open(state.path()).expect("layout"));
        let manifest = manifest(&layout, "concurrent");
        let barrier = Arc::new(Barrier::new(8));
        let threads = (0..8)
            .map(|index| {
                let layout = Arc::clone(&layout);
                let manifest = manifest.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    LocalImageCatalog::new(&layout)
                        .upsert(
                            &format!("runlab/concurrent:{index}"),
                            &manifest,
                            platform(),
                            &CatalogMetadata::default(),
                        )
                        .expect("concurrent upsert");
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("upsert thread");
        }
        assert_eq!(
            LocalImageCatalog::new(&layout).list().expect("list").len(),
            8
        );
    }

    fn manifest(layout: &OciLayout, marker: &str) -> OciDescriptor {
        layout
            .put_bytes(
                &canonical_json(&manifest_json(marker)).expect("Manifest bytes"),
                OCI_IMAGE_MANIFEST,
            )
            .expect("Manifest")
    }

    fn manifest_json(marker: &str) -> serde_json::Value {
        json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_MANIFEST,
            "config": {
                "mediaType": OCI_IMAGE_CONFIG,
                "digest": format!("sha256:{}", "0".repeat(64)),
                "size": 0
            },
            "layers": [],
            "annotations": {"test.marker": marker}
        })
    }

    const fn platform() -> Platform {
        Platform::linux(Architecture::Arm64)
    }
}
