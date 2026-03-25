//! Fetch artifacts from Radicle Artifact COBs.
//!
//! Provides a [`Fetcher`] trait for protocol-extensible artifact retrieval,
//! built-in [`HttpFetcher`] and [`IrohBlobFetcher`] implementations, and a
//! [`download`] function that tries URLs in order with CID verification.

use std::io::{self, Write};

pub use cid::Cid;
use cid::multihash::Multihash;
pub use url::Url;

/// Whether the caller must verify the downloaded content against the CID.
///
/// Fetchers that provide their own integrity verification (e.g. iroh-blobs
/// with BLAKE3) return `No`. HTTP fetchers return `Yes` since the transport
/// doesn't guarantee content integrity.
///
/// NOTE: iroh-blobs verifies the BLAKE3 hash from the URL, but that is a
/// different hash than the artifact's CID (SHA2-256). A `radworks://` URL
/// pointing at the wrong blob will pass iroh's check but won't match the
/// CID. Callers should consider always verifying the CID regardless.
pub enum NeedsVerification {
    Yes,
    No,
}

/// A protocol handler that can fetch content from URLs with a given scheme.
pub trait Fetcher {
    /// URL schemes this fetcher handles (e.g. `["https", "http"]`).
    fn schemes(&self) -> &[&str];

    /// Fetch content at `url` into `dest`.
    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<NeedsVerification, FetchError>;
}

/// Iroh blob fetcher for `radworks://` URLs.
///
/// URL format: `radworks://<blake3-hash>?hint=<endpoint-id>`
/// where the BLAKE3 hash is the content address (authority) and the `hint`
/// query parameter identifies the node to fetch from.
///
/// Integrity is verified by iroh-blobs natively (BLAKE3), so no CID
/// verification is needed by the caller.
pub struct IrohBlobFetcher;

impl IrohBlobFetcher {
    /// Parse a `radworks://` URL into (blake3 hash, endpoint ID).
    fn parse_url(url: &Url) -> Result<(iroh_blobs::Hash, iroh::EndpointId), FetchError> {
        let hash_str = url
            .host_str()
            .ok_or_else(|| FetchError::InvalidRadworksUrl("missing BLAKE3 hash".into()))?;

        let hash = hash_str
            .parse::<iroh_blobs::Hash>()
            .map_err(|e| FetchError::InvalidRadworksUrl(format!("invalid BLAKE3 hash: {e}")))?;

        // Extract endpoint ID from ?hint= query parameter
        let endpoint_str = url
            .query_pairs()
            .find(|(key, _)| key == "hint")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| FetchError::InvalidRadworksUrl("missing ?hint= endpoint ID".into()))?;

        let endpoint_id = endpoint_str
            .parse::<iroh::EndpointId>()
            .map_err(|e| FetchError::InvalidRadworksUrl(format!("invalid endpoint ID: {e}")))?;

        Ok((hash, endpoint_id))
    }
}

impl Fetcher for IrohBlobFetcher {
    fn schemes(&self) -> &[&str] {
        &["radworks"]
    }

    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<NeedsVerification, FetchError> {
        let (hash, endpoint_id) = Self::parse_url(url)?;

        let rt = tokio::runtime::Runtime::new().map_err(|e| FetchError::Iroh(e.to_string()))?;
        rt.block_on(async {
            let endpoint = iroh::Endpoint::empty_builder()
                .relay_mode(iroh::RelayMode::Default)
                .address_lookup(iroh::address_lookup::PkarrResolver::n0_dns())
                .bind()
                .await
                .map_err(|e| FetchError::Iroh(format!("endpoint bind: {e}")))?;

            let connection = endpoint
                .connect(endpoint_id, iroh_blobs::ALPN)
                .await
                .map_err(|e| FetchError::Iroh(format!("connect: {e}")))?;

            let progress = iroh_blobs::get::request::get_blob(connection, hash);
            let (bytes, _stats) = progress
                .bytes_and_stats()
                .await
                .map_err(|e| FetchError::Iroh(format!("download: {e}")))?;

            dest.write_all(&bytes).map_err(FetchError::Io)?;
            Ok(NeedsVerification::No)
        })
    }
}

/// HTTP(S) fetcher using ureq.
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn schemes(&self) -> &[&str] {
        &["https", "http"]
    }

    fn fetch(&self, url: &Url, dest: &mut dyn Write) -> Result<NeedsVerification, FetchError> {
        let resp = ureq::get(url.as_str())
            .call()
            .map_err(|e| FetchError::Http(e.to_string()))?;
        let mut reader = resp.into_body().into_reader();
        io::copy(&mut reader, dest).map_err(FetchError::Io)?;
        Ok(NeedsVerification::Yes)
    }
}

/// Verify that `data` matches the expected CID (sha2-256, raw codec 0x55).
pub fn verify_cid(data: &[u8], expected: &Cid) -> Result<(), FetchError> {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(data);
    // 0x12 = sha2-256 multihash code
    let mh = Multihash::<64>::wrap(0x12, &digest)
        .map_err(|e| FetchError::Cid(format!("multihash wrap: {e}")))?;
    // 0x55 = raw codec
    let actual = Cid::new_v1(0x55, mh);

    if actual != *expected {
        return Err(FetchError::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// Try each URL in order until one succeeds. Verify CID when the fetcher
/// indicates it's needed.
pub fn download(
    urls: &[&Url],
    expected_cid: &Cid,
    dest: &mut dyn Write,
    fetchers: &[Box<dyn Fetcher>],
) -> Result<(), FetchError> {
    if urls.is_empty() {
        return Err(FetchError::NoLocations);
    }

    let mut errors = Vec::new();

    for url in urls {
        let scheme = url.scheme();
        let fetcher = match fetchers.iter().find(|f| f.schemes().contains(&scheme)) {
            Some(f) => f,
            None => {
                errors.push(FetchError::UnsupportedScheme(scheme.to_string()));
                continue;
            }
        };

        let mut buf = Vec::new();
        match fetcher.fetch(url, &mut buf) {
            Ok(needs_verify) => {
                if matches!(needs_verify, NeedsVerification::Yes) {
                    if let Err(e) = verify_cid(&buf, expected_cid) {
                        errors.push(e);
                        continue;
                    }
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

/// Returns the default set of fetchers (HTTP + iroh-blobs).
pub fn default_fetchers() -> Vec<Box<dyn Fetcher>> {
    vec![Box::new(HttpFetcher), Box::new(IrohBlobFetcher)]
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

    #[error("invalid radworks:// URL: {0}")]
    InvalidRadworksUrl(String),

    #[error("no locations registered for this artifact")]
    NoLocations,

    #[error("all fetch attempts failed")]
    AllFailed(Vec<FetchError>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_cid_matches() {
        let data = b"hello world";
        // Compute expected CID for "hello world"
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(data);
        let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
        let cid = Cid::new_v1(0x55, mh);

        assert!(verify_cid(data, &cid).is_ok());
    }

    #[test]
    fn verify_cid_mismatch() {
        let data = b"hello world";
        // Wrong CID (different data)
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(b"wrong");
        let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
        let wrong_cid = Cid::new_v1(0x55, mh);

        assert!(matches!(
            verify_cid(data, &wrong_cid),
            Err(FetchError::CidMismatch { .. })
        ));
    }

    #[test]
    fn download_no_locations() {
        let cid = {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(b"test");
            let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
            Cid::new_v1(0x55, mh)
        };
        let mut buf = Vec::new();
        let result = download(&[], &cid, &mut buf, &default_fetchers());
        assert!(matches!(result, Err(FetchError::NoLocations)));
    }

    #[test]
    fn parse_radworks_url() {
        // 64-char hex = valid BLAKE3 hash
        let hash = "a".repeat(64);
        // Valid ed25519 public key (64 hex chars)
        let node = "b".repeat(64);
        let url = Url::parse(&format!("radworks://{hash}?hint={node}")).unwrap();
        let result = IrohBlobFetcher::parse_url(&url);
        assert!(result.is_ok(), "parse failed: {result:?}");
    }

    #[test]
    fn parse_radworks_url_missing_hint() {
        let hash = "a".repeat(64);
        let url = Url::parse(&format!("radworks://{hash}")).unwrap();
        let result = IrohBlobFetcher::parse_url(&url);
        assert!(matches!(result, Err(FetchError::InvalidRadworksUrl(_))));
    }

    #[test]
    fn download_unsupported_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let cid = {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(b"test");
            let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
            Cid::new_v1(0x55, mh)
        };
        let mut buf = Vec::new();
        let result = download(&[&url], &cid, &mut buf, &default_fetchers());
        assert!(matches!(result, Err(FetchError::AllFailed(_))));
    }
}
