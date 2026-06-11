//! Shared substrate for the radicle-artifact crates.
//!
//! Holds everything both sides of the control socket need to agree on,
//! with no networking stack attached: the wire [`protocol`], the [`cid`]
//! helpers for artifact content addressing, and the [`keys`] endpoint
//! identity type. Consumers that only speak the protocol (CLI, async
//! embedders) depend on this crate without pulling in iroh or tokio.

use std::io;

pub mod cid;
pub mod keys;
pub mod protocol;

/// Directory name (under the radicle home) that holds artifact state —
/// the node's store and the control socket both live here, so the client
/// and the node must agree on it.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Errors from CID computation and endpoint identity handling.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// CID parsing or validation error.
    #[error("CID error: {0}")]
    Cid(String),

    /// Content does not match expected CID.
    #[error("CID mismatch: expected {expected}, got {actual}")]
    CidMismatch {
        /// The CID that was expected.
        expected: String,
        /// The CID that was computed from the actual content.
        actual: String,
    },

    /// Endpoint id or key parsing/conversion error.
    #[error("key error: {0}")]
    Key(String),
}
