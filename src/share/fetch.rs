//! Reusable fetch + export core for the node.
//!
//! These are the building blocks the node's `Fetch`/`Export` handlers call
//! against their persistent [`FsStore`] and shared
//! [`Downloader`](iroh_blobs::api::downloader::Downloader): a multi-provider
//! iroh download ([`download_iroh_to_store`]), an HTTP-into-store download
//! ([`http_to_store`], so HTTP blobs become seedable), and atomic export
//! helpers ([`export_blob_to`], [`export_collection_to`]). None of them own
//! the endpoint, store, or runtime — the caller supplies those.
//!
//! Progress is reported through a [`FetchProgress`] callback so the caller
//! decides how to surface it (the CLI drives a progress bar; the node
//! forwards frames over the control socket).
//!
//! Both transports bound how long an unreachable provider can tie up a
//! fetch. HTTP sets connect and receive-response timeouts on the
//! `ureq::Agent`; iroh applies a connect timeout on the connection pool
//! ([`pool_options`]) and an idle-progress timeout around the Downloader
//! stream. Mid-body HTTP stalls are intentionally not bounded — ureq 3.3
//! only offers a total-body timeout, which would break large downloads.
//!
//! Per-provider iroh causes are not preserved (the [`iroh_blobs`]
//! downloader drops them on `ProviderFailed`); set
//! `RUST_LOG=iroh_blobs=debug` in the node to surface them
//! (subscriber is installed in `rad-artifact node start --foreground`).

use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use cid::Cid;
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode as IrohImportMode};
use iroh_blobs::api::downloader::{DownloadProgressItem, Downloader, Shuffled};
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::util::connection_pool::Options as PoolOptions;
use iroh_blobs::{BlobFormat, Hash, HashAndFormat};
use n0_future::StreamExt;
use url::Url;

use super::cid_utils::{self, ArtifactKind};
use super::keys::EndpointId;
use super::Error;
use crate::protocol::FetchProgress;

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

/// Build a connection pool with our connect-timeout override.
pub(crate) fn pool_options() -> PoolOptions {
    PoolOptions {
        connect_timeout: CONNECT_TIMEOUT,
        ..PoolOptions::default()
    }
}

/// Run a multi-provider iroh download into `store` using a caller-supplied
/// [`Downloader`].
///
/// The reusable core shared by the CLI's ephemeral path and the node's
/// persistent-store handler — it owns neither the endpoint nor the store,
/// so partial progress persists across providers and (for the node)
/// across fetches. Progress is reported through `on_progress`; the CLI
/// drives a progress bar, the node forwards [`FetchProgress`] frames over
/// the control socket.
///
/// Returns per-provider errors on failure. `DownloadProgressItem::ProviderFailed`
/// intentionally drops the underlying cause — the errors vector therefore
/// records only which provider failed plus the final stream-level cause if
/// the download terminates fatally.
pub(crate) async fn download_iroh_to_store(
    downloader: &Downloader,
    store: &FsStore,
    hash_and_format: HashAndFormat,
    providers: Vec<EndpointId>,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<(), Vec<Error>> {
    // Convert to iroh's bare type at the iroh-blobs API boundary.
    let providers: Vec<iroh::EndpointId> =
        providers.into_iter().map(EndpointId::into_inner).collect();

    // below we shuffle to avoid overloading a single provider, but the
    // trade-off is that we may lose freshness ordering if providers are prioritized by the caller.
    let progress = downloader.download(hash_and_format, Shuffled::new(providers));
    let mut stream = match progress.stream().await {
        Ok(s) => s,
        Err(e) => return Err(vec![Error::Iroh(format!("downloader rpc: {e}"))]),
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
                    on_progress(FetchProgress::TryingProvider {
                        endpoint_id: EndpointId::from(id),
                    });
                }
                DownloadProgressItem::ProviderFailed { id, .. } => {
                    let endpoint_id = EndpointId::from(id);
                    on_progress(FetchProgress::ProviderFailed { endpoint_id });
                    errors.push(Error::Iroh(format!(
                        "provider {endpoint_id}: download failed"
                    )));
                }
                DownloadProgressItem::Progress(offset) => {
                    on_progress(FetchProgress::Downloading {
                        offset,
                        total: None,
                    });
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

    // Trust the store: if nothing is missing, we have the content — even
    // if individual providers emitted `ProviderFailed` along the way. If
    // the completeness check itself errors, treat the attempt as failed
    // and surface the cause so it isn't silently swallowed.
    let done = match store.remote().local(hash_and_format).await {
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

/// Export a single blob from `store` to `dest`, atomically.
///
/// Writes to a sibling `.partial` file and renames on success, so a kill
/// mid-export never leaves a truncated file at `dest`. Returns the number
/// of bytes written and emits one [`FetchProgress::Exporting`] frame.
pub(crate) async fn export_blob_to(
    store: &FsStore,
    hash: Hash,
    dest: &Path,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<u64, Error> {
    let tmp = dest.with_extension("partial");
    store
        .blobs()
        .export(hash, &tmp)
        .await
        .map_err(|e| Error::Iroh(format!("export: {e}")))?;
    std::fs::rename(&tmp, dest).map_err(Error::Io)?;
    let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    on_progress(FetchProgress::Exporting {
        offset: bytes,
        total: Some(bytes),
        entry: None,
    });
    Ok(bytes)
}

/// Export a hashseq collection from `store` under `dest_dir`.
///
/// Each entry is exported in turn, emitting a per-member
/// [`FetchProgress::Exporting`] frame. Returns the total bytes written.
/// A killed export leaves a partial directory, which a retry overwrites.
pub(crate) async fn export_collection_to(
    store: &FsStore,
    hash: Hash,
    dest_dir: &Path,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<u64, Error> {
    let collection = Collection::load(hash, store.as_ref())
        .await
        .map_err(|e| Error::Iroh(format!("load collection: {e}")))?;
    std::fs::create_dir_all(dest_dir).map_err(Error::Io)?;
    let mut total = 0u64;
    for (name, entry_hash) in collection.iter() {
        let target = dest_dir.join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        store
            .blobs()
            .export(*entry_hash, &target)
            .await
            .map_err(|e| Error::Iroh(format!("export '{name}': {e}")))?;
        let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        total = total.saturating_add(bytes);
        on_progress(FetchProgress::Exporting {
            offset: total,
            total: None,
            entry: Some(name.to_string()),
        });
    }
    Ok(total)
}

/// Download an HTTP(S) blob into `store`, verifying it matches `expected`.
///
/// Routes HTTP content through the store (rather than straight to disk) so
/// an HTTP-fetched blob becomes a first-class, seedable blob — identical to
/// one fetched over iroh. ureq is blocking, so the network read runs on a
/// blocking thread; the file is then imported (copied) into the store and
/// its hash checked against the CID. Blob-only: collections require iroh.
///
/// The returned bytes are protected only by the import's temp tag, which is
/// dropped here — the caller must already hold a tag covering the expected
/// hash (it does: the fetch handler tags before downloading).
pub(crate) async fn http_to_store(
    store: &FsStore,
    url: &Url,
    expected: &Cid,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<Hash, Error> {
    on_progress(FetchProgress::Connecting);
    let expected_hash = cid_utils::cid_to_blake3_hash(expected)?;
    let tmp = std::env::temp_dir().join(format!(".rad-artifact-http-{}", expected_hash.to_hex()));

    // ureq is blocking; download to a temp file off the async runtime.
    let url_owned = url.clone();
    let tmp_dl = tmp.clone();
    let downloaded = tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let agent = http_agent();
        let file = std::fs::File::create(&tmp_dl).map_err(Error::Io)?;
        let mut writer = BufWriter::new(file);
        fetch_http(&agent, &url_owned, &mut writer)?;
        writer.flush().map_err(Error::Io)
    })
    .await
    .map_err(|e| Error::Iroh(format!("http download task: {e}")))?;
    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    on_progress(FetchProgress::Downloading {
        offset: size,
        total: Some(size),
    });

    // Import the file into the store (copy), then verify the hash.
    let import = store
        .add_path_with_opts(AddPathOptions {
            path: tmp.clone(),
            format: BlobFormat::Raw,
            mode: IrohImportMode::Copy,
        })
        .temp_tag()
        .await;
    let _ = std::fs::remove_file(&tmp);
    let tt = import.map_err(|e| Error::Iroh(format!("import http blob: {e}")))?;
    let hash = tt.hash();
    if hash != expected_hash {
        let actual = cid_utils::blake3_hash_to_cid(hash, ArtifactKind::Blob);
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(hash)
}
