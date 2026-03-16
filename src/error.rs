//! Errors that are captured for artifact release related actions.

use radicle::{cob, git};
use thiserror::Error;

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