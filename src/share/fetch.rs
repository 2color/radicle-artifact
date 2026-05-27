//! Artifact fetching from registered locations.
//!
//! Provides HTTP and iroh-blobs fetching for artifacts. HTTP uses [`ureq`];
//! iroh downloads stream to disk via [`iroh_blobs::store::fs::FsStore`],
//! avoiding in-memory buffering. Progress is reported via [`indicatif`].
//!
//! Both transports bound how long an unreachable provider can tie up a
//! fetch. HTTP sets connect and receive-response timeouts on the
//! `ureq::Agent`; iroh sets a connect timeout on the connection pool and
//! an idle-progress timeout around the Downloader stream. Mid-body HTTP
//! stalls are intentionally not bounded — ureq 3.3 only offers a
//! total-body timeout, which would break large artifact downloads.
//!
//! Per-provider iroh causes are not preserved (the [`iroh_blobs`]
//! downloader drops them on `ProviderFailed`); set
//! `RUST_LOG=iroh_blobs=debug` in the node to surface them
//! (subscriber is installed in `rad-artifact node start --foreground`).

use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use cid::Cid;
use indicatif::{ProgressBar, ProgressStyle};
use iroh_blobs::api::downloader::{DownloadProgressItem, Downloader, Shuffled};
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::util::connection_pool::Options as PoolOptions;
use iroh_blobs::{BlobFormat, HashAndFormat};
use n0_future::StreamExt;
use url::Url;

use super::cid_utils::{self, ArtifactKind};
use super::iroh::EndpointPreset;
use super::keys::EndpointId;
use super::Error;

/// Per-provider connect bound. A provider that cannot establish a usable
/// connection (HTTP TCP handshake or iroh QUIC+relay path) within this
/// window is abandoned so the next one is tried. More generous than
/// iroh's 1s default to accommodate slower relay paths without giving up
/// on reachable but cold providers.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle (no-progress) bound. Reset only on `Progress` / `PartComplete`
/// events — control events like `TryProvider` or `ProviderFailed` do not
/// count as progress, so a cascade of dead providers can't keep the
/// download alive past this window.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A location to try fetching an artifact from.
pub enum Location<'a> {
    /// Fetch from an HTTP(S) URL.
    Url(&'a Url),
    /// Fetch via iroh-blobs. The BLAKE3 hash is extracted from the CID.
    Iroh(EndpointId),
}

/// Build a ureq agent with connect and response-header timeouts.
///
/// No total-body timeout: large artifact downloads must be allowed to
/// stream for as long as they make progress, and ureq 3.3 does not offer
/// a per-read socket timeout that would bound only stalls.
fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(CONNECT_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

/// Fetch HTTP content at `url` into `dest` using a pre-configured agent.
fn fetch_http(agent: &ureq::Agent, url: &Url, dest: &mut dyn Write) -> Result<(), Error> {
    let resp = agent
        .get(url.as_str())
        .call()
        .map_err(|e| Error::Http(e.to_string()))?;
    let mut reader = resp.into_body().into_reader();
    io::copy(&mut reader, dest).map_err(Error::Io)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Progress bar
// ---------------------------------------------------------------------------

fn make_download_progress() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(250));
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} Downloading {bytes} ({binary_bytes_per_sec})",
        )
        .unwrap(),
    );
    pb
}

// ---------------------------------------------------------------------------
// Iroh internals
// ---------------------------------------------------------------------------

/// Ephemeral store directory for an iroh download, following sendme's
/// convention. Placed in the current working directory and cleaned up
/// after the download.
fn iroh_store_dir(hash: &iroh_blobs::Hash) -> std::path::PathBuf {
    let hex = hash.to_hex();
    std::path::PathBuf::from(format!(".rad-artifact-fetch-{hex}"))
}

/// Map the CID codec to the iroh-blobs on-wire format.
fn blob_format_for_cid(cid: &Cid) -> Result<BlobFormat, Error> {
    match cid_utils::artifact_kind(cid)? {
        ArtifactKind::Blob => Ok(BlobFormat::Raw),
        ArtifactKind::Collection => Ok(BlobFormat::HashSeq),
    }
}

/// Run a multi-provider iroh download
///
/// One `Endpoint` and one [`Downloader`] are reused across providers, so we pay the
/// endpoint-bind cost once and partial progress persists across providers
/// (the second provider only supplies what the first didn't deliver).
///
/// Returns per-provider errors on failure. `DownloadProgressItem::ProviderFailed`
/// intentionally drops the underlying cause — the errors vector therefore
/// records only which provider failed plus the final stream-level cause if
/// the download terminates fatally.
async fn iroh_fetch_to_store(
    hash_and_format: HashAndFormat,
    providers: Vec<EndpointId>,
    preset: EndpointPreset,
    db: &FsStore,
) -> Result<(), Vec<Error>> {
    // Convert to iroh's bare type at the iroh-blobs API boundary.
    let providers: Vec<iroh::EndpointId> =
        providers.into_iter().map(EndpointId::into_inner).collect();
    let endpoint = match iroh::Endpoint::builder(preset).bind().await {
        Ok(ep) => ep,
        Err(e) => return Err(vec![Error::Iroh(format!("endpoint bind: {e}"))]),
    };

    let pool_opts = PoolOptions {
        connect_timeout: CONNECT_TIMEOUT,
        ..PoolOptions::default()
    };
    let downloader = Downloader::new_with_opts(db.as_ref(), &endpoint, pool_opts);

    let pb: ProgressBar = make_download_progress();

    // below we shuffle to avoid overloading a single provider, but the
    // trade-off is that we may lose freshness ordering if providers are prioritized by the caller.
    let progress = downloader.download(hash_and_format, Shuffled::new(providers));
    let mut stream = match progress.stream().await {
        Ok(s) => s,
        Err(e) => {
            pb.finish_and_clear();
            endpoint.close().await;
            return Err(vec![Error::Iroh(format!("downloader rpc: {e}"))]);
        }
    };

    let mut errors: Vec<Error> = Vec::new();
    let mut fatal: Option<Error> = None;
    // Idle deadline is only bumped by data-movement events (`Progress`,
    // `PartComplete`). Control events from the downloader — `TryProvider`,
    // `ProviderFailed` — do not reset it, so a stream of dead providers
    // can't silently extend the wait beyond IDLE_TIMEOUT of real progress.
    let mut deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => {
                fatal = Some(Error::Iroh(format!(
                    "no progress for {}s",
                    IDLE_TIMEOUT.as_secs()
                )));
                break;
            }
            Ok(None) => break, // Download completed!
            Ok(Some(item)) => match item {
                DownloadProgressItem::TryProvider { id, .. } => {
                    eprintln!("Trying iroh provider {}...", EndpointId::from(id));
                }
                DownloadProgressItem::ProviderFailed { id, .. } => {
                    errors.push(Error::Iroh(format!(
                        "provider {}: download failed",
                        EndpointId::from(id)
                    )));
                }
                DownloadProgressItem::Progress(offset) => {
                    pb.set_position(offset);
                    deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                }
                DownloadProgressItem::PartComplete { .. } => {
                    deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                }
                DownloadProgressItem::DownloadError => {
                    fatal = Some(Error::Iroh("download error".into()));
                    break;
                }
                DownloadProgressItem::Error(cause) => {
                    fatal = Some(Error::Iroh(format!("{cause}")));
                    break;
                }
            },
        }
    }
    pb.finish_and_clear();
    endpoint.close().await;

    // Trust the store: if nothing is missing, we have the content — even
    // if individual providers emitted `ProviderFailed` along the way. If
    // the completeness check itself errors, treat the attempt as failed
    // and surface the cause so it isn't silently swallowed.
    let done = match db.remote().local(hash_and_format).await {
        Ok(local) => local.is_complete(),
        Err(e) => {
            errors.push(Error::Iroh(format!("store completeness check: {e}")));
            false
        }
    };
    if done {
        Ok(())
    } else {
        if let Some(e) = fatal {
            errors.push(e);
        }
        Err(errors)
    }
}

/// What to write to disk after the iroh download completes.
enum ExportTarget {
    /// Export the single raw blob to this file path.
    Blob(std::path::PathBuf),
    /// Load the hashseq collection and export each entry under this dir.
    Collection(std::path::PathBuf),
}

/// Synchronous wrapper around one iroh download attempt.
///
/// Creates an ephemeral tokio runtime and on-disk store, runs the
/// multi-provider download via [`iroh_fetch_to_store`], then performs the
/// blob-or-collection export step described by `target`. The store and
/// runtime are torn down before returning.
fn run_iroh_attempt(
    cid: &Cid,
    providers: Vec<EndpointId>,
    preset: &EndpointPreset,
    target: ExportTarget,
) -> Result<(), Vec<Error>> {
    let hash = cid_utils::cid_to_blake3_hash(cid).map_err(|e| vec![e])?;
    // iroh-blobs export requires absolute paths, so resolve before moving
    // into the async block.
    let target = match target {
        ExportTarget::Blob(p) => {
            ExportTarget::Blob(std::path::absolute(&p).map_err(|e| vec![Error::Io(e)])?)
        }
        ExportTarget::Collection(p) => {
            ExportTarget::Collection(std::path::absolute(&p).map_err(|e| vec![Error::Io(e)])?)
        }
    };
    let format = match &target {
        ExportTarget::Blob(_) => BlobFormat::Raw,
        ExportTarget::Collection(_) => BlobFormat::HashSeq,
    };
    let hash_and_format = HashAndFormat { hash, format };

    let rt =
        tokio::runtime::Runtime::new().map_err(|e| vec![Error::Iroh(format!("runtime: {e}"))])?;
    rt.block_on(async move {
        let (db, store_dir) = open_ephemeral_store(&hash).await?;
        let result = async {
            iroh_fetch_to_store(hash_and_format, providers, preset.clone(), &db).await?;
            match target {
                ExportTarget::Blob(dest) => {
                    db.blobs()
                        .export(hash, dest)
                        .await
                        .map_err(|e| vec![Error::Iroh(format!("export: {e}"))])?;
                }
                ExportTarget::Collection(dest_dir) => {
                    let collection = Collection::load(hash, db.as_ref())
                        .await
                        .map_err(|e| vec![Error::Iroh(format!("load collection: {e}"))])?;
                    std::fs::create_dir_all(&dest_dir).map_err(|e| vec![Error::Io(e)])?;
                    for (name, entry_hash) in collection.iter() {
                        let target = dest_dir.join(name);
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent).map_err(|e| vec![Error::Io(e)])?;
                        }
                        db.blobs()
                            .export(*entry_hash, &target)
                            .await
                            .map_err(|e| vec![Error::Iroh(format!("export '{name}': {e}"))])?;
                    }
                }
            }
            Ok::<(), Vec<Error>>(())
        }
        .await;
        db.shutdown().await.ok();
        std::fs::remove_dir_all(&store_dir).ok();
        result
    })
}

/// Create the ephemeral on-disk store used for the duration of one fetch.
async fn open_ephemeral_store(
    hash: &iroh_blobs::Hash,
) -> Result<(FsStore, std::path::PathBuf), Vec<Error>> {
    let store_dir = iroh_store_dir(hash);
    std::fs::create_dir_all(&store_dir).map_err(|e| vec![Error::Io(e)])?;
    let db = FsStore::load(&store_dir)
        .await
        .map_err(|e| vec![Error::Iroh(format!("store load: {e}"))])?;
    Ok((db, store_dir))
}

// ---------------------------------------------------------------------------
// Location-fallback orchestrators
// ---------------------------------------------------------------------------

/// Split locations into iroh providers and HTTP URLs.
///
/// The iroh side is batched into a single multi-provider download attempt
/// (one shared endpoint, one connection pool); the URL side stays as a
/// sequential per-location fallback. Ordering from the input is lost — all
/// iroh providers are tried together before any URL is tried.
fn partition_locations<'a>(locations: &'a [Location<'a>]) -> (Vec<EndpointId>, Vec<&'a Url>) {
    let mut iroh = Vec::new();
    let mut urls = Vec::new();
    for loc in locations {
        match loc {
            Location::Iroh(id) => iroh.push(*id),
            Location::Url(url) => urls.push(*url),
        }
    }
    (iroh, urls)
}

/// Download a single-blob artifact. Raw-codec CID required; collections
/// go through [`download_collection`].
///
/// Strategy: all iroh providers are attempted together through a single
/// shared endpoint (partial progress reuses across providers), then each
/// HTTP URL is tried in sequence.
///
/// # Sync/async design
///
/// This is a synchronous, CLI-oriented orchestrator. HTTP uses blocking
/// `ureq` with connect/read timeouts; iroh creates an ephemeral runtime,
/// endpoint, and store for the duration of the call.
///
/// Async callers with a persistent iroh endpoint and store should build
/// their own fetch logic using `iroh_blobs::api::downloader::Downloader`
/// directly, plus an async HTTP client for URL sources.
pub fn download(
    locations: &[Location],
    expected_cid: &Cid,
    dest: &Path,
    preset: &EndpointPreset,
) -> Result<(), Error> {
    if locations.is_empty() {
        return Err(Error::NoLocations);
    }
    // Defensive: `download` is the blob entry point. A hashseq CID indicates
    // caller misuse and should fail cleanly rather than producing garbage.
    if !matches!(blob_format_for_cid(expected_cid)?, BlobFormat::Raw) {
        return Err(Error::Cid(
            "download() requires a raw-blob CID; use download_collection".into(),
        ));
    }

    let (iroh_ids, urls) = partition_locations(locations);
    let mut errors: Vec<Error> = Vec::new();

    if !iroh_ids.is_empty() {
        let target = ExportTarget::Blob(dest.to_path_buf());
        match run_iroh_attempt(expected_cid, iroh_ids, preset, target) {
            Ok(()) => return Ok(()),
            Err(mut per_provider) => errors.append(&mut per_provider),
        }
    }

    if !urls.is_empty() {
        let agent = http_agent();
        for url in urls {
            match url.scheme() {
                "https" | "http" => match download_http(&agent, url, expected_cid, dest) {
                    Ok(()) => return Ok(()),
                    Err(e) => errors.push(e),
                },
                scheme => errors.push(Error::UnsupportedScheme(scheme.to_string())),
            }
        }
    }

    Err(Error::AllFailed(errors))
}

/// Fetch via HTTP to a partial file, verify CID from disk, then rename to dest.
fn download_http(
    agent: &ureq::Agent,
    url: &Url,
    expected_cid: &Cid,
    dest: &Path,
) -> Result<(), Error> {
    // Write to a .partial file next to the destination, then rename on success.
    let partial = dest.with_extension("partial");
    let file = std::fs::File::create(&partial).map_err(Error::Io)?;
    let mut writer = BufWriter::new(file);
    let fetch_result = fetch_http(agent, url, &mut writer);

    // Clean up the partial file on any error.
    if let Err(e) = fetch_result {
        std::fs::remove_file(&partial).ok();
        return Err(e);
    }
    writer.flush().map_err(Error::Io)?;
    drop(writer);

    // Verify CID by hashing the partial file from disk (no memory buffering).
    if let Err(e) = cid_utils::verify_cid_file(&partial, expected_cid) {
        std::fs::remove_file(&partial).ok();
        return Err(e);
    }

    // Rename to final destination.
    std::fs::rename(&partial, dest).map_err(Error::Io)?;
    Ok(())
}

/// Download a hashseq-collection artifact, writing each entry under `dest_dir`.
///
/// Only iroh providers are used — HTTP collection fetch is not implemented.
/// All iroh providers run through one shared endpoint.
pub fn download_collection(
    locations: &[Location],
    expected_cid: &Cid,
    dest_dir: &Path,
    preset: &EndpointPreset,
) -> Result<(), Error> {
    if locations.is_empty() {
        return Err(Error::NoLocations);
    }
    if !matches!(blob_format_for_cid(expected_cid)?, BlobFormat::HashSeq) {
        return Err(Error::Cid(
            "download_collection() requires a hashseq CID; use download".into(),
        ));
    }

    let (iroh_ids, urls) = partition_locations(locations);
    if iroh_ids.is_empty() {
        // HTTP collection fetch is unsupported, so URL-only inputs cannot be
        // served. Surface each URL as HttpCollectionUnsupported so the caller
        // sees *why* no attempt was made — not a misleading "unsupported
        // scheme: https" or generic "no locations".
        let errors = urls
            .into_iter()
            .map(|u| Error::HttpCollectionUnsupported(u.to_string()))
            .collect();
        return Err(Error::AllFailed(errors));
    }

    let target = ExportTarget::Collection(dest_dir.to_path_buf());
    match run_iroh_attempt(expected_cid, iroh_ids, preset, target) {
        Ok(()) => Ok(()),
        Err(errors) => Err(Error::AllFailed(errors)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh =
            cid::multihash::Multihash::<64>::wrap(cid_utils::HASH_CODE_BLAKE3, digest.as_bytes())
                .unwrap();
        Cid::new_v1(cid_utils::RAW_CODEC, mh)
    }

    fn collection_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh =
            cid::multihash::Multihash::<64>::wrap(cid_utils::HASH_CODE_BLAKE3, digest.as_bytes())
                .unwrap();
        Cid::new_v1(cid_utils::BLAKE3_HASHSEQ_CODEC, mh)
    }

    #[test]
    fn download_no_locations() {
        let cid = blob_cid(b"test");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let preset = EndpointPreset::default();
        let result = download(&[], &cid, &dest, &preset);
        assert!(matches!(result, Err(Error::NoLocations)));
    }

    #[test]
    fn download_unsupported_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let cid = blob_cid(b"test");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let preset = EndpointPreset::default();
        let result = download(&[Location::Url(&url)], &cid, &dest, &preset);
        assert!(matches!(result, Err(Error::AllFailed(_))));
    }

    #[test]
    fn partition_locations_splits_iroh_and_url() {
        let url_a = Url::parse("https://a.example/x").unwrap();
        let url_b = Url::parse("https://b.example/y").unwrap();
        // EndpointId is a PublicKey; derive two distinct ones from fixed
        // Ed25519 secret-key bytes so the test is deterministic.
        let id1: EndpointId = iroh::SecretKey::from_bytes(&[1u8; 32]).public().into();
        let id2: EndpointId = iroh::SecretKey::from_bytes(&[2u8; 32]).public().into();
        let locs = [
            Location::Url(&url_a),
            Location::Iroh(id1),
            Location::Url(&url_b),
            Location::Iroh(id2),
        ];
        let (iroh, urls) = partition_locations(&locs);
        assert_eq!(iroh, vec![id1, id2]);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].as_str(), "https://a.example/x");
        assert_eq!(urls[1].as_str(), "https://b.example/y");
    }

    // Regression guard for the timeout wiring: a URL pointing at an
    // unroutable RFC5737 address should fail via the ureq connect timeout
    // rather than hanging. We allow generous slack — CI may be slow — but
    // still bound the test well under "forever".
    #[test]
    fn download_http_connect_times_out_fast() {
        let url = Url::parse("http://192.0.2.1:1/not-there").unwrap();
        let cid = blob_cid(b"test");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let preset = EndpointPreset::default();
        let start = std::time::Instant::now();
        let result = download(&[Location::Url(&url)], &cid, &dest, &preset);
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(Error::AllFailed(_))));
        // Must complete within CONNECT_TIMEOUT + generous slack.
        assert!(
            elapsed < CONNECT_TIMEOUT + Duration::from_secs(10),
            "expected fast timeout, took {elapsed:?}"
        );
    }

    // URL-only locations for a collection CID cannot be served (HTTP
    // collection fetch is unsupported). The caller should see each URL
    // reported as HttpCollectionUnsupported, not a misleading
    // UnsupportedScheme(https) or a generic NoLocations.
    #[test]
    fn download_collection_url_only_reports_unsupported() {
        let cid = collection_cid(b"test");
        let url = Url::parse("https://example.com/x").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let preset = EndpointPreset::default();
        let result = download_collection(&[Location::Url(&url)], &cid, dir.path(), &preset);
        match result {
            Err(Error::AllFailed(errors)) => {
                assert_eq!(errors.len(), 1);
                assert!(matches!(errors[0], Error::HttpCollectionUnsupported(_)));
            }
            other => panic!("expected AllFailed with HttpCollectionUnsupported, got {other:?}"),
        }
    }
}
