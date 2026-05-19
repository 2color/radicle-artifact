//! Wire protocol for the rad-artifact node control socket.
//!
//! One request and one response per Unix-socket connection, each a single
//! line of JSON terminated by `\n`. The client writes one [`Command`] and
//! reads one [`CommandResult<T>`], where `T` is the response type expected
//! for that command (the node always returns the matching type or a
//! [`CommandError`]).
//!
//! [`Command`] and [`ErrorCode`] are `#[non_exhaustive]` so future
//! additions (e.g. `Subscribe`) land without breaking the schema.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::share::cid_utils::ArtifactKind;

pub use crate::seeder::ImportMode;

/// A control-socket request.
///
/// Serialized as JSON with an internal `"command"` tag, kebab-case
/// variant names. Unit variants (`Status`, `Shutdown`) serialize to
/// `{"command":"status"}`.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Command {
    /// Report node status.
    Status,
    /// Import bytes from `path`, verify against `cid`, register the
    /// `seeded/{rid}/{cid}` tag.
    Seed {
        /// Repository the artifact belongs to.
        rid: String,
        /// Expected content identifier.
        cid: String,
        /// Path to the artifact on disk (file for blobs, directory for collections).
        path: PathBuf,
        /// Whether the artifact is a single blob or a collection.
        kind: ArtifactKind,
        /// Copy bytes into the store or reference them in place.
        mode: ImportMode,
    },
    /// Remove the `seeded/{rid}/{cid}` tag. Idempotent.
    Unseed {
        /// Repository the artifact belongs to.
        rid: String,
        /// Content identifier to stop seeding.
        cid: String,
    },
    /// Whether `(rid, cid)` is currently seeded.
    IsSeeding {
        /// Repository the artifact belongs to.
        rid: String,
        /// Content identifier to check.
        cid: String,
    },
    /// List CIDs seeded under `rid`.
    ListSeeded {
        /// Repository to enumerate.
        rid: String,
    },
    /// Ask the node to shut down gracefully.
    Shutdown,
}

/// Top-level response envelope.
///
/// Externally tagged so the wire reads `{"okay": <T>}` or
/// `{"error": <CommandError>}`. Type-stable across commands by parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CommandResult<T> {
    /// Success with command-specific payload.
    Okay(T),
    /// Failure with structured error.
    Error(CommandError),
}

/// Structured failure: an [`ErrorCode`] plus a human-readable message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandError {
    /// Machine-readable category.
    pub code: ErrorCode,
    /// Free-form detail; not for parsing.
    pub message: String,
}

/// Classifier for [`CommandError`]. `#[non_exhaustive]` so new codes are
/// additive.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// Content hash from `path` did not match the requested `cid`.
    CidMismatch,
    /// `path` does not exist or is not readable.
    PathNotFound,
    /// Tried to operate on a `(rid, cid)` that is not seeded.
    NotSeeding,
    /// Local I/O failure.
    Io,
    /// Iroh networking or store failure.
    Iroh,
    /// Bug or unhandled state inside the node.
    Internal,
}

/// Successful result of [`Command::Seed`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeedReceipt {
    /// Echo of the requested repository.
    pub rid: String,
    /// Echo of the requested CID.
    pub cid: String,
    /// Lowercase base32 (RFC 4648, no padding) endpoint id the node is serving on.
    pub endpoint_id: String,
    /// Logical size of the imported artifact in bytes.
    pub bytes: u64,
    /// `true` if this call newly tagged the pair; `false` if it was
    /// already tagged before the call.
    pub was_new: bool,
}

/// Successful result of [`Command::Unseed`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnseedReceipt {
    /// Echo of the requested repository.
    pub rid: String,
    /// Echo of the requested CID.
    pub cid: String,
    /// `true` if a tag was removed; `false` if no tag existed.
    pub was_removed: bool,
}

/// One entry returned by [`Command::ListSeeded`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeededEntry {
    /// Content identifier currently tagged under the requested rid.
    pub cid: String,
    /// Logical artifact size in bytes. Best-effort — zero if the iroh
    /// status call temporarily fails.
    pub bytes: u64,
}

/// Successful result of [`Command::Status`]. See the design doc for the
/// per-field source recipes; in v1 only the obvious fields are wired and
/// the rest default to zero.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    /// Lowercase base32 (RFC 4648, no padding) endpoint id.
    pub endpoint_id: String,
    /// Unix timestamp (seconds) when the node bound its socket.
    pub started_at_unix: i64,
    /// Aggregated tag-level stats.
    pub seeded: SeededStats,
    /// On-disk store stats.
    pub disk: DiskStats,
    /// Connection counters derived from iroh's metrics.
    pub connections: ConnectionStats,
    /// Bytes-on-the-wire counters from iroh's socket metrics.
    pub traffic: TrafficStats,
    /// Soft warnings rendered as advice to the user.
    pub warnings: Warnings,
}

/// Aggregated `seeded/{rid}/{cid}` tag stats across all repos.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeededStats {
    /// Number of tagged `(rid, cid)` pairs.
    pub count: usize,
    /// Sum of logical artifact sizes across all tags.
    pub bytes_logical: u64,
}

/// Disk usage for the store backing this node.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskStats {
    /// Total bytes on disk under `<home>/artifacts/store/`. Includes db
    /// and scratch overhead alongside seeded content.
    pub store_bytes: u64,
    /// Logical bytes of seeded artifacts. Equal to [`SeededStats::bytes_logical`].
    pub seeded_bytes_logical: u64,
}

/// QUIC connection counters from iroh's socket metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionStats {
    /// Currently open connections (`opened_total - closed_total`).
    pub active: u32,
    /// Lifetime opened-handshaked count (excludes 0-RTT).
    pub opened_total: u64,
    /// Lifetime closed count.
    pub closed_total: u64,
    /// Lifetime count of direct (non-relayed) connections.
    pub direct_total: u64,
    /// Lifetime count of holepunch attempts (client-side increments only).
    pub holepunch_attempts: u64,
    /// Path counter: direct.
    pub paths_direct: u64,
    /// Path counter: relayed.
    pub paths_relayed: u64,
    /// Path counter: custom (user-supplied transports).
    pub paths_custom: u64,
}

/// Bytes-on-the-wire counters from iroh's socket metrics. See the design
/// doc for the disco-vs-data semantics — `out_bytes` includes disco
/// frames, `in_bytes` excludes them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrafficStats {
    /// Bytes sent across ipv4/ipv6/relay (includes disco).
    pub out_bytes: u64,
    /// Bytes received across ipv4/ipv6/relay/custom (data only).
    pub in_bytes: u64,
    /// Packet-count cross-check (different unit from bytes).
    pub recv_datagrams: u64,
}

/// Soft warnings surfaced in `Status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Warnings {
    /// Count of COB locations under our DID whose endpoint id does not
    /// match the node's current endpoint id. Populated client-side at
    /// status-print time; the node always returns zero here.
    pub did_locations_unmatched: usize,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    /// Catch-all check that every top-level type round-trips through
    /// JSON and the encoding stays bit-for-bit stable. If you add a
    /// field or change a tag, expect to update the literals here.
    #[test]
    fn wire_snapshot_command_status() {
        let cmd = Command::Status;
        let s = serde_json::to_string(&cmd).unwrap();
        assert_eq!(s, r#"{"command":"status"}"#);
        let back: Command = serde_json::from_str(&s).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn wire_snapshot_command_seed() {
        let cmd = Command::Seed {
            rid: "rad:abc".into(),
            cid: "bafy...".into(),
            path: PathBuf::from("/tmp/a"),
            kind: ArtifactKind::Blob,
            mode: ImportMode::Copy,
        };
        let s = serde_json::to_value(&cmd).unwrap();
        assert_eq!(
            s,
            json!({
                "command": "seed",
                "rid": "rad:abc",
                "cid": "bafy...",
                "path": "/tmp/a",
                "kind": "blob",
                "mode": "copy",
            })
        );
    }

    #[test]
    fn wire_snapshot_command_unseed_and_lookups() {
        let unseed = Command::Unseed {
            rid: "r".into(),
            cid: "c".into(),
        };
        assert_eq!(
            serde_json::to_value(&unseed).unwrap(),
            json!({"command":"unseed", "rid":"r", "cid":"c"})
        );

        let is_seeding = Command::IsSeeding {
            rid: "r".into(),
            cid: "c".into(),
        };
        assert_eq!(
            serde_json::to_value(&is_seeding).unwrap(),
            json!({"command":"is-seeding", "rid":"r", "cid":"c"})
        );

        let list = Command::ListSeeded { rid: "r".into() };
        assert_eq!(
            serde_json::to_value(&list).unwrap(),
            json!({"command":"list-seeded", "rid":"r"})
        );

        let shutdown = Command::Shutdown;
        assert_eq!(
            serde_json::to_value(&shutdown).unwrap(),
            json!({"command":"shutdown"})
        );
    }

    #[test]
    fn wire_snapshot_command_result_ok_and_err() {
        let ok: CommandResult<u32> = CommandResult::Okay(7);
        assert_eq!(serde_json::to_value(&ok).unwrap(), json!({"okay": 7}));

        let err: CommandResult<u32> = CommandResult::Error(CommandError {
            code: ErrorCode::CidMismatch,
            message: "expected != actual".into(),
        });
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"error": {"code": "cid-mismatch", "message": "expected != actual"}})
        );
    }

    #[test]
    fn wire_snapshot_receipts() {
        let seed = SeedReceipt {
            rid: "r".into(),
            cid: "c".into(),
            endpoint_id: "ABCDE".into(),
            bytes: 42,
            was_new: true,
        };
        assert_eq!(
            serde_json::to_value(&seed).unwrap(),
            json!({
                "rid": "r",
                "cid": "c",
                "endpoint_id": "ABCDE",
                "bytes": 42,
                "was_new": true,
            })
        );

        let unseed = UnseedReceipt {
            rid: "r".into(),
            cid: "c".into(),
            was_removed: false,
        };
        assert_eq!(
            serde_json::to_value(&unseed).unwrap(),
            json!({"rid":"r","cid":"c","was_removed":false})
        );

        let entry = SeededEntry {
            cid: "c".into(),
            bytes: 1024,
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"cid":"c","bytes":1024})
        );
    }

    #[test]
    fn wire_snapshot_status_default() {
        let st = Status::default();
        assert_eq!(
            serde_json::to_value(&st).unwrap(),
            json!({
                "endpoint_id": "",
                "started_at_unix": 0,
                "seeded": {"count": 0, "bytes_logical": 0},
                "disk": {"store_bytes": 0, "seeded_bytes_logical": 0},
                "connections": {
                    "active": 0,
                    "opened_total": 0,
                    "closed_total": 0,
                    "direct_total": 0,
                    "holepunch_attempts": 0,
                    "paths_direct": 0,
                    "paths_relayed": 0,
                    "paths_custom": 0,
                },
                "traffic": {"out_bytes": 0, "in_bytes": 0, "recv_datagrams": 0},
                "warnings": {"did_locations_unmatched": 0},
            })
        );
    }
}
