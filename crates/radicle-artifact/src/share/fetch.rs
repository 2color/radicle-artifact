//! Reusable fetch + export core for the node.
//!
//! These are the building blocks the node's `Fetch`/`Export` handlers call
//! against their persistent [`FsStore`] and shared
//! [`Downloader`]: a multi-provider
//! iroh download (`download_iroh_to_store`), an HTTP-into-store download
//! (`http_to_store`, so HTTP blobs become seedable), and atomic export
//! helpers (`export_blob_to`, `export_collection_to`). None of them own
//! the endpoint, store, or runtime — the caller supplies those.
//!
//! Progress is reported through a [`FetchProgress`] callback so the caller
//! decides how to surface it: the node forwards frames over the control
//! socket, which the CLI then renders as a progress bar.
//!
//! Both transports bound how long an unreachable provider can tie up a
//! fetch. HTTP sets connect and receive-response timeouts on the
//! `ureq::Agent`; iroh applies a connect timeout on the connection pool
//! (`pool_options`) and an idle-progress timeout around the Downloader
//! stream. Mid-body HTTP stalls are intentionally not bounded — ureq 3.3
//! only offers a total-body timeout, which would break large downloads.
//!
//! Per-provider iroh causes are not preserved (the [`iroh_blobs`]
//! downloader drops them on `ProviderFailed`); set
//! `RUST_LOG=iroh_blobs=debug` in the node to surface them
//! (subscriber is installed in `rad-artifact node start --foreground`).

use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cid::Cid;
use iroh_blobs::api::blobs::{AddPathOptions, ExportProgressItem, ImportMode as IrohImportMode};
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

/// Build an iroh connection pool with our connect-timeout override.
pub(crate) fn pool_options() -> PoolOptions {
    PoolOptions {
        connect_timeout: CONNECT_TIMEOUT,
        ..PoolOptions::default()
    }
}

/// Run a multi-provider iroh download into `store` using a caller-supplied
/// [`Downloader`].
///
/// The reusable core behind the node's persistent-store fetch handler — it
/// owns neither the endpoint nor the store, so partial progress persists
/// across providers and across fetches. Progress is reported through
/// `on_progress`; the node forwards [`FetchProgress`] frames over the
/// control socket.
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
                    on_progress(FetchProgress::TryingLocation {
                        endpoint_id: EndpointId::from(id),
                    });
                }
                DownloadProgressItem::ProviderFailed { id, .. } => {
                    let endpoint_id = EndpointId::from(id);
                    on_progress(FetchProgress::LocationFailed { endpoint_id });
                    errors.push(Error::Iroh(format!(
                        "location {endpoint_id}: download failed"
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
    let done = match store.blobs().has(hash_and_format.hash).await {
        Ok(complete) => complete,
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

/// Removes a path (file or directory) when dropped — best-effort cleanup
/// that also runs if the owning future is cancelled mid-operation, so an
/// aborted export/download leaves no stray temp file or staging directory.
struct ScopedPath(PathBuf);

impl Drop for ScopedPath {
    fn drop(&mut self) {
        // One of these matches the path's kind; the other is a harmless no-op.
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How often [`http_to_store`] emits a `Downloading` frame while the
/// blocking body transfer runs — frequent enough for responsive progress
/// and to keep the caller's idle timer alive on large blobs.
const HTTP_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// Hidden sibling of `dest` used as the staging path for an atomic export:
/// `.<name>.rad-partial` in the same directory, so renaming it onto `dest`
/// is a same-filesystem move. Distinct suffix (not a replaced extension) so
/// it won't clobber a user file that happens to share `dest`'s stem.
fn partial_sibling(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "artifact".to_string());
    let staged = format!(".{name}.rad-partial");
    match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(staged),
        _ => PathBuf::from(staged),
    }
}

/// Export one blob to `target`, forwarding byte progress as
/// [`FetchProgress::Exporting`] frames so a long copy keeps the caller's
/// idle timer alive. `entry` names the collection member (None for a single
/// blob). Returns the blob size.
///
/// Copy-on-write filesystems complete instantly and emit no
/// `CopyProgress` (so no intermediate frames), which is fine — there is no
/// slow window to report. A plain cross-filesystem copy does emit them.
async fn export_streamed(
    store: &FsStore,
    hash: Hash,
    target: &Path,
    entry: Option<&str>,
    on_progress: &mut impl FnMut(FetchProgress),
) -> Result<u64, Error> {
    let mut stream = store.blobs().export(hash, target).stream().await;
    let mut size: Option<u64> = None;
    while let Some(item) = stream.next().await {
        match item {
            ExportProgressItem::Size(s) => size = Some(s),
            ExportProgressItem::CopyProgress(offset) => on_progress(FetchProgress::Exporting {
                offset,
                total: size,
                entry: entry.map(str::to_owned),
            }),
            ExportProgressItem::Done => {}
            ExportProgressItem::Error(e) => return Err(Error::Iroh(format!("export: {e}"))),
        }
    }
    let bytes = size.unwrap_or_else(|| std::fs::metadata(target).map(|m| m.len()).unwrap_or(0));
    // Always emit a terminal frame for this entry so completion is visible
    // even when the copy was instant (no CopyProgress events).
    on_progress(FetchProgress::Exporting {
        offset: bytes,
        total: Some(bytes),
        entry: entry.map(str::to_owned),
    });
    Ok(bytes)
}

/// Export a single blob from `store` to `dest`, atomically.
///
/// Writes to a sibling staging file and renames on success, so a kill or
/// cancellation mid-export never leaves a truncated file at `dest`. The
/// staging file is removed on every exit path (including future-drop and
/// a failed rename). Returns the bytes written and streams progress.
pub(crate) async fn export_blob_to(
    store: &FsStore,
    hash: Hash,
    dest: &Path,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<u64, Error> {
    let tmp = partial_sibling(dest);
    let _tmp = ScopedPath(tmp.clone());
    let bytes = export_streamed(store, hash, &tmp, None, &mut on_progress)
        .await
        .map_err(|e| {
            Error::Iroh(format!(
                "failed to export from the iroh store to '{}': {e}",
                dest.display()
            ))
        })?;
    std::fs::rename(&tmp, dest).map_err(Error::Io)?;
    Ok(bytes)
}

/// Export a hashseq collection from `store`, atomically.
///
/// Members are written into a sibling staging directory; the whole
/// directory is renamed onto `dest_dir` only once every member is exported,
/// so a killed or disconnected export never leaves a partially-populated
/// destination. Member names are sanitized — a name that is absolute or
/// escapes the directory (a `..` component) is rejected rather than written
/// outside `dest_dir`. Returns the total bytes written.
pub(crate) async fn export_collection_to(
    store: &FsStore,
    hash: Hash,
    dest_dir: &Path,
    on_progress: impl FnMut(FetchProgress),
) -> Result<u64, Error> {
    let collection = Collection::load(hash, store.as_ref())
        .await
        .map_err(|e| Error::Iroh(format!("load collection: {e}")))?;

    let staging = partial_sibling(dest_dir);
    // Clear any leftover staging from a previously-crashed export, then
    // guard it so a cancelled export cleans up too.
    let _ = std::fs::remove_dir_all(&staging);
    let _staging = ScopedPath(staging.clone());
    std::fs::create_dir_all(&staging).map_err(Error::Io)?;

    let total = export_members(store, &collection, &staging, on_progress)
        .await
        .map_err(|e| {
            Error::Iroh(format!(
                "failed to export from the iroh store to '{}': {e}",
                dest_dir.display()
            ))
        })?;
    // Swap staging into place. staging is a sibling of dest_dir, so the
    // rename is a same-filesystem move; replace any existing destination
    // first (rename onto a non-empty dir fails).
    if dest_dir.exists() {
        std::fs::remove_dir_all(dest_dir).map_err(Error::Io)?;
    }
    std::fs::rename(&staging, dest_dir).map_err(Error::Io)?;
    Ok(total)
}

/// Export each collection member into `dir`, rejecting unsafe names.
async fn export_members(
    store: &FsStore,
    collection: &Collection,
    dir: &Path,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<u64, Error> {
    let mut total = 0u64;
    for (name, entry_hash) in collection.iter() {
        let target = safe_join(dir, name)
            .ok_or_else(|| Error::Iroh(format!("unsafe collection member name: {name:?}")))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let name_ref: &str = name;
        let bytes = export_streamed(
            store,
            *entry_hash,
            &target,
            Some(name_ref),
            &mut on_progress,
        )
        .await
        .map_err(|e| Error::Iroh(format!("export '{name}': {e}")))?;
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// Join `name` under `base`, returning `None` when the name is absolute or
/// would escape `base` (a root, prefix, or `..` component). Normal nested
/// paths (`a/b/c`) and `.` are allowed — collection member names are
/// untrusted (a malicious provider controls them), so this guards against
/// writing outside the destination.
fn safe_join(base: &Path, name: &str) -> Option<PathBuf> {
    let rel = Path::new(name);
    for component in rel.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(base.join(rel))
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
///
/// While the body transfers, `Downloading` frames are emitted from the
/// async side every [`HTTP_PROGRESS_INTERVAL`] (offset = the temp file's
/// current size), keeping the caller's idle timer alive on large blobs. The
/// caller emits the initial `Connecting` frame.
///
/// Cancellation note: the blocking download cannot be aborted mid-flight
/// (dropping a `spawn_blocking` handle detaches the thread), so on a client
/// disconnect the transfer runs to its bounded end. The temp file is always
/// cleaned up via [`ScopedPath`], so no stray file is leaked. A stalled body
/// keeps emitting frames (the file size stops growing), so an HTTP stall is
/// not surfaced as an idle timeout — HTTP has no body-stall bound anyway.
pub(crate) async fn http_to_store(
    store: &FsStore,
    url: &Url,
    expected: &Cid,
    mut on_progress: impl FnMut(FetchProgress),
) -> Result<Hash, Error> {
    let expected_hash = cid_utils::cid_to_blake3_hash(expected)?;
    // Per-operation unique temp name: process id + a monotonic counter so
    // concurrent fetches of the same CID don't share (and clobber) a path.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        ".rad-artifact-http-{}-{}-{n}",
        expected_hash.to_hex(),
        std::process::id(),
    ));
    let _tmp = ScopedPath(tmp.clone());

    // ureq is blocking; run the download off the async runtime and report
    // progress from here by polling the temp file's size on an interval.
    let url_owned = url.clone();
    let tmp_dl = tmp.clone();
    let download = tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let agent = http_agent();
        let file = std::fs::File::create(&tmp_dl).map_err(Error::Io)?;
        let mut writer = BufWriter::new(file);
        fetch_http(&agent, &url_owned, &mut writer)?;
        writer.flush().map_err(Error::Io)
    });
    tokio::pin!(download);
    let joined = loop {
        tokio::select! {
            res = &mut download => break res,
            _ = tokio::time::sleep(HTTP_PROGRESS_INTERVAL) => {
                let offset = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                on_progress(FetchProgress::Downloading { offset, total: None });
            }
        }
    };
    joined.map_err(|e| Error::Iroh(format!("http download task: {e}")))??;

    // Import the file into the store (copy), then verify the hash.
    let tt = store
        .add_path_with_opts(AddPathOptions {
            path: tmp.clone(),
            format: BlobFormat::Raw,
            mode: IrohImportMode::Copy,
        })
        .temp_tag()
        .await
        .map_err(|e| Error::Iroh(format!("import http blob: {e}")))?;
    let hash = tt.hash();
    if blake3::Hash::from(hash) != expected_hash {
        let actual = cid_utils::blake3_hash_to_cid(hash.into(), ArtifactKind::Blob);
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;

    use super::*;

    /// Spawn a one-shot HTTP/1.1 server that serves `body` once and exits.
    /// Returns the URL to GET and the server thread's join handle.
    fn serve_once(body: Vec<u8>) -> (Url, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the request so the client's write side completes.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        let url = Url::parse(&format!("http://{addr}/blob")).unwrap();
        (url, handle)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// `http_to_store` downloads the body, imports it, and returns the hash
    /// when the bytes match the expected CID.
    #[test]
    fn http_to_store_imports_matching_blob() {
        runtime().block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();

            let body = b"hello over http".to_vec();
            let expected = cid_utils::blake3_hash_to_cid(blake3::hash(&body), ArtifactKind::Blob);
            let (url, server) = serve_once(body.clone());

            let hash = http_to_store(&store, &url, &expected, |_| {})
                .await
                .unwrap();
            assert_eq!(hash, Hash::new(&body));
            assert!(store.blobs().has(hash).await.unwrap());
            server.join().unwrap();
        });
    }

    /// A body whose bytes don't hash to `expected` surfaces `CidMismatch`
    /// rather than silently importing the wrong content.
    #[test]
    fn http_to_store_rejects_mismatch() {
        runtime().block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();

            // Expect one thing, serve another.
            let expected =
                cid_utils::blake3_hash_to_cid(blake3::hash(b"expected"), ArtifactKind::Blob);
            let (url, server) = serve_once(b"something else entirely".to_vec());

            let err = http_to_store(&store, &url, &expected, |_| {})
                .await
                .unwrap_err();
            assert!(matches!(err, Error::CidMismatch { .. }), "got {err:?}");
            server.join().unwrap();
        });
    }

    /// Build a small collection in the store and seed three child blobs.
    /// Returns the root hash. Member names are caller-supplied so tests can
    /// inject unsafe ones.
    async fn store_collection(store: &FsStore, members: &[(&str, &[u8])]) -> Hash {
        let mut pairs = Vec::new();
        // Hold child tags until the root tag covers them.
        let mut tags = Vec::new();
        for (name, data) in members {
            let tt = store.add_bytes(data.to_vec()).temp_tag().await.unwrap();
            pairs.push((name.to_string(), tt.hash()));
            tags.push(tt);
        }
        let collection = Collection::from_iter(pairs);
        let root = collection.store(store.as_ref()).await.unwrap();
        root.hash()
    }

    /// `export_collection_to` writes every member to disk, creating nested
    /// directories, and renames the staging tree onto the destination.
    #[test]
    fn export_collection_writes_members() {
        runtime().block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let root =
                store_collection(&store, &[("a.txt", b"alpha"), ("sub/b.txt", b"beta")]).await;

            let dest = tmp.path().join("out");
            let total = export_collection_to(&store, root, &dest, |_| {})
                .await
                .unwrap();

            assert_eq!(total, (b"alpha".len() + b"beta".len()) as u64);
            assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"alpha");
            assert_eq!(std::fs::read(dest.join("sub/b.txt")).unwrap(), b"beta");
            // Staging sibling is gone once the swap completes.
            assert!(!partial_sibling(&dest).exists());
        });
    }

    /// A member name that escapes the destination (`..`) aborts the export;
    /// the destination is never created and the staging tree is cleaned up.
    #[test]
    fn export_collection_rejects_unsafe_member() {
        runtime().block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let root = store_collection(&store, &[("../escape", b"evil")]).await;

            let dest = tmp.path().join("out");
            let err = export_collection_to(&store, root, &dest, |_| {})
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Iroh(m) if m.contains("unsafe collection member name")),
                "got {err:?}"
            );
            // Neither the destination nor the staging tree survives.
            assert!(!dest.exists());
            assert!(!partial_sibling(&dest).exists());
            // And nothing was written outside the destination.
            assert!(!tmp.path().join("escape").exists());
        });
    }

    /// `ScopedPath` removes a file when dropped — the cancellation-cleanup
    /// guarantee the export/download paths rely on.
    #[test]
    fn scoped_path_removes_file_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("staged");
        std::fs::write(&file, b"partial").unwrap();
        {
            let _guard = ScopedPath(file.clone());
            assert!(file.exists());
        }
        assert!(!file.exists());
    }

    /// `ScopedPath` also removes a staging directory when dropped.
    #[test]
    fn scoped_path_removes_dir_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("staging");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inner"), b"x").unwrap();
        {
            let _guard = ScopedPath(dir.clone());
            assert!(dir.exists());
        }
        assert!(!dir.exists());
    }

    #[test]
    fn safe_join_allows_nested_paths() {
        let base = Path::new("/dest");
        assert_eq!(
            safe_join(base, "a/b/c.txt"),
            Some(PathBuf::from("/dest/a/b/c.txt"))
        );
        assert!(safe_join(base, "file.bin").is_some());
        assert!(safe_join(base, "./file.bin").is_some());
    }

    #[test]
    fn safe_join_rejects_traversal_and_absolute() {
        let base = Path::new("/dest");
        // Leading, interior, and bare parent-dir escapes are all rejected.
        assert!(safe_join(base, "../escape").is_none());
        assert!(safe_join(base, "a/../../escape").is_none());
        assert!(safe_join(base, "..").is_none());
        // Absolute paths must not replace the base.
        assert!(safe_join(base, "/etc/passwd").is_none());
    }
}
