//! Fetch artifacts from Radicle Artifact COBs.
//!
//! Provides a [`Fetcher`] trait for protocol-extensible artifact retrieval,
//! a built-in [`HttpFetcher`] implementation, iroh-blobs fetching via
//! [`fetch_iroh_blob`] and [`fetch_iroh_collection`], and a [`download`]
//! function that tries locations in order with CID verification.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

pub use cid::Cid;
use cid::multihash::Multihash;
pub use url::Url;

/// Source: <https://github.com/multiformats/multicodec/blob/master/table.csv#L51>
const HASH_CODE_BLAKE3: u64 = 0x1e;
/// `blake3-hashseq` codec for iroh collections (a sequence of BLAKE3 hashes).
const BLAKE3_HASHSEQ_CODEC: u64 = 0x80;
/// Raw binary codec for single blobs.
const RAW_CODEC: u64 = 0x55;

/// Whether the CID represents a single blob or a collection of named blobs.
pub fn artifact_kind(cid: &Cid) -> Result<ArtifactKind, FetchError> {
    match cid.codec() {
        RAW_CODEC => Ok(ArtifactKind::Blob),
        BLAKE3_HASHSEQ_CODEC => Ok(ArtifactKind::Collection),
        other => Err(FetchError::Cid(format!("unsupported CID codec: 0x{other:x}"))),
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

/// A protocol handler that can fetch content from URLs with a given scheme.
pub trait Fetcher {
    /// URL schemes this fetcher handles (e.g. `["https", "http"]`).
    fn schemes(&self) -> &[&str];

    /// Fetch content at `url` into `dest`.
    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<(), FetchError>;
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

    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<(), FetchError> {
        let resp = ureq::get(url.as_str())
            .call()
            .map_err(|e| FetchError::Http(e.to_string()))?;
        let mut reader = resp.into_body().into_reader();
        io::copy(&mut reader, dest).map_err(FetchError::Io)?;
        Ok(())
    }
}

/// Extract the BLAKE3 digest from a CID's multihash.
fn cid_to_blake3_hash(cid: &Cid) -> Result<iroh_blobs::Hash, FetchError> {
    let mh = cid.hash();
    if mh.code() != HASH_CODE_BLAKE3 {
        return Err(FetchError::Cid(format!(
            "expected BLAKE3 multihash (0x1e), got 0x{:x}",
            mh.code()
        )));
    }
    let digest: [u8; 32] = mh.digest().try_into().map_err(|_| {
        FetchError::Cid(format!(
            "expected 32-byte BLAKE3 digest, got {} bytes",
            mh.digest().len()
        ))
    })?;
    Ok(iroh_blobs::Hash::from_bytes(digest))
}

/// Connect to an iroh endpoint and return the connection.
async fn iroh_connect(
    endpoint_id: iroh::EndpointId,
) -> Result<iroh::endpoint::Connection, FetchError> {
    let endpoint = iroh::Endpoint::empty_builder()
        .relay_mode(iroh::RelayMode::Default)
        .address_lookup(iroh::address_lookup::PkarrResolver::n0_dns())
        .bind()
        .await
        .map_err(|e| FetchError::Iroh(format!("endpoint bind: {e}")))?;

    endpoint
        .connect(endpoint_id, iroh_blobs::ALPN)
        .await
        .map_err(|e| FetchError::Iroh(format!("connect: {e}")))
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
) -> Result<(), FetchError> {
    let hash = cid_to_blake3_hash(cid)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| FetchError::Iroh(e.to_string()))?;
    rt.block_on(async {
        let connection = iroh_connect(endpoint_id).await?;

        let progress = iroh_blobs::get::request::get_blob(connection, hash);
        let (bytes, _stats) = progress
            .bytes_and_stats()
            .await
            .map_err(|e| FetchError::Iroh(format!("download: {e}")))?;

        dest.write_all(&bytes).map_err(FetchError::Io)?;
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
) -> Result<(), FetchError> {
    let hash = cid_to_blake3_hash(cid)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| FetchError::Iroh(e.to_string()))?;
    rt.block_on(async {
        let connection = iroh_connect(endpoint_id).await?;

        // Request the full collection (hashseq + all children).
        let request = iroh_blobs::protocol::GetRequest::all(hash);
        let at_start = iroh_blobs::get::fsm::start(connection, request, Default::default());
        let at_connected = at_start
            .next()
            .await
            .map_err(|e| FetchError::Iroh(format!("connect: {e}")))?;

        let iroh_blobs::get::fsm::ConnectedNext::StartRoot(start) =
            at_connected
                .next()
                .await
                .map_err(|e| FetchError::Iroh(format!("start root: {e}")))?
        else {
            return Err(FetchError::Iroh("expected start root".into()));
        };

        let (collection, children, _stats) =
            iroh_blobs::format::collection::Collection::read_fsm_all(start)
                .await
                .map_err(|e| FetchError::Iroh(format!("read collection: {e}")))?;

        std::fs::create_dir_all(dest_dir).map_err(FetchError::Io)?;
        write_collection(dest_dir, &collection, &children)?;

        Ok(())
    })
}

/// Write collection entries to disk.
fn write_collection(
    dest_dir: &Path,
    collection: &iroh_blobs::format::collection::Collection,
    children: &BTreeMap<u64, bytes::Bytes>,
) -> Result<(), FetchError> {
    for (i, (name, _hash)) in collection.iter().enumerate() {
        let data = children.get(&(i as u64)).ok_or_else(|| {
            FetchError::Iroh(format!("missing data for collection entry '{name}'"))
        })?;
        let path = dest_dir.join(name);
        // Create parent dirs for nested entries like "subdir/file.txt"
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(FetchError::Io)?;
        }
        std::fs::write(&path, data).map_err(FetchError::Io)?;
    }
    Ok(())
}

/// Verify that `data` matches the expected CID (blake3, raw codec 0x55).
pub fn verify_cid(data: &[u8], expected: &Cid) -> Result<(), FetchError> {
    let digest = blake3::hash(data);
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes())
        .map_err(|e| FetchError::Cid(format!("multihash wrap: {e}")))?;
    let actual = Cid::new_v1(RAW_CODEC, mh);

    if actual != *expected {
        return Err(FetchError::CidMismatch {
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
) -> Result<(), FetchError> {
    if locations.is_empty() {
        return Err(FetchError::NoLocations);
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
                        errors.push(FetchError::UnsupportedScheme(scheme.to_string()));
                        continue;
                    }
                }
            }
            Location::Iroh(endpoint_id) => fetch_iroh_blob(expected_cid, *endpoint_id, &mut buf),
        };

        match result {
            Ok(()) => {
                // Always verify CID. For iroh this is redundant (same BLAKE3 hash)
                // but cheap and provides defense in depth.
                if let Err(e) = verify_cid(&buf, expected_cid) {
                    errors.push(e);
                    continue;
                }
                dest.write_all(&buf).map_err(FetchError::Io)?;
                return Ok(());
            }
            Err(e) => {
                errors.push(e);
                continue;
            }
        }
    }

    Err(FetchError::AllFailed(errors))
}

/// Try each iroh location until one succeeds at fetching a collection.
///
/// Only iroh locations support collections; URL locations are skipped.
pub fn download_collection(
    locations: &[Location],
    expected_cid: &Cid,
    dest_dir: &Path,
) -> Result<(), FetchError> {
    if locations.is_empty() {
        return Err(FetchError::NoLocations);
    }

    let mut errors = Vec::new();

    for location in locations {
        match location {
            Location::Url(_) => {
                // HTTP doesn't support collection fetching (yet).
                continue;
            }
            Location::Iroh(endpoint_id) => {
                match fetch_iroh_collection(expected_cid, *endpoint_id, dest_dir) {
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
        Err(FetchError::NoLocations)
    } else {
        Err(FetchError::AllFailed(errors))
    }
}

/// Returns the default set of URL-based fetchers.
pub fn default_fetchers() -> Vec<Box<dyn Fetcher>> {
    vec![Box::new(HttpFetcher)]
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    #[error("HTTP fetch failed: {0}")]
    Http(String),

    #[error("iroh fetch failed: {0}")]
    Iroh(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("CID mismatch: expected {expected}, got {actual}")]
    CidMismatch { expected: String, actual: String },

    #[error("CID error: {0}")]
    Cid(String),

    #[error("no locations registered for this artifact")]
    NoLocations,

    #[error("all fetch attempts failed")]
    AllFailed(Vec<FetchError>),
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
            Err(FetchError::CidMismatch { .. })
        ));
    }

    #[test]
    fn download_no_locations() {
        let cid = blob_cid(b"test");
        let mut buf = Vec::new();
        let result = download(&[], &cid, &mut buf, &default_fetchers());
        assert!(matches!(result, Err(FetchError::NoLocations)));
    }

    #[test]
    fn download_unsupported_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let cid = blob_cid(b"test");
        let mut buf = Vec::new();
        let result = download(
            &[Location::Url(&url)],
            &cid,
            &mut buf,
            &default_fetchers(),
        );
        assert!(matches!(result, Err(FetchError::AllFailed(_))));
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
            Err(FetchError::Cid(_))
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
        assert!(matches!(artifact_kind(&cid), Err(FetchError::Cid(_))));
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
}
