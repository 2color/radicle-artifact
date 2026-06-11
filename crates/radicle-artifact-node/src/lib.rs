//! Seeding node for radicle artifacts.
//!
//! Owns all blob I/O: the persistent iroh-blobs store, serving blobs to
//! peers, and fetching/exporting/HTTP downloads against that store. The
//! long-running daemon ([`node::run`]) listens on a Unix control socket;
//! the `rad-artifact` CLI (and other `radicle-artifact-client` users)
//! drive it over that socket. Long-running seeding is owned by [`node`];
//! the async fetch building blocks live in [`fetch`].

use std::io;

pub mod fetch;
pub mod iroh;
pub mod node;
pub mod seeder;

pub use iroh::EndpointConfig;

/// Errors from seeding and fetching operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP fetch failed.
    #[error("HTTP fetch failed: {0}")]
    Http(String),

    /// Iroh networking or fetch error.
    #[error("iroh error: {0}")]
    Iroh(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Downloaded content does not match expected CID.
    #[error("CID mismatch: expected {expected}, got {actual}")]
    CidMismatch {
        /// The CID that was expected.
        expected: String,
        /// The CID that was computed from the actual content.
        actual: String,
    },

    /// CID parsing or validation error.
    #[error("CID error: {0}")]
    Cid(String),
}

impl From<radicle_artifact_core::Error> for Error {
    fn from(e: radicle_artifact_core::Error) -> Self {
        use radicle_artifact_core::Error as Core;
        match e {
            Core::Io(e) => Error::Io(e),
            Core::Cid(s) => Error::Cid(s),
            Core::CidMismatch { expected, actual } => Error::CidMismatch { expected, actual },
            Core::Key(s) => Error::Iroh(s),
        }
    }
}
