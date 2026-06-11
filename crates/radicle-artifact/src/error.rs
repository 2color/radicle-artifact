//! Errors that are captured for artifact release related actions.

use radicle::{cob, git};
use thiserror::Error;

use crate::Cid;

/// Errors that can occur when setting a metadata entry on an artifact.
///
/// Validation is enforced at the [`ReleaseMut`][release_mut] boundary so
/// that malformed keys never reach the COB log; COB replay itself stays
/// permissive for determinism.
///
/// [release_mut]: super::ReleaseMut
#[derive(Debug, Error)]
pub enum Metadata {
    /// The metadata key was empty.
    #[error("metadata key must not be empty")]
    EmptyKey,
    /// The metadata key exceeds the maximum allowed byte length.
    #[error("metadata key exceeds maximum length of {max} bytes (got {actual})")]
    KeyTooLong {
        /// The actual byte length of the key.
        actual: usize,
        /// The maximum allowed byte length.
        max: usize,
    },
    /// The metadata key contained a control character (e.g. newline, tab, NUL).
    #[error("metadata key contains control character {:?}", ch)]
    KeyControlChar {
        /// The offending character.
        ch: char,
    },
    /// The serialized metadata value exceeds the maximum allowed byte length.
    #[error("metadata value exceeds maximum length of {max} bytes (got {actual})")]
    ValueTooLarge {
        /// The actual serialized byte length of the value.
        actual: usize,
        /// The maximum allowed byte length.
        max: usize,
    },
    /// An error occurred in the underlying COB store.
    #[error(transparent)]
    Store(#[from] cob::store::Error),
}

/// Errors that can occur when redacting an artifact.
#[derive(Debug, Error)]
pub enum Redact {
    /// The artifact CID was not found in the release.
    #[error("artifact {cid} not found in release")]
    NotFound {
        /// The CID that was not found.
        cid: Cid,
    },
    /// The redaction reason exceeds the maximum allowed length.
    #[error("redaction reason exceeds maximum length of {max} bytes (got {actual})")]
    ReasonTooLong {
        /// The actual byte length of the reason.
        actual: usize,
        /// The maximum allowed byte length.
        max: usize,
    },
    /// An error occurred in the underlying COB store.
    #[error(transparent)]
    Store(#[from] cob::store::Error),
}

/// Errors that can occur when creating a [`Release`][release] via
/// [`Releases::create`][create].
///
/// [release]: super::Release
/// [create]: super::Releases::create
#[derive(Debug, Error)]
pub enum Create {
    /// No annotated tag object with the given OID exists in the
    /// repository (e.g. a commit OID was supplied, or the tag has not
    /// been fetched).
    #[error("annotated tag {tag} not found in repository")]
    MissingTag {
        /// The OID that was supposed to identify an annotated tag.
        tag: git::Oid,
        /// The underlying error from Git that occurred.
        #[source]
        err: git::raw::Error,
    },
    /// The tag object exists but could not be resolved to a commit
    /// (e.g. it points at a tree/blob, or the tag chain is broken).
    #[error("annotated tag {tag} could not be resolved to a commit")]
    PeelFailed {
        /// The annotated tag OID.
        tag: git::Oid,
        /// The underlying error from Git that occurred while peeling.
        #[source]
        err: git::raw::Error,
    },
    /// The annotated tag's target peels to a different commit than the
    /// release commit OID.
    #[error("annotated tag {tag} peels to commit {actual}, expected {expected}")]
    TagMismatch {
        /// The annotated tag OID.
        tag: git::Oid,
        /// The release commit OID.
        expected: git::Oid,
        /// The commit the tag actually peels to.
        actual: git::Oid,
    },
    /// An error occurred in the underlying COB store.
    #[error(transparent)]
    Store(#[from] cob::store::Error),
}

/// Errors that can occur when building a [`Release`][release].
///
/// [release]: super::Release
#[derive(Debug, Error)]
pub enum Build {
    /// The initial action in the history of the [`Release`][release] was not a
    /// [`Create`][create].
    ///
    /// [release]: super::Release
    /// [create]: super::Action::Create
    #[error("initial action of release must create with an OID")]
    Initial,
    /// The [`Create`][create] referred to a commit that could not be found.
    ///
    /// [create]: super::Action::Create
    #[error("missing commit for release {oid}: {err}")]
    MissingCommit {
        /// The [`Oid`][oid] of the commit that was requested, but is missing.
        ///
        /// [oid]: git::Oid
        oid: git::Oid,
        /// The underlying error from Git that occurred.
        #[source]
        err: git::raw::Error,
    },
}

/// Errors that can occur when applying an [`Entry`][entry] to the [`Release`][release]
/// collaborative object.
///
/// [entry]: radicle::cob::Entry
/// [release]: super::Release
#[derive(Debug, Error)]
pub enum Apply {
    /// Applying the entry resulted in a [`Build`] error.
    #[error(transparent)]
    Build(#[from] Build),
    /// Error occurred when decoding an [`Entry`][entry] into an [`Op`][op].
    ///
    /// [entry]: radicle::cob::Entry
    /// [op]: radicle::cob::Op
    #[error(transparent)]
    Op(#[from] cob::op::OpEncodingError),
}
