use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::run::RunId;

const MAX_RUN_IDS: usize = 1000;
const MAX_RUN_IDS_BYTES: usize = 64 * 1024;

pub(super) fn read_optional_run_ids(source: Option<&Path>) -> Result<BTreeSet<String>> {
    source.map_or_else(|| Ok(BTreeSet::new()), read_required_run_ids)
}

pub(super) fn read_required_run_ids(source: &Path) -> Result<BTreeSet<String>> {
    let bytes = read_bounded(source, MAX_RUN_IDS_BYTES, "Run ID input")?;
    let text = std::str::from_utf8(&bytes).context("Run ID input is not valid UTF-8")?;
    let mut ids = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let value = line.trim();
        if value.is_empty() {
            continue;
        }
        let id = value
            .parse::<RunId>()
            .with_context(|| format!("Run ID input line {} is invalid", index.saturating_add(1)))?
            .to_string();
        if !ids.insert(id.clone()) {
            bail!("Run ID input contains a duplicate: {id}");
        }
        if ids.len() > MAX_RUN_IDS {
            bail!(
                "Run ID input exceeds {MAX_RUN_IDS} entries; split the selection into bounded batches"
            );
        }
    }
    if ids.is_empty() {
        bail!("Run ID input must contain at least one Run identity");
    }
    Ok(ids)
}

pub(super) fn read_bounded(source: &Path, limit: usize, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if source == Path::new("-") {
        std::io::stdin()
            .lock()
            .take(u64::try_from(limit)? + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {label} from stdin"))?;
    } else {
        std::fs::File::open(source)
            .with_context(|| format!("failed to open {label} file {}", source.display()))?
            .take(u64::try_from(limit)? + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {label} file {}", source.display()))?;
    }
    if bytes.len() > limit {
        bail!("{label} exceeds {limit} bytes");
    }
    Ok(bytes)
}
