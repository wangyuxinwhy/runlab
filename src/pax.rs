use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use tar::{Builder, EntryType, Header};
use thiserror::Error;

use crate::filesystem::{Timestamp, Xattrs};

pub(crate) const DEFAULT_MAX_PAX_BYTES: u64 = 1024 * 1024;
const BLOCK_BYTES: u64 = 512;
const SCHILY_XATTR: &[u8] = b"SCHILY.xattr.";
const LIBARCHIVE_XATTR: &[u8] = b"LIBARCHIVE.xattr.";
const MAX_LINUX_XATTR_NAME_BYTES: usize = 255;

#[derive(Debug, Error)]
pub(crate) enum PaxError {
    #[error("tar entry count exceeds limit {limit}: observed {observed}")]
    EntryLimit { limit: u64, observed: u64 },
    #[error("tar PAX index bytes exceed limit {limit}: observed {observed}")]
    IndexBytesLimit { limit: u64, observed: u64 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PaxRecords(BTreeMap<Box<[u8]>, Box<[u8]>>);

impl PaxRecords {
    pub(crate) fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        validate_key(key)?;
        self.0.insert(key.into(), value.into());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.0.get(key).map(AsRef::as_ref)
    }

    pub(crate) fn contains_key(&self, key: &[u8]) -> bool {
        self.0.contains_key(key)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn mtime(&self, fallback: u64) -> Result<Timestamp> {
        let Some(value) = self.0.get(b"mtime".as_slice()) else {
            return Ok(Timestamp {
                seconds: i64::try_from(fallback).context("tar mtime exceeds i64")?,
                nanos: 0,
            });
        };
        parse_timestamp(value)
    }

    fn entry_size(&self, fallback: u64) -> Result<u64> {
        let Some(bytes) = self.0.get(b"size".as_slice()) else {
            return Ok(fallback);
        };
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
            bail!("PAX size is invalid");
        }
        std::str::from_utf8(bytes)?
            .parse::<u64>()
            .context("PAX size is invalid")
    }

    pub(crate) fn encode(&self, limit: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for (key, value) in &self.0 {
            let body = key
                .len()
                .checked_add(value.len())
                .and_then(|size| size.checked_add(3))
                .context("PAX record size overflow")?;
            let mut length = body
                .checked_add(decimal_digits(body))
                .context("PAX record size overflow")?;
            loop {
                let adjusted = body
                    .checked_add(decimal_digits(length))
                    .context("PAX record size overflow")?;
                if adjusted == length {
                    break;
                }
                length = adjusted;
            }
            let observed = bytes
                .len()
                .checked_add(length)
                .context("PAX payload size overflow")?;
            if u64::try_from(observed)? > limit {
                bail!("PAX payload exceeds limit {limit}: observed {observed}");
            }
            bytes.extend_from_slice(length.to_string().as_bytes());
            bytes.push(b' ');
            bytes.extend_from_slice(key);
            bytes.push(b'=');
            bytes.extend_from_slice(value);
            bytes.push(b'\n');
        }
        Ok(bytes)
    }

    pub(crate) fn parse(bytes: &[u8], limit: u64) -> Result<Self> {
        if u64::try_from(bytes.len())? > limit {
            bail!(
                "PAX payload exceeds limit {limit}: observed {}",
                bytes.len()
            );
        }
        let mut records = Self::default();
        let mut offset = 0;
        while offset < bytes.len() {
            let relative_space = bytes[offset..]
                .iter()
                .position(|byte| *byte == b' ')
                .context("PAX record lacks a length separator")?;
            let space = offset + relative_space;
            let length_bytes = &bytes[offset..space];
            if length_bytes.is_empty() || !length_bytes.iter().all(u8::is_ascii_digit) {
                bail!("PAX record length is invalid");
            }
            let length = std::str::from_utf8(length_bytes)?
                .parse::<usize>()
                .context("PAX record length is invalid")?;
            let end = offset
                .checked_add(length)
                .context("PAX record length overflow")?;
            let body_start = space.checked_add(1).context("PAX record offset overflow")?;
            let Some(payload_end) = end.checked_sub(1) else {
                bail!("PAX record length does not match its payload");
            };
            if payload_end < body_start || end > bytes.len() || bytes[payload_end] != b'\n' {
                bail!("PAX record length does not match its payload");
            }
            let body = &bytes[body_start..payload_end];
            let equals = body
                .iter()
                .position(|byte| *byte == b'=')
                .context("PAX record lacks a key/value separator")?;
            let key = &body[..equals];
            let value = &body[equals + 1..];
            validate_key(key)?;
            records.0.insert(key.into(), value.into());
            offset = end;
        }
        Ok(records)
    }
}

fn parse_timestamp(value: &[u8]) -> Result<Timestamp> {
    let value = std::str::from_utf8(value).context("PAX mtime is not ASCII")?;
    let (negative, unsigned) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || parts.next().is_some()
        || fraction.is_some_and(|digits| !digits.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("PAX mtime is invalid");
    }
    let whole = whole
        .parse::<i128>()
        .context("PAX mtime seconds overflow")?;
    let fraction = fraction.unwrap_or_default();
    if fraction.len() > 9 && fraction.as_bytes()[9..].iter().any(|byte| *byte != b'0') {
        bail!("PAX mtime exceeds nanosecond precision");
    }
    let significant = &fraction[..fraction.len().min(9)];
    let mut nanos = if significant.is_empty() {
        0
    } else {
        significant
            .parse::<i128>()
            .context("PAX mtime fraction overflow")?
    };
    for _ in significant.len()..9 {
        nanos = nanos.checked_mul(10).context("PAX mtime overflow")?;
    }
    let total = whole
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .context("PAX mtime overflow")?;
    let total = if negative {
        total.checked_neg().context("PAX mtime overflow")?
    } else {
        total
    };
    Ok(Timestamp {
        seconds: i64::try_from(total.div_euclid(1_000_000_000))
            .context("PAX mtime seconds overflow")?,
        nanos: u32::try_from(total.rem_euclid(1_000_000_000))
            .context("PAX mtime nanoseconds overflow")?,
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TarPaxLimits {
    pub(crate) entries: u64,
    pub(crate) total_bytes: u64,
    pub(crate) pax_bytes: u64,
    pub(crate) index_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct TarPaxIndex {
    len: usize,
    records: BTreeMap<u64, PaxRecords>,
}

impl TarPaxIndex {
    pub(crate) fn get(&self, ordinal: u64) -> Result<Option<&PaxRecords>> {
        let ordinal = usize::try_from(ordinal).context("tar entry ordinal overflow")?;
        if ordinal >= self.len {
            bail!("tar PAX index is missing an entry");
        }
        Ok(self.records.get(&u64::try_from(ordinal)?))
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

pub(crate) fn scan_tar(reader: impl Read, limits: TarPaxLimits) -> Result<TarPaxIndex> {
    let mut reader = LimitedReader::new(reader, limits.total_bytes);
    let mut len = 0_usize;
    let mut records = BTreeMap::new();
    let mut index_bytes = 0_u64;
    let mut pending = None;
    loop {
        let Some(block) = read_block(&mut reader)? else {
            break;
        };
        if block.iter().all(|byte| *byte == 0) {
            break;
        }
        let header = Header::from_byte_slice(&block);
        let entry_type = header.entry_type();
        if entry_type == EntryType::GNUSparse {
            bail!("GNU sparse tar entries are unsupported");
        }
        if entry_type.is_pax_global_extensions() {
            bail!("global PAX headers are unsupported");
        }
        let size = header
            .entry_size()
            .context("tar header has an invalid entry size")?;
        let recognized = header.as_gnu().is_some() || header.as_ustar().is_some();
        if recognized && entry_type.is_pax_local_extensions() {
            if pending.is_some() {
                bail!("two PAX headers describe the same tar entry");
            }
            if size > limits.pax_bytes {
                bail!(
                    "PAX payload exceeds limit {}: observed {size}",
                    limits.pax_bytes
                );
            }
            index_bytes = index_bytes
                .checked_add(size)
                .context("PAX index byte count overflow")?;
            if index_bytes > limits.index_bytes {
                return Err(PaxError::IndexBytesLimit {
                    limit: limits.index_bytes,
                    observed: index_bytes,
                }
                .into());
            }
            let size = usize::try_from(size).context("PAX payload size overflow")?;
            let mut payload = vec![0_u8; size];
            reader.read_exact(&mut payload)?;
            discard_padding(&mut reader, u64::try_from(size)?)?;
            pending = Some(PaxRecords::parse(&payload, limits.pax_bytes)?);
            continue;
        }
        if recognized && (entry_type.is_gnu_longname() || entry_type.is_gnu_longlink()) {
            discard_entry(&mut reader, size)?;
            continue;
        }
        let observed = u64::try_from(len)?
            .checked_add(1)
            .context("tar entry count overflow")?;
        if observed > limits.entries {
            return Err(PaxError::EntryLimit {
                limit: limits.entries,
                observed,
            }
            .into());
        }
        let entry_records = pending.take().unwrap_or_default();
        let size = entry_records.entry_size(size)?;
        if !entry_records.is_empty() {
            records.insert(u64::try_from(len)?, entry_records);
        }
        len = len.checked_add(1).context("tar entry count overflow")?;
        discard_entry(&mut reader, size)?;
    }
    if pending.is_some() {
        bail!("PAX header does not describe a following tar entry");
    }
    Ok(TarPaxIndex { len, records })
}

pub(crate) fn append_header<W: Write>(
    builder: &mut Builder<W>,
    records: &PaxRecords,
    limit: u64,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let payload = records.encode(limit)?;
    let mut header = Header::new_ustar();
    header.set_size(u64::try_from(payload.len())?);
    header.set_mode(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(EntryType::XHeader);
    header.set_username("")?;
    header.set_groupname("")?;
    header.set_cksum();
    builder
        .append(&header, payload.as_slice())
        .context("failed to write PAX header")
}

pub(crate) fn insert_xattrs(records: &mut PaxRecords, xattrs: &Xattrs) -> Result<()> {
    for (name, value) in xattrs {
        validate_xattr_name(name)?;
        let encoded_name = encode_xattr_name(name);
        let mut schily = SCHILY_XATTR.to_vec();
        schily.extend_from_slice(&encoded_name);
        records.insert(&schily, value)?;
        let mut libarchive = LIBARCHIVE_XATTR.to_vec();
        libarchive.extend_from_slice(&encoded_name);
        records.insert(&libarchive, base64_encode(value).as_bytes())?;
    }
    Ok(())
}

pub(crate) fn decode_xattrs(records: &PaxRecords) -> Result<Xattrs> {
    let mut schily = BTreeMap::new();
    let mut libarchive = BTreeMap::new();
    for (key, value) in &records.0 {
        let (destination, suffix, decoded_value) =
            if let Some(suffix) = key.strip_prefix(SCHILY_XATTR) {
                (&mut schily, suffix, value.to_vec())
            } else if let Some(suffix) = key.strip_prefix(LIBARCHIVE_XATTR) {
                (&mut libarchive, suffix, base64_decode(value)?)
            } else {
                continue;
            };
        let name = decode_xattr_name(suffix)?;
        validate_xattr_name(&name)?;
        if destination.insert(name.clone(), decoded_value).is_some() {
            bail!("duplicate PAX xattr name: {}", display_bytes(&name));
        }
    }
    let names = schily
        .keys()
        .chain(libarchive.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut xattrs = Xattrs::new();
    for name in names {
        let value = match (schily.get(&name), libarchive.get(&name)) {
            (Some(left), Some(right)) if left != right => {
                bail!("conflicting PAX xattr values: {}", display_bytes(&name));
            }
            (Some(value), _) | (_, Some(value)) => value,
            (None, None) => unreachable!("name came from one xattr convention"),
        };
        xattrs.insert(name.into_boxed_slice(), value.clone().into_boxed_slice());
    }
    Ok(xattrs)
}

fn validate_key(key: &[u8]) -> Result<()> {
    if key.is_empty()
        || key
            .iter()
            .any(|byte| !(33..=126).contains(byte) || *byte == b'=')
    {
        bail!("PAX key is invalid: {}", display_bytes(key));
    }
    Ok(())
}

fn validate_xattr_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.len() > MAX_LINUX_XATTR_NAME_BYTES || name.contains(&0) {
        bail!("Linux xattr name is invalid: {}", display_bytes(name));
    }
    Ok(())
}

fn encode_xattr_name(name: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for byte in name {
        if (33..=126).contains(byte) && *byte != b'%' && *byte != b'=' {
            encoded.push(*byte);
        } else {
            encoded.push(b'%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn decode_xattr_name(encoded: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut offset = 0;
    while offset < encoded.len() {
        if encoded[offset] != b'%' {
            decoded.push(encoded[offset]);
            offset += 1;
            continue;
        }
        let pair = encoded
            .get(offset + 1..offset + 3)
            .context("PAX xattr name has an incomplete percent escape")?;
        decoded.push((hex_value(pair[0])? << 4) | hex_value(pair[1])?);
        offset += 3;
    }
    Ok(decoded)
}

fn base64_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(DIGITS[usize::from(first >> 2)]);
        let second = chunk.get(1).copied();
        encoded.push(DIGITS[usize::from((first & 0x03) << 4 | second.unwrap_or(0) >> 4)]);
        if let Some(second) = second {
            let third = chunk.get(2).copied();
            encoded.push(DIGITS[usize::from((second & 0x0f) << 2 | third.unwrap_or(0) >> 6)]);
            if let Some(third) = third {
                encoded.push(DIGITS[usize::from(third & 0x3f)]);
            }
        }
    }
    String::from_utf8(encoded).expect("base64 alphabet is UTF-8")
}

fn base64_decode(encoded: &[u8]) -> Result<Vec<u8>> {
    let padding = encoded
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 || encoded[..encoded.len() - padding].contains(&b'=') {
        bail!("LIBARCHIVE xattr contains invalid base64 padding");
    }
    if padding != 0 && !encoded.len().is_multiple_of(4) {
        bail!("LIBARCHIVE xattr has invalid padded base64 length");
    }
    let encoded = &encoded[..encoded.len() - padding];
    if encoded.len() % 4 == 1 {
        bail!("LIBARCHIVE xattr has invalid base64 length");
    }
    if padding != 0 && padding != 4 - encoded.len() % 4 {
        bail!("LIBARCHIVE xattr has invalid base64 padding");
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3 + 2);
    for chunk in encoded.chunks(4) {
        let values = chunk
            .iter()
            .map(|byte| base64_value(*byte))
            .collect::<Result<Vec<_>>>()?;
        if values.len() == 2 && values[1] & 0x0f != 0 || values.len() == 3 && values[2] & 0x03 != 0
        {
            bail!("LIBARCHIVE xattr has non-canonical base64 tail bits");
        }
        decoded.push(values[0] << 2 | values[1] >> 4);
        if values.len() > 2 {
            decoded.push(values[1] << 4 | values[2] >> 2);
        }
        if values.len() > 3 {
            decoded.push(values[2] << 6 | values[3]);
        }
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => bail!("LIBARCHIVE xattr contains invalid base64"),
    }
}

fn read_block(reader: &mut impl Read) -> Result<Option<[u8; 512]>> {
    let mut block = [0_u8; 512];
    let mut read = 0;
    while read < block.len() {
        let count = reader.read(&mut block[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(None);
            }
            bail!("tar archive ends within a header block");
        }
        read += count;
    }
    Ok(Some(block))
}

fn discard_entry(reader: &mut impl Read, size: u64) -> Result<()> {
    discard(reader, size)?;
    discard_padding(reader, size)
}

fn discard_padding(reader: &mut impl Read, size: u64) -> Result<()> {
    discard(reader, (BLOCK_BYTES - size % BLOCK_BYTES) % BLOCK_BYTES)
}

fn discard(reader: &mut impl Read, mut remaining: u64) -> Result<()> {
    let mut buffer = [0_u8; 8192];
    while remaining > 0 {
        let buffer_len = u64::try_from(buffer.len())?;
        let take = usize::try_from(remaining.min(buffer_len))?;
        reader.read_exact(&mut buffer[..take])?;
        remaining -= u64::try_from(take)?;
    }
    Ok(())
}

fn decimal_digits(value: usize) -> usize {
    value.to_string().len()
}

const fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'A' + value - 10,
    }
}

fn hex_value(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => bail!("PAX xattr name contains an invalid percent escape"),
    }
}

fn display_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R> LimitedReader<R> {
    const fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let buffer_len = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        let allowed = self.remaining.saturating_add(1).min(buffer_len);
        let allowed = usize::try_from(allowed).unwrap_or(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
        if read_u64 > self.remaining {
            return Err(std::io::Error::other("tar scan exceeds its byte limit"));
        }
        self.remaining -= read_u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn binary_records_round_trip_by_declared_length() {
        let mut records = PaxRecords::default();
        records
            .insert(b"SCHILY.xattr.user.test", b"line one\nline two\0tail")
            .expect("record");
        records.insert(b"mtime", b"-0.5").expect("mtime");
        let encoded = records.encode(1024).expect("encode");
        assert_eq!(PaxRecords::parse(&encoded, 1024).expect("parse"), records);
    }

    #[test]
    fn xattr_conventions_agree_for_binary_values_and_encoded_names() {
        let name = b"user.percent%=\xff".to_vec().into_boxed_slice();
        let value = b"line\nzero\0tail".to_vec().into_boxed_slice();
        let xattrs = Xattrs::from([(name, value)]);
        let mut records = PaxRecords::default();
        insert_xattrs(&mut records, &xattrs).expect("encode xattrs");
        assert_eq!(decode_xattrs(&records).expect("decode xattrs"), xattrs);
    }

    #[test]
    fn tar_index_is_sparse_and_bounds_aggregate_pax_payloads() {
        let mut records = PaxRecords::default();
        records.insert(b"mtime", b"1.25").expect("mtime");
        let payload_bytes =
            u64::try_from(records.encode(1024).expect("payload").len()).expect("payload length");
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for index in 0..2 {
                append_header(&mut builder, &records, 1024).expect("PAX header");
                let mut header = Header::new_ustar();
                header.set_size(0);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_entry_type(EntryType::Regular);
                header.set_path(format!("file-{index}")).expect("path");
                header.set_cksum();
                builder.append(&header, Cursor::new([])).expect("file");
            }
            for index in 0..64 {
                let mut header = Header::new_ustar();
                header.set_size(0);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_entry_type(EntryType::Regular);
                header.set_path(format!("plain-{index}")).expect("path");
                header.set_cksum();
                builder.append(&header, Cursor::new([])).expect("file");
            }
            builder.finish().expect("finish tar");
        }
        let limits = TarPaxLimits {
            entries: 66,
            total_bytes: u64::try_from(tar.len()).expect("tar length"),
            pax_bytes: 1024,
            index_bytes: payload_bytes * 2,
        };
        let index = scan_tar(tar.as_slice(), limits).expect("PAX index");
        assert_eq!(index.len, 66);
        assert_eq!(index.records.len(), 2);
        assert!(index.get(65).expect("ordinary entry").is_none());

        let error = scan_tar(
            tar.as_slice(),
            TarPaxLimits {
                index_bytes: payload_bytes * 2 - 1,
                ..limits
            },
        )
        .expect_err("aggregate PAX limit");
        assert!(matches!(
            error.downcast_ref::<PaxError>(),
            Some(PaxError::IndexBytesLimit { .. })
        ));
    }
}
