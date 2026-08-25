use std::fmt;

use thiserror::Error;

/// Location of an invalid, unavailable, or unsupported value within a
/// [`crate::RunInput`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InputPath(Vec<PathSegment>);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum PathSegment {
    Field(String),
    Key(String),
    Index(usize),
}

impl InputPath {
    /// Creates a path rooted at a protocol field.
    #[must_use]
    pub fn field(field: impl Into<String>) -> Self {
        Self(vec![PathSegment::Field(field.into())])
    }

    /// Appends a nested protocol field.
    #[must_use]
    pub fn child(mut self, field: impl Into<String>) -> Self {
        self.0.push(PathSegment::Field(field.into()));
        self
    }

    /// Appends a map key without giving its text path or DNS semantics.
    #[must_use]
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.0.push(PathSegment::Key(key.into()));
        self
    }

    /// Appends an array index.
    #[must_use]
    pub fn index(mut self, index: usize) -> Self {
        self.0.push(PathSegment::Index(index));
        self
    }

    fn prefixed(mut self, mut prefix: Self) -> Self {
        prefix.0.append(&mut self.0);
        prefix
    }
}

impl fmt::Display for InputPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, segment) in self.0.iter().enumerate() {
            match segment {
                PathSegment::Field(field) if position == 0 => formatter.write_str(field)?,
                PathSegment::Field(field) => write!(formatter, ".{field}")?,
                PathSegment::Key(key) => write!(formatter, "[{key:?}]")?,
                PathSegment::Index(index) => write!(formatter, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// A structural violation found while constructing protocol input values.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid RunInput at {path}: {reason}")]
pub struct InputError {
    path: InputPath,
    reason: String,
}

impl InputError {
    #[must_use]
    pub(crate) fn new(path: InputPath, reason: impl Into<String>) -> Self {
        Self {
            path,
            reason: reason.into(),
        }
    }

    /// Adds the surrounding input location to an error produced while parsing
    /// a nested value.
    #[must_use]
    pub fn under(mut self, prefix: InputPath) -> Self {
        self.path = self.path.prefixed(prefix);
        self
    }

    /// Returns the input location that violated the protocol.
    #[must_use]
    pub fn path(&self) -> &InputPath {
        &self.path
    }

    /// Returns the protocol reason without transport-specific decoration.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Why a Run Engine implementation could not return a trustworthy
/// [`crate::RunOutput`].
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EngineError {
    /// The input or referenced content does not satisfy the protocol.
    #[error("invalid RunInput at {path}: {reason}")]
    InvalidInput {
        /// Location of the invalid protocol value.
        path: InputPath,
        /// Constraint that the value violates.
        reason: String,
    },

    /// Required content or an explicitly referenced host input cannot be obtained.
    #[error("unavailable RunInput at {path}: {reason}")]
    InputUnavailable {
        /// Location of the reference that could not be resolved.
        path: InputPath,
        /// Why the required input could not be obtained.
        reason: String,
    },

    /// The input is valid, but this Engine cannot execute it faithfully.
    #[error("unsupported RunInput at {path}: {reason}")]
    UnsupportedInput {
        /// Location of the unsupported protocol value.
        path: InputPath,
        /// Engine capability that prevents faithful execution.
        reason: String,
    },

    /// The Engine cannot form a structurally complete, trustworthy output.
    #[error("Run Engine failed: {reason}")]
    Internal {
        /// Why the Engine cannot return a trustworthy output.
        reason: String,
    },
}

impl EngineError {
    /// Creates an invalid-input error at a protocol field path.
    #[must_use]
    pub fn invalid(path: InputPath, reason: impl Into<String>) -> Self {
        Self::InvalidInput {
            path,
            reason: reason.into(),
        }
    }

    /// Creates an error for required input content that cannot be obtained.
    #[must_use]
    pub fn input_unavailable(path: InputPath, reason: impl Into<String>) -> Self {
        Self::InputUnavailable {
            path,
            reason: reason.into(),
        }
    }

    /// Creates an unsupported-input error at a protocol field path.
    #[must_use]
    pub fn unsupported(path: InputPath, reason: impl Into<String>) -> Self {
        Self::UnsupportedInput {
            path,
            reason: reason.into(),
        }
    }

    /// Creates an error for an Engine failure that prevents a trustworthy output.
    #[must_use]
    pub fn internal(reason: impl Into<String>) -> Self {
        Self::Internal {
            reason: reason.into(),
        }
    }

    /// Returns the input path for input-related errors.
    #[must_use]
    pub fn path(&self) -> Option<&InputPath> {
        match self {
            Self::InvalidInput { path, .. }
            | Self::InputUnavailable { path, .. }
            | Self::UnsupportedInput { path, .. } => Some(path),
            Self::Internal { .. } => None,
        }
    }

    /// Returns the underlying reason without presentation-specific prefixes.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::InvalidInput { reason, .. }
            | Self::InputUnavailable { reason, .. }
            | Self::UnsupportedInput { reason, .. }
            | Self::Internal { reason } => reason,
        }
    }
}

/// A violated invariant while assembling a [`crate::RunOutput`].
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid RunOutput at {path}: {reason}")]
pub struct OutputError {
    path: String,
    reason: String,
}

impl OutputError {
    #[must_use]
    pub(crate) fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// Returns the output location that violates an invariant.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the violated invariant without presentation decoration.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_error_keeps_protocol_categories_and_structured_paths() {
        let path = InputPath::field("programs")
            .key("dependency")
            .child("initial_environment");
        let errors = [
            EngineError::invalid(path.clone(), "invalid descriptor"),
            EngineError::input_unavailable(path.clone(), "blob is missing"),
            EngineError::unsupported(path.clone(), "platform is not supported"),
            EngineError::internal("cannot form trustworthy output"),
        ];

        assert_eq!(errors[0].path(), Some(&path));
        assert_eq!(errors[1].path(), Some(&path));
        assert_eq!(errors[2].path(), Some(&path));
        assert_eq!(errors[3].path(), None);
    }
}
