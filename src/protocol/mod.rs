//! Wire protocol for the rad-artifact node control socket.
//!
//! One request per Unix-socket connection, each command a single line of
//! JSON terminated by `\n`. Most commands are one-shot: the client writes
//! one [`Command`] and reads one [`CommandResult<T>`]. The streaming
//! commands ([`Command::Fetch`], [`Command::Export`]) instead emit a
//! sequence of [`StreamEvent`] frames — zero or more `progress`, then one
//! terminal `okay`/`error` whose tags match [`CommandResult`].
//!
//! [`Command`] and [`ErrorCode`] are `#[non_exhaustive]` so future
//! additions land without breaking the schema; the response payload
//! structs are likewise `#[non_exhaustive]` so fields can be added without
//! breaking downstream crates that link this type.

use std::path::PathBuf;

use cid::Cid;
use radicle::identity::RepoId;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::share::cid_utils::ArtifactKind;
use crate::share::keys::EndpointId;

/// Serde glue for `Cid` on the wire.
///
/// The `cid` crate's derived [`serde::Serialize`] encodes a CID as a
/// newtype-struct of raw bytes, which renders as a JSON byte array.
/// We want the canonical multibase string (`"bafy…"`) instead, so wire
/// fields carrying a [`Cid`] are annotated with
/// `#[serde(with = "cid_string")]`.
mod cid_string {
    use std::str::FromStr;

    use cid::Cid;
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &Cid, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Cid, D::Error> {
        let s = String::deserialize(d)?;
        Cid::from_str(&s).map_err(de::Error::custom)
    }
}

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
        rid: RepoId,
        /// Expected content identifier.
        #[serde(with = "cid_string")]
        cid: Cid,
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
        rid: RepoId,
        /// Content identifier to stop seeding.
        #[serde(with = "cid_string")]
        cid: Cid,
    },
    /// Whether `(rid, cid)` is currently seeded.
    IsSeeding {
        /// Repository the artifact belongs to.
        rid: RepoId,
        /// Content identifier to check.
        #[serde(with = "cid_string")]
        cid: Cid,
    },
    /// List CIDs seeded under `rid`.
    ListSeeded {
        /// Repository to enumerate.
        rid: RepoId,
    },
    /// Cheap predicate: is this CID's content present/complete in the
    /// store? No network. One-shot, returns [`HasResult`]. Hash-keyed and
    /// repo-agnostic.
    Has {
        /// Content identifier to look up.
        #[serde(with = "cid_string")]
        cid: Cid,
    },
    /// Export already-local bytes to `dest`. No network. Streaming —
    /// emits `exporting` progress, then [`ExportReceipt`]. Errors with
    /// [`ErrorCode::NotLocal`] if the content isn't complete in the store.
    Export {
        /// Content identifier to export.
        #[serde(with = "cid_string")]
        cid: Cid,
        /// Destination path (file for blobs, directory for collections).
        dest: PathBuf,
    },
    /// Fetch an artifact: fast-path export if already local, else download
    /// from `locations` into the store, export to `dest`, optionally tag
    /// as seeded. Streaming — emits progress, then [`FetchReceipt`].
    Fetch {
        /// Repository the artifact belongs to (for the seeded tag).
        rid: RepoId,
        /// Expected content identifier; the blob kind is derived from it.
        #[serde(with = "cid_string")]
        cid: Cid,
        /// Resolved providers/URLs to try. Iroh providers are batched into
        /// one multi-provider download; URLs are tried in sequence.
        locations: Vec<FetchLocation>,
        /// Destination path (file for blobs, directory for collections).
        dest: PathBuf,
        /// Tag `seeded/{rid}/{cid}` after completion so the node serves it.
        seed: bool,
    },
    /// Ask the node to shut down gracefully.
    Shutdown,
}

/// A resolved place to fetch an artifact from.
///
/// Owned and serde-friendly, unlike [`crate::share::fetch::Location`].
/// The caller resolves COB locations (including DID-derived bare
/// `radiroh://` entries) into this concrete form; the node does no
/// identity resolution of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FetchLocation {
    /// An HTTP(S) (or other-scheme) URL, serialized as the URL string.
    Url(Url),
    /// An iroh provider, serialized as the canonical `radiroh://<base32>` URL.
    Iroh(EndpointId),
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

/// Frame of a streaming response ([`Command::Fetch`], [`Command::Export`]).
///
/// Externally tagged: `{"progress": …}` (repeatable, non-terminal) then
/// exactly one terminal `{"okay": <T>}` / `{"error": <CommandError>}`. The
/// terminal tags deliberately match [`CommandResult`] so a generic reader
/// recognizes them; `progress` is the new, non-terminal frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StreamEvent<T> {
    /// Non-terminal progress update.
    Progress(FetchProgress),
    /// Terminal success with command-specific payload.
    Okay(T),
    /// Terminal failure.
    Error(CommandError),
}

/// One progress frame for a streaming command.
///
/// An enum, not a struct, so provider-level events (which carry no byte
/// offset) and byte-movement events are modeled distinctly. The variants
/// map onto the iroh `DownloadProgressItem` kinds the download loop
/// already produces. `Export` only ever emits [`FetchProgress::Exporting`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum FetchProgress {
    /// Endpoint/relay setup, before any provider is tried.
    Connecting,
    /// Now attempting this provider.
    TryingProvider {
        /// Provider being tried.
        endpoint_id: EndpointId,
    },
    /// This provider failed; moving on to the next.
    ProviderFailed {
        /// Provider that failed.
        endpoint_id: EndpointId,
    },
    /// Byte movement during download.
    Downloading {
        /// Bytes downloaded so far.
        offset: u64,
        /// Total size, if known.
        total: Option<u64>,
    },
    /// Byte movement while writing the store out to disk.
    Exporting {
        /// Bytes exported so far.
        offset: u64,
        /// Total size, if known.
        total: Option<u64>,
        /// Collection member being exported; `None` for a single blob.
        entry: Option<String>,
    },
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
    /// Wire-level decode failure: the command line was not valid JSON,
    /// or a typed field (rid, cid, …) failed to parse. The accompanying
    /// `message` surfaces the underlying serde error.
    InvalidRequest,
    /// `Export` (or a fetch fast path) needed local bytes that the store
    /// does not hold completely.
    NotLocal,
    /// `Fetch` was given no usable locations to try.
    NoLocations,
    /// Every provider/URL a `Fetch` tried failed; `message` lists them.
    AllFailed,
    /// Bug or unhandled state inside the node.
    Internal,
}

/// Successful result of [`Command::Seed`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeedReceipt {
    /// Echo of the requested repository.
    pub rid: RepoId,
    /// Echo of the requested CID.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// Endpoint id the node is serving on, as a canonical `radiroh://<base32>` URL.
    pub endpoint_id: EndpointId,
    /// Logical size of the imported artifact in bytes.
    pub bytes: u64,
    /// `true` if this call newly tagged the pair; `false` if it was
    /// already tagged before the call.
    pub was_new: bool,
}

/// Successful result of [`Command::Unseed`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnseedReceipt {
    /// Echo of the requested repository.
    pub rid: RepoId,
    /// Echo of the requested CID.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// `true` if a tag was removed; `false` if no tag existed.
    pub was_removed: bool,
}

/// One entry returned by [`Command::ListSeeded`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeededEntry {
    /// Content identifier currently tagged under the requested rid.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// Logical artifact size in bytes. Best-effort — zero if the iroh
    /// status call temporarily fails.
    pub bytes: u64,
}

/// Result of [`Command::Has`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HasResult {
    /// Some bytes for this CID are in the store.
    pub present: bool,
    /// The content is fully downloaded.
    pub complete: bool,
    /// Logical size known so far, in bytes.
    pub bytes: u64,
}

/// Terminal result of [`Command::Export`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportReceipt {
    /// Echo of the exported CID.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// Where the bytes were written.
    pub dest: PathBuf,
    /// Logical size exported, in bytes.
    pub bytes: u64,
}

/// Terminal result of [`Command::Fetch`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchReceipt {
    /// Echo of the requested repository.
    pub rid: RepoId,
    /// Echo of the fetched CID.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// Where the bytes were written.
    pub dest: PathBuf,
    /// Logical size fetched, in bytes.
    pub bytes: u64,
    /// `true` if the bytes were already local; no network was used.
    pub from_cache: bool,
    /// `true` if a `seeded/{rid}/{cid}` tag is now set.
    pub seeded: bool,
    /// Endpoint id the node serves on, as a canonical `radiroh://<base32>`
    /// URL. Present so the caller can write the `add_location` COB after a
    /// `seed: true` fetch. Mirrors [`SeedReceipt::endpoint_id`].
    pub endpoint_id: EndpointId,
}

/// Successful result of [`Command::Status`]. See the design doc for the
/// per-field source recipes; in v1 only the obvious fields are wired and
/// the rest default to zero.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    /// Endpoint id the node is serving on, as a canonical `radiroh://<base32>` URL.
    pub endpoint_id: EndpointId,
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
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeededStats {
    /// Number of tagged `(rid, cid)` pairs.
    pub count: usize,
    /// Sum of logical artifact sizes across all tags.
    pub bytes_logical: u64,
}

/// Disk usage for the store backing this node.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskStats {
    /// Total bytes on disk under `<home>/artifacts/store/`. Includes db
    /// and scratch overhead alongside seeded content.
    pub store_bytes: u64,
    /// Logical bytes of seeded artifacts. Equal to [`SeededStats::bytes_logical`].
    pub seeded_bytes_logical: u64,
}

/// QUIC connection counters from iroh's socket metrics.
#[non_exhaustive]
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
}

/// Bytes-on-the-wire counters from iroh's socket metrics. See the design
/// doc for the disco-vs-data semantics — `out_bytes` includes disco
/// frames, `in_bytes` excludes them.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrafficStats {
    /// Bytes sent across ipv4/ipv6/relay (includes disco).
    pub out_bytes: u64,
    /// Bytes received across ipv4/ipv6/relay/custom (data only).
    pub in_bytes: u64,
}

/// Soft warnings surfaced in `Status`.
#[non_exhaustive]
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
    use std::str::FromStr;

    use serde_json::json;

    use super::*;

    /// Real RepoId used as a wire-snapshot fixture.
    const SAMPLE_RID: &str = "rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip";

    fn sample_rid() -> RepoId {
        RepoId::from_str(SAMPLE_RID).unwrap()
    }

    /// Real Blake3/raw Cid for wire snapshots — built from a pinned
    /// preimage so the multibase string stays stable across runs.
    fn sample_cid() -> Cid {
        let digest = blake3::hash(b"protocol-cid-sample");
        let mh = cid::multihash::Multihash::<64>::wrap(
            crate::share::cid_utils::HASH_CODE_BLAKE3,
            digest.as_bytes(),
        )
        .unwrap();
        Cid::new_v1(crate::share::cid_utils::RAW_CODEC, mh)
    }

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
        let cid = sample_cid();
        let cmd = Command::Seed {
            rid: sample_rid(),
            cid,
            path: PathBuf::from("/tmp/a"),
            kind: ArtifactKind::Blob,
            mode: ImportMode::Copy,
        };
        let s = serde_json::to_value(&cmd).unwrap();
        assert_eq!(
            s,
            json!({
                "command": "seed",
                "rid": SAMPLE_RID,
                "cid": cid.to_string(),
                "path": "/tmp/a",
                "kind": "blob",
                "mode": "copy",
            })
        );
    }

    #[test]
    fn wire_snapshot_command_unseed_and_lookups() {
        let cid = sample_cid();
        let unseed = Command::Unseed {
            rid: sample_rid(),
            cid,
        };
        assert_eq!(
            serde_json::to_value(&unseed).unwrap(),
            json!({"command":"unseed", "rid": SAMPLE_RID, "cid": cid.to_string()})
        );

        let is_seeding = Command::IsSeeding {
            rid: sample_rid(),
            cid,
        };
        assert_eq!(
            serde_json::to_value(&is_seeding).unwrap(),
            json!({"command":"is-seeding", "rid": SAMPLE_RID, "cid": cid.to_string()})
        );

        let list = Command::ListSeeded { rid: sample_rid() };
        assert_eq!(
            serde_json::to_value(&list).unwrap(),
            json!({"command":"list-seeded", "rid": SAMPLE_RID})
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
        let endpoint_id = sample_endpoint_id();
        let cid = sample_cid();
        let seed = SeedReceipt {
            rid: sample_rid(),
            cid,
            endpoint_id,
            bytes: 42,
            was_new: true,
        };
        assert_eq!(
            serde_json::to_value(&seed).unwrap(),
            json!({
                "rid": SAMPLE_RID,
                "cid": cid.to_string(),
                // Serialized as the canonical radiroh:// URL form.
                "endpoint_id": endpoint_id.to_string(),
                "bytes": 42,
                "was_new": true,
            })
        );
        // Round-trips back to the same typed value.
        let back: SeedReceipt =
            serde_json::from_value(serde_json::to_value(&seed).unwrap()).unwrap();
        assert_eq!(back, seed);

        let unseed = UnseedReceipt {
            rid: sample_rid(),
            cid,
            was_removed: false,
        };
        assert_eq!(
            serde_json::to_value(&unseed).unwrap(),
            json!({"rid": SAMPLE_RID, "cid": cid.to_string(), "was_removed": false})
        );

        let entry = SeededEntry { cid, bytes: 1024 };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"cid": cid.to_string(), "bytes": 1024})
        );
    }

    /// Fixed endpoint id for wire snapshots; derived from a pinned secret.
    fn sample_endpoint_id() -> EndpointId {
        iroh::SecretKey::from_bytes(&[7u8; 32]).public().into()
    }

    #[test]
    fn wire_snapshot_status_zeroed() {
        let endpoint_id = sample_endpoint_id();
        let st = Status {
            endpoint_id,
            started_at_unix: 0,
            seeded: SeededStats::default(),
            disk: DiskStats::default(),
            connections: ConnectionStats::default(),
            traffic: TrafficStats::default(),
            warnings: Warnings::default(),
        };
        assert_eq!(
            serde_json::to_value(&st).unwrap(),
            json!({
                "endpoint_id": endpoint_id.to_string(),
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
                },
                "traffic": {"out_bytes": 0, "in_bytes": 0},
                "warnings": {"did_locations_unmatched": 0},
            })
        );
    }

    #[test]
    fn wire_snapshot_command_has_export_fetch() {
        let cid = sample_cid();
        let endpoint_id = sample_endpoint_id();

        let has = Command::Has { cid };
        assert_eq!(
            serde_json::to_value(&has).unwrap(),
            json!({"command": "has", "cid": cid.to_string()})
        );

        let export = Command::Export {
            cid,
            dest: PathBuf::from("/tmp/out"),
        };
        assert_eq!(
            serde_json::to_value(&export).unwrap(),
            json!({"command": "export", "cid": cid.to_string(), "dest": "/tmp/out"})
        );

        let fetch = Command::Fetch {
            rid: sample_rid(),
            cid,
            locations: vec![
                FetchLocation::Iroh(endpoint_id),
                FetchLocation::Url(Url::parse("https://e.x/f").unwrap()),
            ],
            dest: PathBuf::from("/tmp/out"),
            seed: true,
        };
        assert_eq!(
            serde_json::to_value(&fetch).unwrap(),
            json!({
                "command": "fetch",
                "rid": SAMPLE_RID,
                "cid": cid.to_string(),
                "locations": [
                    {"iroh": endpoint_id.to_string()},
                    {"url": "https://e.x/f"},
                ],
                "dest": "/tmp/out",
                "seed": true,
            })
        );
        // Round-trips back to the same typed value.
        let back: Command = serde_json::from_value(serde_json::to_value(&fetch).unwrap()).unwrap();
        assert_eq!(back, fetch);
    }

    #[test]
    fn wire_snapshot_stream_event() {
        // Terminal tags match CommandResult; `progress` is the new frame.
        let progress: StreamEvent<u32> = StreamEvent::Progress(FetchProgress::Connecting);
        assert_eq!(
            serde_json::to_value(&progress).unwrap(),
            json!({"progress": {"kind": "connecting"}})
        );

        let ok: StreamEvent<u32> = StreamEvent::Okay(7);
        assert_eq!(serde_json::to_value(&ok).unwrap(), json!({"okay": 7}));

        let err: StreamEvent<u32> = StreamEvent::Error(CommandError {
            code: ErrorCode::AllFailed,
            message: "no providers".into(),
        });
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"error": {"code": "all-failed", "message": "no providers"}})
        );
    }

    #[test]
    fn wire_snapshot_fetch_progress() {
        let endpoint_id = sample_endpoint_id();
        let cases = [
            (FetchProgress::Connecting, json!({"kind": "connecting"})),
            (
                FetchProgress::TryingProvider { endpoint_id },
                json!({"kind": "trying-provider", "endpoint_id": endpoint_id.to_string()}),
            ),
            (
                FetchProgress::ProviderFailed { endpoint_id },
                json!({"kind": "provider-failed", "endpoint_id": endpoint_id.to_string()}),
            ),
            (
                FetchProgress::Downloading {
                    offset: 65536,
                    total: Some(1048576),
                },
                json!({"kind": "downloading", "offset": 65536, "total": 1048576}),
            ),
            (
                FetchProgress::Exporting {
                    offset: 10,
                    total: None,
                    entry: Some("a/b.txt".into()),
                },
                json!({"kind": "exporting", "offset": 10, "total": null, "entry": "a/b.txt"}),
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(serde_json::to_value(&value).unwrap(), expected);
            let back: FetchProgress =
                serde_json::from_value(serde_json::to_value(&value).unwrap()).unwrap();
            assert_eq!(back, value);
        }
    }

    #[test]
    fn wire_snapshot_fetch_results() {
        let cid = sample_cid();
        let endpoint_id = sample_endpoint_id();

        let has = HasResult {
            present: true,
            complete: false,
            bytes: 1024,
        };
        assert_eq!(
            serde_json::to_value(&has).unwrap(),
            json!({"present": true, "complete": false, "bytes": 1024})
        );

        let export = ExportReceipt {
            cid,
            dest: PathBuf::from("/tmp/out"),
            bytes: 2048,
        };
        assert_eq!(
            serde_json::to_value(&export).unwrap(),
            json!({"cid": cid.to_string(), "dest": "/tmp/out", "bytes": 2048})
        );

        let fetch = FetchReceipt {
            rid: sample_rid(),
            cid,
            dest: PathBuf::from("/tmp/out"),
            bytes: 4096,
            from_cache: false,
            seeded: true,
            endpoint_id,
        };
        assert_eq!(
            serde_json::to_value(&fetch).unwrap(),
            json!({
                "rid": SAMPLE_RID,
                "cid": cid.to_string(),
                "dest": "/tmp/out",
                "bytes": 4096,
                "from_cache": false,
                "seeded": true,
                "endpoint_id": endpoint_id.to_string(),
            })
        );
        let back: FetchReceipt =
            serde_json::from_value(serde_json::to_value(&fetch).unwrap()).unwrap();
        assert_eq!(back, fetch);
    }
}
