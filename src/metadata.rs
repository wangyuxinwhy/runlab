use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

const MAX_METADATA_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Metadata {
    description: Option<String>,
    labels: BTreeMap<String, String>,
}

impl Metadata {
    pub(crate) fn new(description: Option<String>, labels: &[Label]) -> Result<Self> {
        let mut mapped = BTreeMap::new();
        for label in labels {
            if mapped
                .insert(label.key.clone(), label.value.clone())
                .is_some()
            {
                bail!("metadata label key is duplicated: {}", label.key);
            }
        }
        let metadata = Self {
            description,
            labels: mapped,
        };
        if serde_json::to_vec(&metadata)?.len() > MAX_METADATA_BYTES {
            bail!("metadata exceeds the 8 KiB encoded size limit");
        }
        Ok(metadata)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Label {
    key: String,
    value: String,
}

impl FromStr for Label {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("metadata label must use KEY=VALUE"))?;
        if key.is_empty() {
            bail!("metadata label key must not be empty");
        }
        Ok(Self {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_split_only_the_first_equals_and_sort_by_key() {
        let metadata = Metadata::new(
            Some("agent base".to_owned()),
            &[
                "runtime=python".parse().expect("runtime label"),
                "command=a=b".parse().expect("command label"),
            ],
        )
        .expect("metadata");

        assert_eq!(metadata.description.as_deref(), Some("agent base"));
        assert_eq!(metadata.labels["command"], "a=b");
        assert_eq!(metadata.labels["runtime"], "python");
    }

    #[test]
    fn duplicate_and_oversized_metadata_are_rejected() {
        let duplicate = Metadata::new(
            None,
            &[
                "runtime=python".parse().expect("first label"),
                "runtime=rust".parse().expect("second label"),
            ],
        )
        .expect_err("duplicate label");
        assert!(duplicate.to_string().contains("duplicated"));

        let oversized = Metadata::new(Some("x".repeat(MAX_METADATA_BYTES)), &[])
            .expect_err("oversized metadata");
        assert!(oversized.to_string().contains("8 KiB"));
    }
}
