use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FsPath(Box<[u8]>);

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum FsPathError {
    #[error("unsafe filesystem path: {0}")]
    Unsafe(String),
    #[error("filesystem path exceeds the {limit}-byte limit: {observed}")]
    TooLong { limit: u64, observed: u64 },
}

impl FsPath {
    pub(crate) fn from_relative(raw: &[u8], limit: u64) -> Result<Self, FsPathError> {
        if raw.contains(&0) || raw.starts_with(b"/") {
            return Err(FsPathError::Unsafe(display_bytes(raw)));
        }
        let mut bytes = Vec::with_capacity(raw.len());
        for component in raw.split(|byte| *byte == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            if component == b".." {
                return Err(FsPathError::Unsafe(display_bytes(raw)));
            }
            if !bytes.is_empty() {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(component);
        }
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if observed > limit {
            return Err(FsPathError::TooLong { limit, observed });
        }
        Ok(Self(bytes.into_boxed_slice()))
    }

    pub(crate) fn from_absolute(raw: &[u8], limit: u64) -> Result<Self, FsPathError> {
        if !raw.starts_with(b"/") || raw.contains(&0) {
            return Err(FsPathError::Unsafe(display_bytes(raw)));
        }
        Self::from_relative(&raw[1..], limit)
    }

    pub(crate) fn from_normalized_components(
        components: &[Vec<u8>],
        limit: u64,
    ) -> Result<Self, FsPathError> {
        let mut bytes = Vec::new();
        for component in components {
            if !bytes.is_empty() {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(component);
        }
        Self::from_relative(&bytes, limit)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
    }

    pub(crate) fn parent(&self) -> Self {
        match self.0.iter().rposition(|byte| *byte == b'/') {
            Some(index) => Self(self.0[..index].into()),
            None => Self(Box::default()),
        }
    }

    pub(crate) fn basename(&self) -> &[u8] {
        match self.0.iter().rposition(|byte| *byte == b'/') {
            Some(index) => &self.0[index + 1..],
            None => &self.0,
        }
    }

    pub(crate) fn join_component(&self, component: &[u8], limit: u64) -> Result<Self, FsPathError> {
        if component.is_empty()
            || component == b"."
            || component == b".."
            || component.contains(&b'/')
            || component.contains(&0)
        {
            return Err(FsPathError::Unsafe(display_bytes(component)));
        }
        let mut bytes =
            Vec::with_capacity(self.0.len() + usize::from(!self.0.is_empty()) + component.len());
        bytes.extend_from_slice(&self.0);
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(component);
        Self::from_relative(&bytes, limit)
    }

    pub(crate) fn is_descendant_of(&self, ancestor: &Self) -> bool {
        if ancestor.is_root() {
            return !self.is_root();
        }
        self.0.len() > ancestor.0.len()
            && self.0.starts_with(&ancestor.0)
            && self.0[ancestor.0.len()] == b'/'
    }

    pub(crate) fn display(&self) -> String {
        format!("/{}", display_bytes(&self.0))
    }
}

fn display_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_raw_bytes_and_normalization_is_bounded() {
        let raw = FsPath::from_relative(b"a/./ff-\xff", 16).expect("raw path");
        let utf8 = FsPath::from_relative("a/ff-�".as_bytes(), 16).expect("utf8 path");
        assert_ne!(raw, utf8);
        assert_eq!(raw.as_bytes(), b"a/ff-\xff");
        assert_eq!(
            FsPath::from_relative(b"a//b", 3).expect("normalized"),
            FsPath::from_relative(b"a/b", 3).expect("canonical")
        );
        assert!(matches!(
            FsPath::from_relative(b"../escape", 128),
            Err(FsPathError::Unsafe(_))
        ));
        assert!(matches!(
            FsPath::from_relative(b"four", 3),
            Err(FsPathError::TooLong { .. })
        ));
    }
}
