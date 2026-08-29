use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use oci_spec::image::Descriptor;
use run_engine::{ContentError, ContentErrorKind, OciContent, OciContentStore};
use sha2::{Digest as _, Sha256};

pub(crate) struct LocalOciStore {
    root: PathBuf,
}

impl LocalOciStore {
    pub(crate) fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("blobs/sha256"))
            .with_context(|| format!("failed to create OCI content store {}", root.display()))?;
        Ok(Self { root })
    }

    pub(crate) fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        let encoded = parse_sha256(digest)?;
        Ok(self.root.join("blobs/sha256").join(encoded))
    }

    pub(crate) fn read(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        let path = self.blob_path(descriptor.digest().as_ref())?;
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read OCI content {}", path.display()))?;
        verify_bytes(descriptor, &bytes)?;
        Ok(bytes)
    }

    pub(crate) fn manifest_descriptor(&self, digest: &str) -> Result<Descriptor> {
        let path = self.blob_path(digest)?;
        let size = fs::metadata(&path)
            .with_context(|| format!("OCI Manifest is unavailable: {digest}"))?
            .len();
        let descriptor = serde_json::from_value(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest,
            "size": size,
        }))
        .context("failed to form OCI Descriptor")?;
        let bytes = self.read(&descriptor)?;
        serde_json::from_slice::<oci_spec::image::ImageManifest>(&bytes)
            .context("selected digest is not an OCI Image Manifest")?;
        Ok(descriptor)
    }

    fn publish_inner(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<()> {
        let destination = self.blob_path(descriptor.digest().as_ref())?;
        if destination.exists() {
            let mut existing = File::open(&destination)?;
            verify_reader(descriptor, &mut existing)?;
            return Ok(());
        }
        let parent = destination
            .parent()
            .context("OCI blob destination has no parent")?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        std::io::copy(content, &mut temporary)?;
        temporary.as_file_mut().sync_all()?;
        temporary.as_file_mut().seek(SeekFrom::Start(0))?;
        verify_reader(descriptor, temporary.as_file_mut())?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing = OpenOptions::new().read(true).open(&destination)?;
                verify_reader(descriptor, &mut existing)
            }
            Err(error) => Err(error.error.into()),
        }
    }
}

impl OciContentStore for LocalOciStore {
    fn published_content_is_immutable(&self) -> bool {
        true
    }

    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
        let path = self
            .blob_path(descriptor.digest().as_ref())
            .map_err(|error| content_error(ContentErrorKind::Unavailable, error))?;
        let file = File::open(&path)
            .map_err(|error| content_error(ContentErrorKind::Unavailable, error))?;
        Ok(Box::new(file))
    }

    fn publish(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<(), ContentError> {
        self.publish_inner(descriptor, content)
            .map_err(|error| content_error(ContentErrorKind::Rejected, error))
    }
}

fn parse_sha256(digest: &str) -> Result<&str> {
    let encoded = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported OCI digest algorithm: {digest}"))?;
    if encoded.len() != 64
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid sha256 digest: {digest}");
    }
    Ok(encoded)
}

fn verify_bytes(descriptor: &Descriptor, bytes: &[u8]) -> Result<()> {
    if descriptor.size() != u64::try_from(bytes.len()).context("OCI content is too large")? {
        bail!("OCI content size does not match {}", descriptor.digest());
    }
    let actual = sha256_digest(bytes);
    if actual != descriptor.digest().to_string() {
        bail!("OCI content digest does not match {}", descriptor.digest());
    }
    Ok(())
}

fn verify_reader(descriptor: &Descriptor, reader: &mut (impl Read + Seek)) -> Result<()> {
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(u64::try_from(read).context("OCI content size overflow")?)
            .context("OCI content size overflow")?;
    }
    let expected_size = descriptor.size();
    if size != expected_size {
        bail!("OCI content size does not match {}", descriptor.digest());
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    let actual = format!("sha256:{encoded}");
    if actual != descriptor.digest().to_string() {
        bail!("OCI content digest does not match {}", descriptor.digest());
    }
    Ok(())
}

fn content_error(kind: ContentErrorKind, error: impl std::fmt::Display) -> ContentError {
    ContentError::new(kind, error.to_string())
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}
