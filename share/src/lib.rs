//! Share artifacts from Radicle Artifact COBs.
//!
//! Provides a [`Fetcher`] trait for protocol-extensible artifact retrieval,
//! a built-in [`HttpFetcher`] implementation, iroh-blobs fetching via
//! [`fetch_iroh_blob`] and [`fetch_iroh_collection`], a [`download`]
//! function that tries locations in order with CID verification, and a
//! [`Server`] for serving blobs via iroh-blobs.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use std::{env, fmt};

pub use cid::Cid;
use cid::multihash::Multihash;
use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher};
use iroh::endpoint::presets::{self, Preset};
pub use url::Url;

/// Source: <https://github.com/multiformats/multicodec/blob/master/table.csv#L51>
const HASH_CODE_BLAKE3: u64 = 0x1e;
/// `blake3-hashseq` codec for iroh collections (a sequence of BLAKE3 hashes).
const BLAKE3_HASHSEQ_CODEC: u64 = 0x80;
/// Raw binary codec for single blobs.
const RAW_CODEC: u64 = 0x55;

const ENV_IROH_PRESET: &str = "RADWORKS_IROH_PRESET";
const ENV_RELAY_URL: &str = "RADWORKS_RELAY_URL";
const ENV_PKARR_URL: &str = "RADWORKS_PKARR_URL";
const ENV_DNS_DOMAIN: &str = "RADWORKS_DNS_DOMAIN";

// ---------------------------------------------------------------------------
// Endpoint preset
// ---------------------------------------------------------------------------

/// Iroh endpoint configuration.
///
/// Controls relay servers and discovery services for the endpoint.
/// Defaults to [`EndpointPreset::N0`] (n0's relay servers and DNS discovery).
///
/// Set `RADWORKS_IROH_PRESET=radworks` with `RADWORKS_RELAY_URL`,
/// `RADWORKS_PKARR_URL`, and `RADWORKS_DNS_DOMAIN` to use Radworks
/// infrastructure instead.
#[derive(Debug, Clone, Default)]
pub enum EndpointPreset {
    /// Use n0's relay servers and DNS discovery.
    #[default]
    N0,
    /// Use Radworks relay and discovery infrastructure.
    Radworks {
        relay_url: Url,
        pkarr_relay_url: Url,
        dns_origin_domain: String,
    },
}

impl EndpointPreset {
    /// Build an EndpointPreset from environment variables.
    ///
    /// - `RADWORKS_IROH_PRESET`: `n0` (default) or `radworks`
    /// - When `radworks`:
    ///   - `RADWORKS_RELAY_URL`: relay server URL (required)
    ///   - `RADWORKS_PKARR_URL`: pkarr relay URL (required)
    ///   - `RADWORKS_DNS_DOMAIN`: DNS origin domain (required)
    pub fn from_env() -> Result<Self, Error> {
        Self::parse(|key| env::var(key).ok())
    }

    /// Parse preset configuration from a key-value lookup function.
    fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        let preset = get(ENV_IROH_PRESET).unwrap_or_default();

        match preset.as_str() {
            "" | "n0" => Ok(Self::N0),
            "radworks" => {
                let relay_url = require_url(&get, ENV_RELAY_URL)?;
                let pkarr_relay_url = require_url(&get, ENV_PKARR_URL)?;
                let dns_origin_domain = require_val(&get, ENV_DNS_DOMAIN)?;

                Ok(Self::Radworks {
                    relay_url,
                    pkarr_relay_url,
                    dns_origin_domain,
                })
            }
            other => Err(Error::Iroh(format!(
                "unknown {ENV_IROH_PRESET} value: {other:?} (expected \"n0\" or \"radworks\")"
            ))),
        }
    }
}

impl fmt::Display for EndpointPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::N0 => write!(f, "n0"),
            Self::Radworks { relay_url, .. } => write!(f, "radworks (relay={relay_url})"),
        }
    }
}

fn require_val(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<String, Error> {
    get(key).ok_or_else(|| {
        Error::Iroh(format!(
            "{key} is required when {ENV_IROH_PRESET}=radworks"
        ))
    })
}

fn require_url(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Url, Error> {
    let val = require_val(get, key)?;
    val.parse::<Url>()
        .map_err(|e| Error::Iroh(format!("{key} is not a valid URL: {e}")))
}

impl Preset for EndpointPreset {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        match self {
            Self::N0 => presets::N0.apply(builder),
            Self::Radworks {
                relay_url,
                pkarr_relay_url,
                dns_origin_domain,
            } => builder
                .address_lookup(PkarrPublisher::builder(pkarr_relay_url))
                .address_lookup(DnsAddressLookup::builder(dns_origin_domain))
                .relay_mode(iroh::RelayMode::custom([relay_url.into()])),
        }
    }
}

// ---------------------------------------------------------------------------
// CID utilities
// ---------------------------------------------------------------------------

/// Whether the CID represents a single blob or a collection of named blobs.
pub fn artifact_kind(cid: &Cid) -> Result<ArtifactKind, Error> {
    match cid.codec() {
        RAW_CODEC => Ok(ArtifactKind::Blob),
        BLAKE3_HASHSEQ_CODEC => Ok(ArtifactKind::Collection),
        other => Err(Error::Cid(format!("unsupported CID codec: 0x{other:x}"))),
    }
}

/// The kind of artifact a CID points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A single blob (raw codec 0x55).
    Blob,
    /// A named collection of blobs (blake3-hashseq codec 0x80).
    Collection,
}

/// Create a CID from an iroh-blobs hash and artifact kind.
///
/// This is the inverse of [`cid_to_blake3_hash`]: given a BLAKE3 hash and the
/// appropriate codec, it produces a CIDv1.
pub fn blake3_hash_to_cid(hash: iroh_blobs::Hash, kind: ArtifactKind) -> Cid {
    let codec = match kind {
        ArtifactKind::Blob => RAW_CODEC,
        ArtifactKind::Collection => BLAKE3_HASHSEQ_CODEC,
    };
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, hash.as_bytes())
        .expect("BLAKE3 digest is always 32 bytes");
    Cid::new_v1(codec, mh)
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// A protocol handler that can fetch content from URLs with a given scheme.
pub trait Fetcher {
    /// URL schemes this fetcher handles (e.g. `["https", "http"]`).
    fn schemes(&self) -> &[&str];

    /// Fetch content at `url` into `dest`.
    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<(), Error>;
}

/// A location to try fetching an artifact from.
pub enum Location<'a> {
    /// Fetch from a URL using a registered [`Fetcher`].
    Url(&'a Url),
    /// Fetch via iroh-blobs. The BLAKE3 hash is extracted from the CID.
    Iroh(iroh::EndpointId),
}

/// HTTP(S) fetcher using ureq.
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn schemes(&self) -> &[&str] {
        &["https", "http"]
    }

    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<(), Error> {
        let resp = ureq::get(url.as_str())
            .call()
            .map_err(|e| Error::Http(e.to_string()))?;
        let mut reader = resp.into_body().into_reader();
        io::copy(&mut reader, dest).map_err(Error::Io)?;
        Ok(())
    }
}

/// Extract the BLAKE3 digest from a CID's multihash.
fn cid_to_blake3_hash(cid: &Cid) -> Result<iroh_blobs::Hash, Error> {
    let mh = cid.hash();
    if mh.code() != HASH_CODE_BLAKE3 {
        return Err(Error::Cid(format!(
            "expected BLAKE3 multihash (0x1e), got 0x{:x}",
            mh.code()
        )));
    }
    let digest: [u8; 32] = mh.digest().try_into().map_err(|_| {
        Error::Cid(format!(
            "expected 32-byte BLAKE3 digest, got {} bytes",
            mh.digest().len()
        ))
    })?;
    Ok(iroh_blobs::Hash::from_bytes(digest))
}

/// Connect to an iroh endpoint and return the connection.
async fn iroh_connect(
    endpoint_id: iroh::EndpointId,
    preset: EndpointPreset,
) -> Result<iroh::endpoint::Connection, Error> {
    let endpoint = iroh::Endpoint::builder(preset)
        .bind()
        .await
        .map_err(|e| Error::Iroh(format!("endpoint bind: {e}")))?;

    endpoint
        .connect(endpoint_id, iroh_blobs::ALPN)
        .await
        .map_err(|e| Error::Iroh(format!("connect: {e}")))
}

/// Fetch a single blob via iroh-blobs from the given endpoint.
///
/// The BLAKE3 hash is extracted from the CID's multihash. Since iroh-blobs
/// verifies BLAKE3 natively and the CID uses the same hash, transport
/// verification and content verification are the same operation.
pub fn fetch_iroh_blob(
    cid: &Cid,
    endpoint_id: iroh::EndpointId,
    dest: &mut dyn Write,
    preset: EndpointPreset,
) -> Result<(), Error> {
    let hash = cid_to_blake3_hash(cid)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Iroh(e.to_string()))?;
    rt.block_on(async {
        let connection = iroh_connect(endpoint_id, preset).await?;

        let progress = iroh_blobs::get::request::get_blob(connection, hash);
        let (bytes, _stats) = progress
            .bytes_and_stats()
            .await
            .map_err(|e| Error::Iroh(format!("download: {e}")))?;

        dest.write_all(&bytes).map_err(Error::Io)?;
        Ok(())
    })
}

/// Fetch an iroh-blobs collection and write each entry as a file under `dest_dir`.
///
/// The CID must use the `blake3-hashseq` codec (0x80). Each entry in the
/// collection is written to `dest_dir/<name>`.
pub fn fetch_iroh_collection(
    cid: &Cid,
    endpoint_id: iroh::EndpointId,
    dest_dir: &Path,
    preset: EndpointPreset,
) -> Result<(), Error> {
    let hash = cid_to_blake3_hash(cid)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Iroh(e.to_string()))?;
    rt.block_on(async {
        let connection = iroh_connect(endpoint_id, preset).await?;

        // Request the full collection (hashseq + all children).
        let request = iroh_blobs::protocol::GetRequest::all(hash);
        let at_start = iroh_blobs::get::fsm::start(connection, request, Default::default());
        let at_connected = at_start
            .next()
            .await
            .map_err(|e| Error::Iroh(format!("connect: {e}")))?;

        let iroh_blobs::get::fsm::ConnectedNext::StartRoot(start) =
            at_connected
                .next()
                .await
                .map_err(|e| Error::Iroh(format!("start root: {e}")))?
        else {
            return Err(Error::Iroh("expected start root".into()));
        };

        let (collection, children, _stats) =
            iroh_blobs::format::collection::Collection::read_fsm_all(start)
                .await
                .map_err(|e| Error::Iroh(format!("read collection: {e}")))?;

        std::fs::create_dir_all(dest_dir).map_err(Error::Io)?;
        write_collection(dest_dir, &collection, &children)?;

        Ok(())
    })
}

/// Write collection entries to disk.
fn write_collection(
    dest_dir: &Path,
    collection: &iroh_blobs::format::collection::Collection,
    children: &BTreeMap<u64, bytes::Bytes>,
) -> Result<(), Error> {
    for (i, (name, _hash)) in collection.iter().enumerate() {
        let data = children.get(&(i as u64)).ok_or_else(|| {
            Error::Iroh(format!("missing data for collection entry '{name}'"))
        })?;
        let path = dest_dir.join(name);
        // Create parent dirs for nested entries like "subdir/file.txt"
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&path, data).map_err(Error::Io)?;
    }
    Ok(())
}

/// Verify that `data` matches the expected CID (blake3, raw codec 0x55).
pub fn verify_cid(data: &[u8], expected: &Cid) -> Result<(), Error> {
    let digest = blake3::hash(data);
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes())
        .map_err(|e| Error::Cid(format!("multihash wrap: {e}")))?;
    let actual = Cid::new_v1(RAW_CODEC, mh);

    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// Try each location in order until one succeeds. Always verify CID.
///
/// Only supports single-blob artifacts (raw codec). For collections, use
/// [`download_collection`].
pub fn download(
    locations: &[Location],
    expected_cid: &Cid,
    dest: &mut dyn Write,
    fetchers: &[Box<dyn Fetcher>],
    preset: &EndpointPreset,
) -> Result<(), Error> {
    if locations.is_empty() {
        return Err(Error::NoLocations);
    }

    let mut errors = Vec::new();

    for location in locations {
        let mut buf = Vec::new();

        let result = match location {
            Location::Url(url) => {
                let scheme = url.scheme();
                match fetchers.iter().find(|f| f.schemes().contains(&scheme)) {
                    Some(fetcher) => fetcher.fetch(url, &mut buf),
                    None => {
                        errors.push(Error::UnsupportedScheme(scheme.to_string()));
                        continue;
                    }
                }
            }
            Location::Iroh(endpoint_id) => {
                fetch_iroh_blob(expected_cid, *endpoint_id, &mut buf, preset.clone())
            }
        };

        match result {
            Ok(()) => {
                // Always verify CID. For iroh this is redundant (same BLAKE3 hash)
                // but cheap and provides defense in depth.
                if let Err(e) = verify_cid(&buf, expected_cid) {
                    errors.push(e);
                    continue;
                }
                dest.write_all(&buf).map_err(Error::Io)?;
                return Ok(());
            }
            Err(e) => {
                errors.push(e);
                continue;
            }
        }
    }

    Err(Error::AllFailed(errors))
}

/// Try each iroh location until one succeeds at fetching a collection.
///
/// Only iroh locations support collections; URL locations are skipped.
pub fn download_collection(
    locations: &[Location],
    expected_cid: &Cid,
    dest_dir: &Path,
    preset: &EndpointPreset,
) -> Result<(), Error> {
    if locations.is_empty() {
        return Err(Error::NoLocations);
    }

    let mut errors = Vec::new();

    for location in locations {
        match location {
            Location::Url(_) => {
                // HTTP doesn't support collection fetching (yet).
                continue;
            }
            Location::Iroh(endpoint_id) => {
                match fetch_iroh_collection(expected_cid, *endpoint_id, dest_dir, preset.clone()) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Err(Error::NoLocations)
    } else {
        Err(Error::AllFailed(errors))
    }
}

/// Returns the default set of URL-based fetchers.
pub fn default_fetchers() -> Vec<Box<dyn Fetcher>> {
    vec![Box::new(HttpFetcher)]
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// An iroh-blobs server that serves content to peers.
///
/// Created via [`Server::start`] with an Ed25519 secret key. The endpoint ID
/// is derived from the key, so peers that know the corresponding public key
/// (or DID) can connect directly.
pub struct Server {
    router: iroh::protocol::Router,
    blobs: iroh_blobs::BlobsProtocol,
}

impl Server {
    /// Start an iroh-blobs server bound to the given identity.
    ///
    /// Uses the provided [`EndpointPreset`] for relay and discovery config.
    /// Waits for the relay connection before returning.
    pub async fn start(
        secret_key: iroh::SecretKey,
        preset: EndpointPreset,
    ) -> Result<Self, Error> {
        let store = iroh_blobs::store::mem::MemStore::new();

        let endpoint = iroh::Endpoint::builder(preset)
            .secret_key(secret_key)
            .alpns(vec![iroh_blobs::ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| Error::Serve(format!("endpoint bind: {e}")))?;

        // Wait for relay so our address is published and discoverable.
        let _ = endpoint.online().await;

        let blobs = iroh_blobs::BlobsProtocol::new(&store, None);
        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, blobs.clone())
            .spawn();

        Ok(Self { router, blobs })
    }

    /// Get a handle to the blob store for adding content.
    pub fn store(&self) -> &iroh_blobs::api::Store {
        self.blobs.store()
    }

    /// Get the iroh endpoint (useful for reading the endpoint ID).
    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
    }

    /// Gracefully shut down the server.
    pub async fn shutdown(self) -> Result<(), Error> {
        tokio::time::timeout(Duration::from_secs(2), self.router.shutdown())
            .await
            .map_err(|_| Error::Serve("shutdown timed out".into()))?
            .map_err(|e| Error::Serve(format!("shutdown: {e}")))
    }
}

/// Add a file to the blob store and verify it matches the expected CID.
pub async fn add_blob(
    store: &iroh_blobs::api::Store,
    path: &Path,
    expected: &Cid,
) -> Result<(), Error> {
    let tag = store
        .add_path(path)
        .temp_tag()
        .await
        .map_err(|e| Error::Serve(format!("add blob: {e}")))?;

    let actual_cid = blake3_hash_to_cid(tag.hash(), ArtifactKind::Blob);
    if actual_cid != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual_cid.to_string(),
        });
    }
    Ok(())
}

/// Add a directory as a named collection and verify it matches the expected CID.
///
/// Each file in the directory becomes a collection entry with its relative path
/// as the name. The directory is walked recursively.
pub async fn add_collection(
    store: &iroh_blobs::api::Store,
    dir: &Path,
    expected: &Cid,
) -> Result<(), Error> {
    let mut entries = Vec::new();

    // Walk the directory and add each file to the store.
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let read_dir = std::fs::read_dir(&current).map_err(Error::Io)?;
        for entry in read_dir {
            let entry = entry.map_err(Error::Io)?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let tag = store
                    .add_path(&path)
                    .temp_tag()
                    .await
                    .map_err(|e| Error::Serve(format!("add file: {e}")))?;

                // Use the relative path from the root dir as the entry name.
                let name = path
                    .strip_prefix(dir)
                    .expect("path is under dir")
                    .to_string_lossy()
                    .into_owned();

                entries.push((name, tag.hash()));
            }
        }
    }

    // Sort entries by name for deterministic ordering.
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let collection = iroh_blobs::format::collection::Collection::from_iter(entries);
    let tag = collection
        .store(store)
        .await
        .map_err(|e| Error::Serve(format!("store collection: {e}")))?;

    let actual_cid = blake3_hash_to_cid(tag.hash(), ArtifactKind::Collection);
    if actual_cid != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual_cid.to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    #[error("HTTP fetch failed: {0}")]
    Http(String),

    #[error("iroh fetch failed: {0}")]
    Iroh(String),

    #[error("serve error: {0}")]
    Serve(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("CID mismatch: expected {expected}, got {actual}")]
    CidMismatch { expected: String, actual: String },

    #[error("CID error: {0}")]
    Cid(String),

    #[error("no locations registered for this artifact")]
    NoLocations,

    #[error("all fetch attempts failed")]
    AllFailed(Vec<Error>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a BLAKE3 CID for the given data (single blob).
    fn blob_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(RAW_CODEC, mh)
    }

    /// Create a BLAKE3 CID with the hashseq codec.
    fn collection_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(BLAKE3_HASHSEQ_CODEC, mh)
    }

    #[test]
    fn verify_cid_matches() {
        let data = b"hello world";
        let cid = blob_cid(data);
        assert!(verify_cid(data, &cid).is_ok());
    }

    #[test]
    fn verify_cid_mismatch() {
        let data = b"hello world";
        let wrong_cid = blob_cid(b"wrong");
        assert!(matches!(
            verify_cid(data, &wrong_cid),
            Err(Error::CidMismatch { .. })
        ));
    }

    #[test]
    fn download_no_locations() {
        let cid = blob_cid(b"test");
        let mut buf = Vec::new();
        let preset = EndpointPreset::default();
        let result = download(&[], &cid, &mut buf, &default_fetchers(), &preset);
        assert!(matches!(result, Err(Error::NoLocations)));
    }

    #[test]
    fn download_unsupported_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let cid = blob_cid(b"test");
        let mut buf = Vec::new();
        let preset = EndpointPreset::default();
        let result = download(
            &[Location::Url(&url)],
            &cid,
            &mut buf,
            &default_fetchers(),
            &preset,
        );
        assert!(matches!(result, Err(Error::AllFailed(_))));
    }

    #[test]
    fn cid_to_blake3_hash_roundtrip() {
        let data = b"test data";
        let expected_hash = iroh_blobs::Hash::new(data);
        let cid = blob_cid(data);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, expected_hash);
    }

    #[test]
    fn cid_to_blake3_hash_rejects_sha256() {
        let digest = [0u8; 32];
        let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
        let cid = Cid::new_v1(RAW_CODEC, mh);
        assert!(matches!(
            cid_to_blake3_hash(&cid),
            Err(Error::Cid(_))
        ));
    }

    #[test]
    fn artifact_kind_blob() {
        let cid = blob_cid(b"test");
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Blob);
    }

    #[test]
    fn artifact_kind_collection() {
        let cid = collection_cid(b"test");
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Collection);
    }

    #[test]
    fn artifact_kind_unknown_codec() {
        let digest = blake3::hash(b"test");
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        let cid = Cid::new_v1(0x99, mh);
        assert!(matches!(artifact_kind(&cid), Err(Error::Cid(_))));
    }

    #[test]
    fn cid_to_blake3_works_with_hashseq_codec() {
        // cid_to_blake3_hash should work regardless of codec
        let data = b"test data";
        let expected_hash = iroh_blobs::Hash::new(data);
        let cid = collection_cid(data);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, expected_hash);
    }

    #[test]
    fn blake3_hash_to_cid_blob_roundtrip() {
        let data = b"test data";
        let hash = iroh_blobs::Hash::new(data);
        let cid = blake3_hash_to_cid(hash, ArtifactKind::Blob);
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Blob);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, hash);
    }

    #[test]
    fn blake3_hash_to_cid_collection_roundtrip() {
        let data = b"test data";
        let hash = iroh_blobs::Hash::new(data);
        let cid = blake3_hash_to_cid(hash, ArtifactKind::Collection);
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Collection);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, hash);
    }

    #[test]
    fn endpoint_preset_default_is_n0() {
        let preset = EndpointPreset::default();
        assert!(matches!(preset, EndpointPreset::N0));
    }

    #[test]
    fn endpoint_preset_parse_empty_is_n0() {
        let preset = EndpointPreset::parse(|_| None).unwrap();
        assert!(matches!(preset, EndpointPreset::N0));
    }

    #[test]
    fn endpoint_preset_parse_radworks() {
        let preset = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("radworks".into()),
            "RADWORKS_RELAY_URL" => Some("https://relay.example.com".into()),
            "RADWORKS_PKARR_URL" => Some("https://pkarr.example.com".into()),
            "RADWORKS_DNS_DOMAIN" => Some("example.com".into()),
            _ => None,
        })
        .unwrap();
        assert!(matches!(preset, EndpointPreset::Radworks { .. }));
    }

    #[test]
    fn endpoint_preset_parse_radworks_missing_url() {
        let result = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("radworks".into()),
            _ => None,
        });
        assert!(matches!(result, Err(Error::Iroh(_))));
    }

    #[test]
    fn endpoint_preset_parse_unknown_value() {
        let result = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("foo".into()),
            _ => None,
        });
        assert!(matches!(result, Err(Error::Iroh(_))));
    }
}