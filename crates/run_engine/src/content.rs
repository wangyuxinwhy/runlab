use std::io::{Read, Seek};

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
/// database operations. It is a synchronous local data-plane boundary:
/// implementations must not resolve remote references, perform network I/O,
/// or detach work beyond the call that requested it. Implementations must be
/// safe for concurrent calls.
///
/// Engines enforce operation budgets around bounded reads, seeks, and writes.
/// As with Engine-owned rootfs I/O, an indefinitely blocked local filesystem or
/// kernel syscall is a host-failure boundary; this interface does not pretend a
/// deadline argument could cancel such a syscall.
pub trait OciContentStore: Send + Sync {
    /// Whether content that was successfully verified can be reused without
    /// re-reading its bytes. The default is conservative. A `true` result is a
    /// promise that published content cannot be replaced or removed while the
    /// store is in use.
    fn published_content_is_immutable(&self) -> bool {
        false
    }

    /// Opens the exact bytes identified by a complete OCI Descriptor.
    ///
    /// The returned reader starts at byte zero and supports seeking so a caller
    /// can verify and then consume large Layers without retaining them in
    /// memory. The caller verifies media type, digest, and size before using the
    /// content; the store must not normalize or reserialize existing bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError`] when the bytes cannot be obtained exactly.
    fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError>;

    /// Atomically publishes exact bytes under their expected OCI Descriptor.
    ///
    /// Successful publication makes the complete bytes available to subsequent
    /// reads. Existing identical content is success; existing conflicting bytes
    /// must not be replaced. The method must consume `content` synchronously,
    /// must not retain the reader, and must not publish partial content.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError`] when descriptor verification or atomic
    /// publication cannot be completed.
    fn publish(&self, descriptor: &Descriptor, content: &mut dyn Read) -> Result<(), ContentError>;
}

/// Seekable, streaming access to one immutable OCI content blob.
pub trait OciContent: Read + Seek + Send {}

impl<T> OciContent for T where T: Read + Seek + Send {}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn assert_store_contract<T: OciContentStore>() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<T>();
    }

    #[test]
    fn store_contract_has_no_product_operations() {
        struct UnavailableStore;

        impl OciContentStore for UnavailableStore {
            fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
                Err(ContentError::new(
                    ContentErrorKind::Unavailable,
                    "content is absent",
                ))
            }

            fn publish(
                &self,
                _descriptor: &Descriptor,
                _content: &mut dyn Read,
            ) -> Result<(), ContentError> {
                Err(ContentError::new(
                    ContentErrorKind::Rejected,
                    "store is read-only",
                ))
            }
        }

        assert_store_contract::<UnavailableStore>();
    }

    #[test]
    fn content_handle_can_be_rewound_without_buffering_in_the_interface() {
        let mut content: Box<dyn OciContent> = Box::new(Cursor::new(b"exact".to_vec()));
        let mut first = Vec::new();
        content.read_to_end(&mut first).expect("first read");
        content.rewind().expect("rewind");
        let mut second = Vec::new();
        content.read_to_end(&mut second).expect("second read");

        assert_eq!(first, b"exact");
        assert_eq!(second, first);
    }
}
