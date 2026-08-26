use std::io::{Read, Seek};

use anyhow::{Context, Result, bail};
use tar::Header;

use super::xattr::validate_pax_xattr_name;
use super::{
    RootfsLimits, checked_total, enforce, internal_error, unsupported_input, usize_to_u64,
};

pub(super) struct MaterializationBudget {
    limits: RootfsLimits,
    compressed_bytes: u64,
    uncompressed_bytes: u64,
    entries: u64,
    raw_path_bytes: u64,
    xattr_bytes: u64,
    extension_bytes: u64,
}

impl MaterializationBudget {
    pub(super) fn new(limits: RootfsLimits) -> Self {
        Self {
            limits,
            compressed_bytes: 0,
            uncompressed_bytes: 0,
            entries: 0,
            raw_path_bytes: 0,
            xattr_bytes: 0,
            extension_bytes: 0,
        }
    }

    pub(super) fn compressed(&mut self, bytes: u64) -> Result<()> {
        self.compressed_bytes = checked_total(
            self.compressed_bytes,
            bytes,
            self.limits.total_compressed_bytes,
            "compressed Layer bytes",
        )?;
        Ok(())
    }

    pub(super) fn remaining_uncompressed(&self) -> u64 {
        self.limits
            .total_uncompressed_bytes
            .saturating_sub(self.uncompressed_bytes)
    }

    pub(super) fn uncompressed(&mut self, bytes: u64) -> Result<()> {
        self.uncompressed_bytes = checked_total(
            self.uncompressed_bytes,
            bytes,
            self.limits.total_uncompressed_bytes,
            "uncompressed Layer bytes",
        )?;
        Ok(())
    }

    fn entry(&mut self) -> Result<()> {
        self.entries = checked_total(self.entries, 1, self.limits.entries, "Layer entries")?;
        Ok(())
    }

    fn raw_path_bytes(&mut self, bytes: u64) -> Result<()> {
        self.raw_path_bytes = checked_total(
            self.raw_path_bytes,
            bytes,
            self.limits.total_path_bytes,
            "Layer raw path bytes",
        )?;
        Ok(())
    }

    fn xattr(&mut self, name_bytes: u64, value_bytes: u64) -> Result<()> {
        let added = name_bytes
            .checked_add(value_bytes)
            .ok_or_else(|| unsupported_input("Layer xattr byte count overflow"))?;
        self.xattr_bytes = checked_total(
            self.xattr_bytes,
            added,
            self.limits.total_xattr_bytes,
            "Layer xattr bytes",
        )?;
        Ok(())
    }

    fn extension(&mut self, bytes: u64) -> Result<()> {
        self.extension_bytes = checked_total(
            self.extension_bytes,
            bytes,
            self.limits.extension_bytes,
            "tar extension bytes",
        )?;
        Ok(())
    }
}

/// Performs a fixed-memory physical tar pass before `tar::Archive` can
/// allocate GNU long-name or PAX extension payloads. The counters belong
/// to the whole image, so a later Layer can only consume what remains.
pub(super) fn preflight_decoded_tar<R: Read + Seek>(
    decoded: &mut R,
    budget: &mut MaterializationBudget,
) -> Result<()> {
    decoded.rewind().map_err(internal_error)?;
    loop {
        let mut header = Header::new_old();
        read_tar_block(decoded, header.as_mut_bytes())?;
        if header.as_bytes().iter().all(|byte| *byte == 0) {
            return verify_zero_tar_tail(decoded);
        }
        validate_tar_checksum(&header)?;
        account_raw_header_paths(&header, budget)?;
        let size = header
            .entry_size()
            .context("invalid OCI Layer tar entry size")?;
        let entry_type = header.entry_type();
        if entry_type.is_gnu_sparse() {
            return Err(unsupported_input(
                "GNU sparse OCI Layer entries are unsupported",
            ));
        }
        if entry_type.is_pax_global_extensions() {
            return Err(unsupported_input(
                "OCI Layer PAX global extensions are unsupported",
            ));
        }
        if entry_type.is_pax_local_extensions() {
            budget.extension(size)?;
            preflight_pax_payload(decoded, size, budget)?;
        } else if entry_type.is_gnu_longname() || entry_type.is_gnu_longlink() {
            budget.extension(size)?;
            budget.raw_path_bytes(size)?;
            discard_exact(decoded, size)?;
        } else {
            budget.entry()?;
            enforce("entry bytes", budget.limits.entry_bytes, size)?;
            discard_exact(decoded, size)?;
        }
        discard_tar_padding(decoded, size)?;
    }
}

fn verify_zero_tar_tail(reader: &mut impl Read) -> Result<()> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        if buffer[..read].iter().any(|byte| *byte != 0) {
            bail!("OCI Layer tar contains non-zero data after its end marker");
        }
    }
}

fn read_tar_block(reader: &mut impl Read, block: &mut [u8; 512]) -> Result<()> {
    reader
        .read_exact(block)
        .context("truncated OCI Layer tar header")
}

fn validate_tar_checksum(header: &Header) -> Result<()> {
    let bytes = header.as_bytes();
    let actual = bytes[..148]
        .iter()
        .chain(&bytes[156..])
        .fold(8_u64 * u64::from(b' '), |sum, byte| {
            sum.saturating_add(u64::from(*byte))
        });
    let expected = u64::from(
        header
            .cksum()
            .context("invalid OCI Layer tar checksum field")?,
    );
    if actual != expected {
        bail!("OCI Layer tar header checksum mismatch");
    }
    Ok(())
}

fn account_raw_header_paths(header: &Header, budget: &mut MaterializationBudget) -> Result<()> {
    let bytes = header.as_bytes();
    budget.raw_path_bytes(usize_to_u64(nul_terminated_len(&bytes[..100])))?;
    budget.raw_path_bytes(usize_to_u64(nul_terminated_len(&bytes[157..257])))?;
    if header.as_ustar().is_some() {
        budget.raw_path_bytes(usize_to_u64(nul_terminated_len(&bytes[345..500])))?;
    }
    Ok(())
}

fn nul_terminated_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len())
}

fn preflight_pax_payload(
    reader: &mut impl Read,
    payload_size: u64,
    budget: &mut MaterializationBudget,
) -> Result<()> {
    let mut remaining = payload_size;
    while remaining != 0 {
        let mut record_length = 0_u64;
        let mut prefix_bytes = 0_u64;
        loop {
            let byte = read_tar_byte(reader)?;
            remaining = remaining
                .checked_sub(1)
                .context("PAX record length exceeds extension payload")?;
            prefix_bytes += 1;
            if byte == b' ' {
                break;
            }
            if !byte.is_ascii_digit() || prefix_bytes > 20 {
                bail!("invalid OCI Layer PAX record length");
            }
            record_length = record_length
                .checked_mul(10)
                .and_then(|length| length.checked_add(u64::from(byte - b'0')))
                .context("OCI Layer PAX record length overflow")?;
        }
        if record_length <= prefix_bytes || record_length - prefix_bytes > remaining {
            bail!("invalid OCI Layer PAX record boundary");
        }
        let body_bytes = record_length - prefix_bytes;
        let mut key = [0_u8; 300];
        let mut key_bytes = 0_u64;
        let mut key_stored = 0_usize;
        loop {
            if key_bytes + 2 > body_bytes {
                bail!("invalid OCI Layer PAX record");
            }
            let byte = read_tar_byte(reader)?;
            remaining -= 1;
            if byte == b'=' {
                break;
            }
            if key_stored < key.len() {
                key[key_stored] = byte;
                key_stored += 1;
            }
            key_bytes += 1;
        }
        let value_bytes = body_bytes - key_bytes - 2;
        let complete_key = key_bytes == usize_to_u64(key_stored);
        if complete_key && (key[..key_stored] == *b"path" || key[..key_stored] == *b"linkpath") {
            budget.raw_path_bytes(value_bytes)?;
        } else if complete_key {
            let key = &key[..key_stored];
            if key == b"size" {
                // tar::Archive uses this value to override the following
                // header's physical payload size. Rejecting it keeps this
                // pass and the high-level parser on identical offsets.
                return Err(unsupported_input(
                    "OCI Layer PAX size overrides are unsupported",
                ));
            }
            if key.starts_with(b"GNU.sparse.") {
                return Err(unsupported_input(
                    "GNU sparse OCI Layer PAX metadata is unsupported",
                ));
            }
            if let Some(name) = key
                .strip_prefix(b"SCHILY.xattr.")
                .or_else(|| key.strip_prefix(b"LIBARCHIVE.xattr."))
            {
                validate_pax_xattr_name(name)?;
                budget.xattr(usize_to_u64(name.len()), value_bytes)?;
            }
        }
        discard_exact(reader, value_bytes)?;
        remaining -= value_bytes;
        if read_tar_byte(reader)? != b'\n' {
            bail!("invalid OCI Layer PAX record terminator");
        }
        remaining -= 1;
    }
    Ok(())
}

fn read_tar_byte(reader: &mut impl Read) -> Result<u8> {
    let mut byte = [0_u8; 1];
    reader
        .read_exact(&mut byte)
        .context("truncated OCI Layer tar extension")?;
    Ok(byte[0])
}

fn discard_exact(reader: &mut impl Read, mut bytes: u64) -> Result<()> {
    let mut buffer = [0_u8; 8 * 1024];
    while bytes != 0 {
        let requested = usize::try_from(bytes.min(usize_to_u64(buffer.len())))?;
        reader
            .read_exact(&mut buffer[..requested])
            .context("truncated OCI Layer tar entry")?;
        bytes -= usize_to_u64(requested);
    }
    Ok(())
}

fn discard_tar_padding(reader: &mut impl Read, size: u64) -> Result<()> {
    let padding = (512 - size % 512) % 512;
    discard_exact(reader, padding)
}
