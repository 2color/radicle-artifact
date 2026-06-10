//! Wire protocol for the rad-artifact node control socket.
//!
//! One request per Unix-socket connection, each command a single line of
//! JSON terminated by `\n`. Most commands are one-shot: the client writes
//! one [`Command`] and reads one [`CommandResult<T>`]. The streaming
//! commands ([`Command::Fetch`], [`Command::Download`], [`Command::Export`])
//! instead emit a sequence of [`StreamEvent`] frames — zero or more
//! `progress`, then one
//! terminal `okay`/`error` whose tags match [`CommandResult`].
//!
//! The schema is additive-friendly on the wire (an unknown command tag
//! is an error; unknown struct fields are ignored, new variants are new
//! tags), but the Rust types are plain —
//! the node crate constructs and exhaustively matches them, and all
//! crates in this workspace version in lockstep.

use std::path::PathBuf;

use cid::Cid;
use radicle::git::Oid;
use radicle::identity::RepoId;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cid::ArtifactKind;
use crate::keys::EndpointId;

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

/// How imported bytes are placed in the store.
///
/// A serde-friendly, project-stable representation suitable for the wire
/// protocol; the node maps it onto `iroh_blobs::api::blobs::ImportMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportMode {
    /// Copy bytes into the store. The source file can be moved or
    /// deleted afterwards without breaking seeding. Default for the
    /// node.
    Copy,
    /// Reference the source file in place. No bytes are copied. The
    /// caller is responsible for keeping the source path stable for as
    /// long as they want to seed the artifact; if the file is moved or
    /// deleted, fetches will fail.
    Reference,
}

/// A control-socket request.
///
/// Serialized as JSON with an internal `"command"` tag, kebab-case
/// variant names. Unit variants (`Status`, `Shutdown`) serialize to
/// `{"command":"status"}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Command {
    /// Cheap liveness probe: the node replies `{"okay":null}` and does no
    /// work. Used to tell a live owner from a stale socket file. Distinct
    /// from any network-level reachability check against a peer endpoint.
    Alive,
    /// Report node status.
    Status,
    /// Import bytes from `path`, verify against `cid`, register the
    /// `seeded/{rid}/{release}/{cid}` tag.
    Seed {
        /// Repository the artifact belongs to.
        rid: RepoId,
        /// Release the seeded tag is scoped to. Lets a CID shared by several
        /// releases be unseeded per release without dropping the others.
        release: Oid,
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
    /// Remove seeded tags for `cid`. Idempotent. `release: Some(id)` drops
    /// just that release's tag; `None` stops seeding the CID across every
    /// release of `rid`.
    Unseed {
        /// Repository the artifact belongs to.
        rid: RepoId,
        /// Release to stop seeding, or `None` for all releases of `rid`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        release: Option<Oid>,
        /// Content identifier to stop seeding.
        #[serde(with = "cid_string")]
        cid: Cid,
    },
    /// Whether `(rid, cid)` is currently seeded under any release.
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
    /// Fetch an artifact into the store: no-op if already complete locally,
    /// else download from `locations`, optionally tag as seeded. Does not
    /// write to disk — use [`Command::Download`] for that. Streaming —
    /// emits progress, then [`FetchReceipt`].
    Fetch {
        /// Repository the artifact belongs to (for the seeded tag).
        rid: RepoId,
        /// Release the seeded tag is scoped to. Required when `seed` is set;
        /// ignored otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        release: Option<Oid>,
        /// Expected content identifier; the blob kind is derived from it.
        #[serde(with = "cid_string")]
        cid: Cid,
        /// Resolved providers/URLs to try. Iroh providers are batched into
        /// one multi-provider download; URLs are tried in sequence.
        locations: Vec<FetchLocation>,
        /// Tag `seeded/{rid}/{release}/{cid}` after completion so the node
        /// serves it.
        seed: bool,
    },
    /// Download an artifact to disk: [`Command::Fetch`] into the store, then
    /// export to `dest`. Fast-path export if already local. Optionally tags
    /// as seeded. Streaming — emits progress, then [`DownloadReceipt`].
    Download {
        /// Repository the artifact belongs to (for the seeded tag).
        rid: RepoId,
        /// Release the seeded tag is scoped to. Required when `seed` is set;
        /// ignored otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        release: Option<Oid>,
        /// Expected content identifier; the blob kind is derived from it.
        #[serde(with = "cid_string")]
        cid: Cid,
        /// Resolved providers/URLs to try. Iroh providers are batched into
        /// one multi-provider download; URLs are tried in sequence.
        locations: Vec<FetchLocation>,
        /// Destination path (file for blobs, directory for collections).
        dest: PathBuf,
        /// Tag `seeded/{rid}/{release}/{cid}` after completion so the node
        /// serves it.
        seed: bool,
    },
    /// Ask the node to shut down gracefully.
    Shutdown,
}

/// A resolved place to fetch an artifact from.
///
/// Owned and serde-friendly, unlike the borrowed COB form (`(&Url, &Did)`).
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

/// Frame of a streaming response ([`Command::Fetch`], [`Command::Download`],
/// [`Command::Export`]).
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
/// An enum, not a struct, so Location-level events (which carry no byte
/// offset) and byte-movement events are modeled distinctly. The variants
/// map onto the iroh `DownloadProgressItem` kinds the download loop
/// already produces. `Export` only ever emits [`FetchProgress::Exporting`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum FetchProgress {
    /// Endpoint/relay setup, before any Location is tried.
    Connecting,
    /// Now attempting this Location.
    TryingLocation {
        /// Endpoint being tried.
        endpoint_id: EndpointId,
    },
    /// This Location failed; moving on to the next.
    LocationFailed {
        /// Endpoint that failed.
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
    /// A `Fetch`/`Download` was given no usable locations to try.
    NoLocations,
    /// Every provider/URL a `Fetch`/`Download` tried failed; `message`
    /// lists them.
    AllFailed,
    /// Bug or unhandled state inside the node.
    Internal,
}

/// Successful result of [`Command::Seed`].
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchReceipt {
    /// Echo of the requested repository.
    pub rid: RepoId,
    /// Echo of the fetched CID.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// Logical size now complete in the store, in bytes.
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

/// Terminal result of [`Command::Download`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadReceipt {
    /// Echo of the requested repository.
    pub rid: RepoId,
    /// Echo of the downloaded CID.
    #[serde(with = "cid_string")]
    pub cid: Cid,
    /// Where the bytes were written.
    pub dest: PathBuf,
    /// Logical size exported, in bytes.
    pub bytes: u64,
    /// `true` if the bytes were already local; no network was used.
    pub from_cache: bool,
    /// `true` if a `seeded/{rid}/{cid}` tag is now set.
    pub seeded: bool,
    /// Endpoint id the node serves on, as a canonical `radiroh://<base32>`
    /// URL. Present so the caller can write the `add_location` COB after a
    /// `seed: true` download. Mirrors [`SeedReceipt::endpoint_id`].
    pub endpoint_id: EndpointId,
}

/// Successful result of [`Command::Status`]. See the design doc for the
/// per-field source recipes; in v1 only the obvious fields are wired and
/// the rest default to zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    /// Endpoint id the node is serving on, as a canonical `radiroh://<base32>` URL.
    pub endpoint_id: EndpointId,
    /// Unix timestamp (seconds) when the node bound its socket.
    pub started_at_unix: i64,
    /// Aggregated tag-level stats.
    pub seeded: SeededStats,
    /// Connection counters derived from iroh's metrics.
    pub connections: ConnectionStats,
    /// Bytes-on-the-wire counters from iroh's socket metrics.
    pub traffic: TrafficStats,
    /// Home-relay connectivity and measured latency.
    pub relay: RelayStats,
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
}

/// Home-relay connectivity. The relay is how peers that can't holepunch a
/// direct path reach this node, so a disconnected relay means reduced
/// reachability even while the local socket is bound.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayStats {
    /// Per-home-relay status. Empty before a relay is selected, or when
    /// relays are disabled.
    pub relays: Vec<RelayHealth>,
    /// URL of the lowest-latency relay net_report would prefer, if a report
    /// has landed yet.
    pub preferred: Option<String>,
    /// A QAD (UDP) round trip completed over IPv4 — i.e. direct UDP works.
    pub udp_v4: bool,
    /// A QAD (UDP) round trip completed over IPv6.
    pub udp_v6: bool,
}

/// Connection status and measured latency of a single home relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayHealth {
    /// Relay URL.
    pub url: String,
    /// `true` when the endpoint currently holds a connection to the relay.
    pub connected: bool,
    /// Lowest round-trip latency measured by net_report, in milliseconds.
    /// `None` until a probe lands.
    pub latency_ms: Option<u64>,
    /// Most recent connection error when disconnected; `None` when connected
    /// or before any failure was observed.
    pub last_error: Option<String>,
}

/// Soft warnings surfaced in `Status`, rendered as advice to the user.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Warnings {
    /// Set when no home relay is connected; peers that can't holepunch may
    /// be unable to reach this node.
    pub relay_unreachable: bool,
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

    /// Release Oid (a COB ObjectId's git hash) used as a wire-snapshot fixture.
    const SAMPLE_RELEASE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn sample_release() -> Oid {
        Oid::from_str(SAMPLE_RELEASE).unwrap()
    }

    /// Real Blake3/raw Cid for wire snapshots — built from a pinned
    /// preimage so the multibase string stays stable across runs.
    fn sample_cid() -> Cid {
        let digest = blake3::hash(b"protocol-cid-sample");
        let mh =
            cid::multihash::Multihash::<64>::wrap(crate::cid::HASH_CODE_BLAKE3, digest.as_bytes())
                .unwrap();
        Cid::new_v1(crate::cid::RAW_CODEC, mh)
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
    fn wire_snapshot_command_alive() {
        let cmd = Command::Alive;
        let s = serde_json::to_string(&cmd).unwrap();
        assert_eq!(s, r#"{"command":"alive"}"#);
        let back: Command = serde_json::from_str(&s).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn wire_snapshot_command_seed() {
        let cid = sample_cid();
        let cmd = Command::Seed {
            rid: sample_rid(),
            release: sample_release(),
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
                "release": SAMPLE_RELEASE,
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
        // `release: None` (stop seeding the CID everywhere) omits the field.
        let unseed = Command::Unseed {
            rid: sample_rid(),
            release: None,
            cid,
        };
        assert_eq!(
            serde_json::to_value(&unseed).unwrap(),
            json!({"command":"unseed", "rid": SAMPLE_RID, "cid": cid.to_string()})
        );

        // `release: Some(..)` (one release) carries the id.
        let unseed_one = Command::Unseed {
            rid: sample_rid(),
            release: Some(sample_release()),
            cid,
        };
        assert_eq!(
            serde_json::to_value(&unseed_one).unwrap(),
            json!({"command":"unseed", "rid": SAMPLE_RID, "release": SAMPLE_RELEASE, "cid": cid.to_string()})
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
        iroh_base::SecretKey::from_bytes(&[7u8; 32]).public().into()
    }

    #[test]
    fn wire_snapshot_status_zeroed() {
        let endpoint_id = sample_endpoint_id();
        let st = Status {
            endpoint_id,
            started_at_unix: 0,
            seeded: SeededStats::default(),
            connections: ConnectionStats::default(),
            traffic: TrafficStats::default(),
            relay: RelayStats::default(),
            warnings: Warnings::default(),
        };
        assert_eq!(
            serde_json::to_value(&st).unwrap(),
            json!({
                "endpoint_id": endpoint_id.to_string(),
                "started_at_unix": 0,
                "seeded": {"count": 0, "bytes_logical": 0},
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
                "relay": {
                    "relays": [],
                    "preferred": null,
                    "udp_v4": false,
                    "udp_v6": false,
                },
                "warnings": {"relay_unreachable": false},
            })
        );
    }

    #[test]
    fn wire_snapshot_command_has_export_fetch_download() {
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

        // Fetch is store-only: no `dest` field on the wire.
        let fetch = Command::Fetch {
            rid: sample_rid(),
            release: Some(sample_release()),
            cid,
            locations: vec![
                FetchLocation::Iroh(endpoint_id),
                FetchLocation::Url(Url::parse("https://e.x/f").unwrap()),
            ],
            seed: true,
        };
        assert_eq!(
            serde_json::to_value(&fetch).unwrap(),
            json!({
                "command": "fetch",
                "rid": SAMPLE_RID,
                "release": SAMPLE_RELEASE,
                "cid": cid.to_string(),
                "locations": [
                    {"iroh": endpoint_id.to_string()},
                    {"url": "https://e.x/f"},
                ],
                "seed": true,
            })
        );
        let back: Command = serde_json::from_value(serde_json::to_value(&fetch).unwrap()).unwrap();
        assert_eq!(back, fetch);

        // Download adds `dest`; `seed: false` omits the release.
        let download = Command::Download {
            rid: sample_rid(),
            release: None,
            cid,
            locations: vec![FetchLocation::Iroh(endpoint_id)],
            dest: PathBuf::from("/tmp/out"),
            seed: false,
        };
        assert_eq!(
            serde_json::to_value(&download).unwrap(),
            json!({
                "command": "download",
                "rid": SAMPLE_RID,
                "cid": cid.to_string(),
                "locations": [{"iroh": endpoint_id.to_string()}],
                "dest": "/tmp/out",
                "seed": false,
            })
        );
        let back: Command =
            serde_json::from_value(serde_json::to_value(&download).unwrap()).unwrap();
        assert_eq!(back, download);
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
            message: "no locations".into(),
        });
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"error": {"code": "all-failed", "message": "no locations"}})
        );
    }

    #[test]
    fn wire_snapshot_fetch_progress() {
        let endpoint_id = sample_endpoint_id();
        let cases = [
            (FetchProgress::Connecting, json!({"kind": "connecting"})),
            (
                FetchProgress::TryingLocation { endpoint_id },
                json!({"kind": "trying-location", "endpoint_id": endpoint_id.to_string()}),
            ),
            (
                FetchProgress::LocationFailed { endpoint_id },
                json!({"kind": "location-failed", "endpoint_id": endpoint_id.to_string()}),
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

        // Fetch receipt has no `dest`; `bytes` is the logical store size.
        let fetch = FetchReceipt {
            rid: sample_rid(),
            cid,
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
                "bytes": 4096,
                "from_cache": false,
                "seeded": true,
                "endpoint_id": endpoint_id.to_string(),
            })
        );
        let back: FetchReceipt =
            serde_json::from_value(serde_json::to_value(&fetch).unwrap()).unwrap();
        assert_eq!(back, fetch);

        // Download receipt adds `dest`.
        let download = DownloadReceipt {
            rid: sample_rid(),
            cid,
            dest: PathBuf::from("/tmp/out"),
            bytes: 4096,
            from_cache: true,
            seeded: false,
            endpoint_id,
        };
        assert_eq!(
            serde_json::to_value(&download).unwrap(),
            json!({
                "rid": SAMPLE_RID,
                "cid": cid.to_string(),
                "dest": "/tmp/out",
                "bytes": 4096,
                "from_cache": true,
                "seeded": false,
                "endpoint_id": endpoint_id.to_string(),
            })
        );
        let back: DownloadReceipt =
            serde_json::from_value(serde_json::to_value(&download).unwrap()).unwrap();
        assert_eq!(back, download);
    }
}
