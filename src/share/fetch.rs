//! Artifact fetching from registered locations.
//!
//! Provides a [`Fetcher`] trait for protocol-extensible artifact retrieval,
//! a built-in [`HttpFetcher`], and iroh-blobs fetching via [`fetch_iroh_blob`]
//! and [`fetch_iroh_collection`].

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use cid::Cid;
use url::Url;

use super::cid as cid_util;
use super::endpoint::EndpointPreset;
use super::Error;

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

/// Connect to an iroh endpoint and return the connection.
///
/// The endpoint is returned alongside the connection because the
/// connection cannot outlive the endpoint that created it.
async fn iroh_connect(
    endpoint_id: iroh::EndpointId,
    preset: EndpointPreset,
) -> Result<(iroh::Endpoint, iroh::endpoint::Connection), Error> {
    let endpoint = iroh::Endpoint::builder(preset)
        .bind()
        .await
        .map_err(|e| Error::Iroh(format!("endpoint bind: {e}")))?;

    let connection = endpoint
        .connect(endpoint_id, iroh_blobs::ALPN)
        .await
        .map_err(|e| Error::Iroh(format!("connect: {e}")))?;
    Ok((endpoint, connection))
}

/// Fetch a single blob via iroh-blobs from the given endpoint.
///
/// The BLAKE3 hash is extracted from the CID's multihash. Since iroh-blobs
/// verifies BLAKE3 natively and the CID uses the same hash, transport
/// verification and content verification are the same operation.
///
/// # Sync/async design
///
/// This function is **synchronous** — it creates an ephemeral `tokio::Runtime`,
/// connects to the remote endpoint, downloads the blob, and tears everything
/// down before returning. This is intentional for CLI use where each fetch is
/// a one-shot operation with no pre-existing async context.
///
/// **Callers already in an async context** (e.g. a Tauri app with a long-lived
/// iroh endpoint and persistent blob store) should NOT use this function.
/// Instead, use `iroh_blobs::api::downloader::Downloader` directly — it
/// integrates with an existing endpoint and store, supports multi-provider
/// downloads, and avoids the overhead of creating a throwaway runtime and
/// endpoint per fetch.
///
/// The same applies to [`fetch_iroh_collection`].
pub fn fetch_iroh_blob(
    cid: &Cid,
    endpoint_id: iroh::EndpointId,
    dest: &mut dyn Write,
    preset: EndpointPreset,
) -> Result<(), Error> {
    let hash = cid_util::cid_to_blake3_hash(cid)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Iroh(e.to_string()))?;
    rt.block_on(async {
        let (endpoint, connection) = iroh_connect(endpoint_id, preset).await?;

        let progress = iroh_blobs::get::request::get_blob(connection, hash);
        let (bytes, _stats) = progress
            .bytes_and_stats()
            .await
            .map_err(|e| Error::Iroh(format!("download: {e}")))?;

        dest.write_all(&bytes).map_err(Error::Io)?;
        endpoint.close().await;
        Ok(())
    })
}

/// Fetch an iroh-blobs collection and write each entry as a file under `dest_dir`.
///
/// The CID must use the `blake3-hashseq` codec (0x80). Each entry in the
/// collection is written to `dest_dir/<name>`.
///
/// # Sync/async design
///
/// Like [`fetch_iroh_blob`], this function is synchronous and creates an
/// ephemeral runtime and endpoint per call. This is suited for CLI tools
/// that perform isolated, one-shot fetches.
///
/// Async callers with a long-lived iroh endpoint should use
/// `iroh_blobs::api::downloader::Downloader` instead, which downloads into
/// an existing blob store without ephemeral runtime overhead. After
/// downloading, use [`iroh_blobs::format::collection::Collection::load`] to
/// read the collection from the store and extract entries.
pub fn fetch_iroh_collection(
    cid: &Cid,
    endpoint_id: iroh::EndpointId,
    dest_dir: &Path,
    preset: EndpointPreset,
) -> Result<(), Error> {
    let hash = cid_util::cid_to_blake3_hash(cid)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Iroh(e.to_string()))?;
    rt.block_on(async {
        let (endpoint, connection) = iroh_connect(endpoint_id, preset).await?;

        let request = iroh_blobs::protocol::GetRequest::all(hash);
        let at_start = iroh_blobs::get::fsm::start(connection, request, Default::default());
        let at_connected = at_start
            .next()
            .await
            .map_err(|e| Error::Iroh(format!("connect: {e}")))?;

        let iroh_blobs::get::fsm::ConnectedNext::StartRoot(start) = at_connected
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

        endpoint.close().await;
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
        let data = children
            .get(&(i as u64))
            .ok_or_else(|| Error::Iroh(format!("missing data for collection entry '{name}'")))?;
        let path = dest_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&path, data).map_err(Error::Io)?;
    }
    Ok(())
}

/// Try each location in order until one succeeds. Always verify CID.
///
/// Only supports single-blob artifacts (raw codec). For collections, use
/// [`download_collection`].
///
/// # Sync/async design
///
/// This is a synchronous, location-fallback orchestrator built for CLI use.
/// HTTP locations use `ureq` (blocking); iroh locations create an ephemeral
/// runtime per attempt via [`fetch_iroh_blob`].
///
/// Async callers with a persistent iroh endpoint and store should build
/// their own fetch logic using `iroh_blobs::api::downloader::Downloader`
/// for iroh sources and an async HTTP client for URL sources.
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
                if let Err(e) = cid_util::verify_cid(&buf, expected_cid) {
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
///
/// # Sync/async design
///
/// Same as [`download`] — synchronous, ephemeral runtime per attempt.
/// Async callers should use `Downloader` directly.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn blob_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh =
            cid::multihash::Multihash::<64>::wrap(cid_util::HASH_CODE_BLAKE3, digest.as_bytes())
                .unwrap();
        Cid::new_v1(cid_util::RAW_CODEC, mh)
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
}
