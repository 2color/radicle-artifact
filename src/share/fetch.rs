//! Artifact fetching from registered locations.
//!
//! Provides a [`Fetcher`] trait for protocol-extensible artifact retrieval,
//! a built-in [`HttpFetcher`], and iroh-blobs fetching via [`fetch_iroh_blob`]
//! and [`fetch_iroh_collection`].
//!
//! Iroh downloads stream to disk via [`iroh_blobs::store::fs::FsStore`],
//! avoiding in-memory buffering. Progress is reported via [`indicatif`].

use std::io::{self, BufWriter, Write};
use std::path::Path;

use cid::Cid;
use indicatif::{ProgressBar, ProgressStyle};
use iroh_blobs::api::remote::GetProgressItem;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobFormat, HashAndFormat};
use n0_future::StreamExt;
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

// ---------------------------------------------------------------------------
// Progress bar helpers (following sendme conventions)
// ---------------------------------------------------------------------------

fn make_download_progress() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(std::time::Duration::from_millis(250));
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} Downloading {bytes} ({binary_bytes_per_sec})",
        )
        .unwrap(),
    );
    pb
}

/// Drive a progress bar from a channel of byte offsets.
///
/// Runs as a spawned task so the download stream and the UI update are
/// decoupled — this supports future multi-connection downloads where
/// multiple providers feed the same store.
async fn show_download_progress(pb: ProgressBar, mut recv: tokio::sync::mpsc::Receiver<u64>) {
    while let Some(offset) = recv.recv().await {
        pb.set_position(offset);
    }
    pb.finish_and_clear();
}

// ---------------------------------------------------------------------------
// Iroh internals
// ---------------------------------------------------------------------------

/// Ephemeral store directory for an iroh download, following sendme's convention.
/// Placed in the current working directory and cleaned up after the download.
fn iroh_store_dir(hash: &iroh_blobs::Hash) -> std::path::PathBuf {
    let hex = hash.to_hex();
    std::path::PathBuf::from(format!(".rad-artifact-fetch-{hex}"))
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

/// Download content into a [`FsStore`] via `execute_get`, showing progress.
///
/// This is the shared core used by both [`fetch_iroh_blob`] and
/// [`fetch_iroh_collection`]. The caller is responsible for exporting
/// from the store after this returns.
async fn fetch_iroh_to_store(
    hash_and_format: HashAndFormat,
    endpoint_id: iroh::EndpointId,
    db: &FsStore,
    preset: EndpointPreset,
) -> Result<iroh::Endpoint, Error> {
    let (endpoint, connection) = iroh_connect(endpoint_id, preset).await?;
    let local = db
        .remote()
        .local(hash_and_format)
        .await
        .map_err(|e| Error::Iroh(format!("local info: {e}")))?;

    if !local.is_complete() {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let pb = make_download_progress();
        let task = tokio::spawn(show_download_progress(pb, rx));

        let get = db.remote().execute_get(connection, local.missing());
        let mut stream = get.stream();
        while let Some(item) = stream.next().await {
            match item {
                GetProgressItem::Progress(offset) => {
                    tx.send(offset).await.ok();
                }
                GetProgressItem::Done(_stats) => break,
                GetProgressItem::Error(cause) => {
                    return Err(Error::Iroh(format!("download: {cause}")));
                }
            }
        }
        drop(tx);
        task.await.ok();
    }

    Ok(endpoint)
}

// ---------------------------------------------------------------------------
// Public iroh fetch API
// ---------------------------------------------------------------------------

/// Fetch a single blob via iroh-blobs from the given endpoint.
///
/// Downloads into a temporary [`FsStore`], then exports the blob to `dest`.
/// No in-memory buffering — data streams through the store to disk.
/// BLAKE3 verification happens during transfer (iroh-blobs verifies natively).
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
    dest: &Path,
    preset: EndpointPreset,
) -> Result<(), Error> {
    let hash = cid_util::cid_to_blake3_hash(cid)?;
    let hash_and_format = HashAndFormat {
        hash,
        format: BlobFormat::Raw,
    };

    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Iroh(e.to_string()))?;
    rt.block_on(async {
        let store_dir = iroh_store_dir(&hash);
        std::fs::create_dir_all(&store_dir).map_err(Error::Io)?;
        let db = FsStore::load(&store_dir)
            .await
            .map_err(|e| Error::Iroh(format!("store load: {e}")))?;

        let result = async {
            let endpoint =
                fetch_iroh_to_store(hash_and_format, endpoint_id, &db, preset).await?;
            db.blobs()
                .export(hash, dest)
                .await
                .map_err(|e| Error::Iroh(format!("export: {e}")))?;
            endpoint.close().await;
            Ok(())
        }
        .await;

        // Always shut down the store before removing the directory.
        db.shutdown().await.ok();
        std::fs::remove_dir_all(&store_dir).ok();
        result
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
    let hash_and_format = HashAndFormat {
        hash,
        format: BlobFormat::HashSeq,
    };

    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Iroh(e.to_string()))?;
    rt.block_on(async {
        let store_dir = iroh_store_dir(&hash);
        std::fs::create_dir_all(&store_dir).map_err(Error::Io)?;
        let db = FsStore::load(&store_dir)
            .await
            .map_err(|e| Error::Iroh(format!("store load: {e}")))?;

        let result = async {
            let endpoint =
                fetch_iroh_to_store(hash_and_format, endpoint_id, &db, preset).await?;

            let collection = Collection::load(hash, db.as_ref())
                .await
                .map_err(|e| Error::Iroh(format!("load collection: {e}")))?;

            std::fs::create_dir_all(dest_dir).map_err(Error::Io)?;
            for (name, entry_hash) in collection.iter() {
                let target = dest_dir.join(name);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(Error::Io)?;
                }
                db.blobs()
                    .export(*entry_hash, &target)
                    .await
                    .map_err(|e| Error::Iroh(format!("export '{name}': {e}")))?;
            }

            endpoint.close().await;
            Ok(())
        }
        .await;

        db.shutdown().await.ok();
        std::fs::remove_dir_all(&store_dir).ok();
        result
    })
}

// ---------------------------------------------------------------------------
// Location-fallback orchestrators
// ---------------------------------------------------------------------------

/// Try each location in order until one succeeds.
///
/// Only supports single-blob artifacts (raw codec). For collections, use
/// [`download_collection`].
///
/// - **HTTP locations:** stream to a temp file, verify CID from disk, rename
///   to `dest`. No in-memory buffering.
/// - **Iroh locations:** download via [`FsStore`] and export to `dest`.
///   BLAKE3 is verified during transfer, so no separate CID check is needed.
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
    dest: &Path,
    fetchers: &[Box<dyn Fetcher>],
    preset: &EndpointPreset,
) -> Result<(), Error> {
    if locations.is_empty() {
        return Err(Error::NoLocations);
    }

    let mut errors = Vec::new();

    for location in locations {
        let result = match location {
            Location::Url(url) => {
                let scheme = url.scheme();
                match fetchers.iter().find(|f| f.schemes().contains(&scheme)) {
                    Some(fetcher) => download_http(fetcher.as_ref(), url, expected_cid, dest),
                    None => {
                        errors.push(Error::UnsupportedScheme(scheme.to_string()));
                        continue;
                    }
                }
            }
            Location::Iroh(endpoint_id) => {
                fetch_iroh_blob(expected_cid, *endpoint_id, dest, preset.clone())
            }
        };

        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                errors.push(e);
                continue;
            }
        }
    }

    Err(Error::AllFailed(errors))
}

/// Fetch via HTTP to a partial file, verify CID from disk, then rename to dest.
fn download_http(
    fetcher: &dyn Fetcher,
    url: &Url,
    expected_cid: &Cid,
    dest: &Path,
) -> Result<(), Error> {
    // Write to a .partial file next to the destination, then rename on success.
    let partial = dest.with_extension("partial");
    let file = std::fs::File::create(&partial).map_err(Error::Io)?;
    let mut writer = BufWriter::new(file);
    let fetch_result = fetcher.fetch(url, &mut writer);

    // Clean up the partial file on any error.
    if let Err(e) = fetch_result {
        std::fs::remove_file(&partial).ok();
        return Err(e);
    }
    writer.flush().map_err(Error::Io)?;
    drop(writer);

    // Verify CID by hashing the partial file from disk (no memory buffering).
    if let Err(e) = cid_util::verify_cid_file(&partial, expected_cid) {
        std::fs::remove_file(&partial).ok();
        return Err(e);
    }

    // Rename to final destination.
    std::fs::rename(&partial, dest).map_err(Error::Io)?;
    Ok(())
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
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let preset = EndpointPreset::default();
        let result = download(&[], &cid, &dest, &default_fetchers(), &preset);
        assert!(matches!(result, Err(Error::NoLocations)));
    }

    #[test]
    fn download_unsupported_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let cid = blob_cid(b"test");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let preset = EndpointPreset::default();
        let result = download(
            &[Location::Url(&url)],
            &cid,
            &dest,
            &default_fetchers(),
            &preset,
        );
        assert!(matches!(result, Err(Error::AllFailed(_))));
    }
}
