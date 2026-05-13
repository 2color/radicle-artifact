//! Share artifacts from Radicle Artifact COBs.
//!
//! Provides utilities for fetching, serving, and content-addressing artifacts
//! using iroh-blobs and BLAKE3 hashing.
//!
//! This module is available when the `share` feature is enabled (default).
//!
//! # Sync/async design
//!
//! The fetch entry points [`download`] and [`download_collection`] are
//! **synchronous**. Each call creates an ephemeral `tokio::Runtime`, a
//! single `iroh::Endpoint` shared across all iroh providers, and a
//! temporary blob store — torn down before the function returns. This is
//! intended for CLI tools that perform isolated, one-shot fetches.
//!
//! Applications with a long-lived async runtime and persistent iroh endpoint
//! (e.g. a Tauri desktop app) should use `iroh_blobs::api::downloader::Downloader`
//! directly instead of these functions.
//!
//! The [`Server`] type similarly uses an in-memory store suited for ephemeral
//! CLI serving. Long-running apps should build their own
//! `iroh::protocol::Router` with an `iroh_blobs::store::fs::FsStore`.

use std::io;

pub mod cid_utils;
pub mod endpoint;
pub mod fetch;
pub mod identity;
pub mod keys;
pub mod serve;

// Re-export key types for convenience.
pub use cid_utils::{
    artifact_kind, blake3_hash_to_cid, canonical_walk, cid_to_blake3_hash, compute_blob_cid,
    compute_content_id, verify_cid, verify_cid_file, ArtifactKind, BLAKE3_HASHSEQ_CODEC,
    HASH_CODE_BLAKE3, RAW_CODEC,
};
pub use endpoint::EndpointPreset;
pub use fetch::{download, download_collection, Location};
pub use identity::load_or_generate_key;
pub use keys::{did_to_iroh_public_key, endpoint_id_from_iroh_url};
pub use serve::{add_blob, add_collection, Server};

/// Errors from sharing operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// URL scheme not handled by any registered fetcher.
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    /// HTTP location given for a collection artifact. HTTP fetch is only
    /// implemented for single-blob artifacts; multi-file collections require
    /// an iroh provider.
    #[error("HTTP fetch is not supported for collection artifacts (URL: {0}); an iroh provider is required")]
    HttpCollectionUnsupported(String),

    /// HTTP fetch failed.
    #[error("HTTP fetch failed: {0}")]
    Http(String),

    /// Iroh networking or fetch error.
    #[error("iroh error: {0}")]
    Iroh(String),

    /// Serve/blob-store error.
    #[error("serve error: {0}")]
    Serve(String),

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

    /// No locations registered for the artifact.
    #[error("no locations registered for this artifact")]
    NoLocations,

    /// All fetch attempts failed.
    #[error("all {} fetch attempt{} failed:\n{}", .0.len(), if .0.len() == 1 { "" } else { "s" }, format_attempts(.0))]
    AllFailed(Vec<Error>),
}

/// Format each per-location error as a numbered list for [`Error::AllFailed`] display.
fn format_attempts(errors: &[Error]) -> String {
    errors
        .iter()
        .enumerate()
        .map(|(i, e)| format!("  {}: {e}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}
