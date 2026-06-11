//! Share artifacts from Radicle Artifact COBs.
//!
//! Provides utilities for fetching and content-addressing artifacts
//! using iroh-blobs and BLAKE3 hashing. Long-running seeding is owned
//! by [`crate::node`] over the control socket; this module only covers
//! the read side (`fetch`) and the CID helpers used by both sides.
//!
//! This module is available when the `share` feature is enabled (default).
//!
//! Blob I/O is owned by the node: fetching, exporting, and HTTP downloads
//! all run against its persistent store and shared endpoint. The async
//! building blocks live in [`fetch`] (`download_iroh_to_store`,
//! `http_to_store`, the export helpers); the node's handlers and the CLI
//! (via the control socket) are the only callers.

use std::io;

pub mod cid_utils;
pub mod fetch;
pub mod iroh;
pub mod keys;

// Re-export key types for convenience.
pub use cid_utils::{
    artifact_kind, blake3_hash_to_cid, canonical_walk, cid_to_blake3_hash, compute_blob_cid,
    compute_content_id, verify_cid_file, ArtifactKind, BLAKE3_HASHSEQ_CODEC, HASH_CODE_BLAKE3,
    RAW_CODEC,
};
pub use iroh::EndpointConfig;

/// Errors from sharing operations.
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
