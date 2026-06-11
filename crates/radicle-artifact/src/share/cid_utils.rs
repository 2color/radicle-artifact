//! CID utilities for artifact content addressing.
//! Re-exported from `radicle-artifact-core`, which speaks `blake3::Hash`;
//! convert to/from `iroh_blobs::Hash` at the iroh-blobs API boundary.

pub use radicle_artifact_core::cid::{
    artifact_kind, blake3_hash_to_cid, canonical_walk, cid_to_blake3_hash, compute_blob_cid,
    compute_content_id, verify_cid_file, ArtifactKind, BLAKE3_HASHSEQ_CODEC, HASH_CODE_BLAKE3,
    RAW_CODEC,
};
