use std::sync::Arc;

use oci_spec::image::Descriptor;
use thiserror::Error;

/// Stable failure category returned by an [`OciContentStore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentErrorKind {
    /// Descriptor-addressed content cannot be obtained.
    Unavailable,
    /// The store refused an otherwise well-formed read or publication.
    Rejected,
    /// The store failed internally and cannot establish the operation result.
    Internal,
}

/// Failure to read or atomically publish descriptor-addressed OCI bytes.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("OCI content {kind:?}: {reason}")]
pub struct ContentError {
    kind: ContentErrorKind,
    reason: String,
}

impl ContentError {
    /// Creates a content-store failure without transport-specific decoration.
    #[must_use]
    pub fn new(kind: ContentErrorKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub fn kind(&self) -> ContentErrorKind {
        self.kind
    }

    /// Returns the store-provided diagnostic reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Exact-byte access to a content-addressed OCI store.
///
/// The store has no Catalog, tag, enumeration, deletion, Run identity, or
/// database operations. Implementations must be safe for concurrent calls.
pub trait OciContentStore: Send + Sync {
    /// Reads the exact bytes identified by a complete OCI Descriptor.
    ///
    /// The caller verifies media type, digest, and size before using the bytes;
    /// the store must not normalize or reserialize existing content.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError`] when the bytes cannot be obtained exactly.
    fn read(&self, descriptor: &Descriptor) -> Result<Arc<[u8]>, ContentError>;

    /// Atomically publishes exact bytes under their expected OCI Descriptor.
    ///
    /// Successful publication makes the complete bytes available to subsequent
    /// reads. Existing identical content is success; existing conflicting bytes
    /// must not be replaced. The method must not publish partial content.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError`] when descriptor verification or atomic
    /// publication cannot be completed.
    fn publish(&self, descriptor: &Descriptor, bytes: &[u8]) -> Result<(), ContentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_store_contract<T: OciContentStore>() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<T>();
    }

    #[test]
    fn store_contract_has_no_product_operations() {
        struct UnavailableStore;

        impl OciContentStore for UnavailableStore {
            fn read(&self, _descriptor: &Descriptor) -> Result<Arc<[u8]>, ContentError> {
                Err(ContentError::new(
                    ContentErrorKind::Unavailable,
                    "content is absent",
                ))
            }

            fn publish(&self, _descriptor: &Descriptor, _bytes: &[u8]) -> Result<(), ContentError> {
                Err(ContentError::new(
                    ContentErrorKind::Rejected,
                    "store is read-only",
                ))
            }
        }

        assert_store_contract::<UnavailableStore>();
    }
}
