//! Long-running `rad-artifact` node: owns the seeder, exposes a
//! control socket, drives graceful shutdown on signals.
//!
//! [`run`] bootstraps the [`Seeder`](crate::seeder::Seeder), binds
//! `<home>/artifacts/control.sock`, accepts one [`Command`] per
//! connection and writes back one [`CommandResult`]. Parent-side
//! helpers (detached spawn, passphrase resolution, log rotation,
//! liveness polling) live in [`lifecycle`].
//!
//! - tags survive shutdown — restart resumes seeding what was previously
//!   tagged

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cid::Cid;
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::HashAndFormat;
use radicle::git::Oid;
use radicle::identity::RepoId;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::seeder;
use crate::{fetch, Error as ShareError};
use radicle_artifact_client::tokio::Client;
use radicle_artifact_core::cid::{self as cid_utils, ArtifactKind};
use radicle_artifact_core::keys::EndpointId;
use radicle_artifact_core::protocol::{
    Command, CommandError, CommandResult, DownloadReceipt, ErrorCode, ExportReceipt, FetchLocation,
    FetchProgress, FetchReceipt, HasResult, ImportMode, SeedReceipt, SeededEntry, Status,
    StreamEvent, UnseedReceipt,
};
use radicle_artifact_core::ARTIFACTS_DIR;

/// How long shutdown waits for in-flight handlers before forcing the
/// router down anyway. Sized to outlast a large collection import so a
/// shutdown mid-seed doesn't abort the write; a genuinely stuck handler
/// still can't pin us past this bound.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a connection may sit without sending its command line before
/// we drop it. Bounds idle/half-open clients so they can't pin a handler
/// task and fd indefinitely.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`iroh::protocol::Router::shutdown`] gets before we give up
/// and return.
const ROUTER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Failure modes for [`run`].
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// Another node is already bound to the control socket — bail out
    /// instead of stealing it.
    #[error("another rad-artifact node is already running at {0}")]
    AlreadyRunning(PathBuf),
    /// Seeder bootstrap or runtime error.
    #[error(transparent)]
    Share(#[from] ShareError),
    /// Local I/O (mkdir, bind, chmod) failure.
    #[error("node I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Shared per-node state passed to every connection handler.
///
/// Built once at startup and wrapped in an `Arc` so each spawned handler
/// gets a cheap clone. Holds the single `FsStore` and the single
/// [`Downloader`] (built on the seeder endpoint, pooling connections
/// across all fetches), plus identity/uptime fields for `Status`.
struct NodeCtx {
    /// The persistent blob store.
    store: FsStore,
    /// Downloader bound to the seeder endpoint, reused across fetches.
    downloader: Downloader,
    /// The seeder endpoint, kept for its live connection/traffic metrics
    /// (a clone shares the underlying counters with the router's endpoint).
    endpoint: iroh::Endpoint,
    /// Endpoint id the node serves on.
    endpoint_id: EndpointId,
    /// Unix timestamp (seconds) when the node bound its socket.
    started_at_unix: i64,
}

/// Run the node in the foreground until it receives a shutdown signal
/// or a [`Command::Shutdown`].
///
/// On entry the function:
/// 1. bootstraps the seeder under `<home>/artifacts/`
/// 2. probes the control socket — if a live owner answers, returns
///    [`NodeError::AlreadyRunning`]; if the socket file exists but
///    nothing answers, unlinks it
/// 3. binds the socket with 0600 perms
/// 4. installs SIGTERM/SIGINT handlers and runs the accept loop
///
/// On exit the function drains in-flight handlers (up to
/// `DRAIN_TIMEOUT`) and shuts the iroh router down (capped by
/// `ROUTER_SHUTDOWN_TIMEOUT`). The socket file is unlinked. Seeded
/// tags are intentionally left in place so a restart resumes the prior
/// set.
pub async fn run(home: &Path, secret: iroh::SecretKey) -> Result<(), NodeError> {
    let socket_path = home.join(ARTIFACTS_DIR).join("control.sock");
    // ensure the parent directory exists before we probe / bind
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Probe the socket BEFORE opening the FsStore: a live owner means
    // we'd block on its single-writer lock, so cheap-fail here with a
    // friendly error. A stale file is unlinked so bind() can succeed.
    if socket_path.exists() {
        let probe = Client::new(socket_path.clone());
        if probe.is_running().await {
            return Err(NodeError::AlreadyRunning(socket_path));
        }
        std::fs::remove_file(&socket_path)?;
    }

    let seeder = seeder::bootstrap(home, secret).await?;

    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let endpoint_id = EndpointId::from(seeder.router.endpoint().id());

    tracing::info!(
        endpoint_id = %endpoint_id,
        socket = %socket_path.display(),
        "rad-artifact node ready"
    );

    // One Downloader on the seeder endpoint, reused across all fetches so
    // the connection pool is shared (no per-fetch endpoint bind).
    let downloader = Downloader::new_with_opts(
        seeder.blobs.as_ref(),
        seeder.router.endpoint(),
        fetch::pool_options(),
    );
    let ctx = Arc::new(NodeCtx {
        store: seeder.blobs.clone(),
        downloader,
        endpoint: seeder.router.endpoint().clone(),
        endpoint_id,
        started_at_unix,
    });

    // Subscribe before installing the signal handler: a signal that
    // arrives during startup must land in a live receiver, otherwise the
    // broadcast (which doesn't buffer for absent receivers) drops it and
    // the accept loop never sees the shutdown.
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(8);
    spawn_signal_handler(shutdown_tx.clone());

    let in_flight = Arc::new(AtomicUsize::new(0));

    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.recv() => {
                // any send (signal or Command::Shutdown) -> stop accepting
                if res.is_ok() { break; }
            }
            accept = listener.accept() => {
                let (stream, _addr) = match accept {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                        continue;
                    }
                };
                let ctx = ctx.clone();
                let shutdown_tx = shutdown_tx.clone();
                let in_flight = in_flight.clone();
                in_flight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &ctx, &shutdown_tx).await {
                        tracing::warn!("handler error: {e}");
                    }
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }
    }

    // stop accepting new connections (file inode still alive until unlink)
    drop(listener);
    let _ = std::fs::remove_file(&socket_path);

    // wait for in-flight handlers; bounded so a stuck handler can't pin us
    let drain_deadline = Instant::now() + DRAIN_TIMEOUT;
    while in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = tokio::time::timeout(ROUTER_SHUTDOWN_TIMEOUT, seeder.router.shutdown()).await;
    tracing::info!("rad-artifact node stopped");
    Ok(())
}

/// Watch SIGTERM/SIGINT in the background and broadcast a shutdown signal.
fn spawn_signal_handler(shutdown_tx: broadcast::Sender<()>) {
    tokio::spawn(async move {
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return;
        };
        let Ok(mut int) = signal(SignalKind::interrupt()) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = shutdown_tx.send(());
    });
}

/// Read one command, then either write one response (one-shot commands)
/// or stream frames (`Fetch`/`Export`), and close.
async fn handle_connection(
    stream: UnixStream,
    ctx: &NodeCtx,
    shutdown_tx: &broadcast::Sender<()>,
) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    // Bound the wait for the command line so a connected-but-silent
    // client can't pin this handler (and its fd) forever.
    let n = match tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(res) => res?,
        Err(_) => return Ok(()),
    };
    if n == 0 {
        return Ok(());
    }

    match parse_command(line.trim_end()) {
        // Streaming commands write their own frames directly. They also get
        // the read half to watch for client disconnect during silent phases.
        Ok(Command::Export { cid, dest }) => {
            stream_export(ctx, &mut reader, &mut write, cid, dest).await
        }
        Ok(Command::Fetch {
            rid,
            release,
            cid,
            locations,
            seed,
        }) => {
            stream_fetch(
                ctx,
                &mut reader,
                &mut write,
                rid,
                release,
                cid,
                locations,
                seed,
            )
            .await
        }
        Ok(Command::Download {
            rid,
            release,
            cid,
            locations,
            dest,
            seed,
        }) => {
            stream_download(
                ctx,
                &mut reader,
                &mut write,
                rid,
                release,
                cid,
                locations,
                dest,
                seed,
            )
            .await
        }
        // One-shot commands return a single JSON line.
        Ok(cmd) => write_line(&mut write, dispatch(cmd, ctx, shutdown_tx).await).await,
        Err((code, msg)) => write_line(&mut write, err_json::<()>(code, msg)).await,
    }
}

/// Write a single JSON response line (with trailing newline) and flush.
async fn write_line(
    write: &mut (impl AsyncWriteExt + Unpin),
    mut response: String,
) -> io::Result<()> {
    response.push('\n');
    write.write_all(response.as_bytes()).await?;
    write.flush().await
}

/// Two-step wire decode: catch malformed `rid` (and other typed fields)
/// with a message that names the field, instead of leaking the
/// underlying serde error verbatim (`Unknown base code: n` etc.).
fn parse_command(line: &str) -> Result<Command, (ErrorCode, String)> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        (
            ErrorCode::InvalidRequest,
            format!("invalid command JSON: {e}"),
        )
    })?;
    if let Some(rid_s) = value.get("rid").and_then(|v| v.as_str()) {
        if let Err(e) = RepoId::from_str(rid_s) {
            return Err((
                ErrorCode::InvalidRequest,
                format!("invalid rid {rid_s:?}: {e}"),
            ));
        }
    }
    if let Some(cid_s) = value.get("cid").and_then(|v| v.as_str()) {
        if let Err(e) = Cid::from_str(cid_s) {
            return Err((
                ErrorCode::InvalidRequest,
                format!("invalid cid {cid_s:?}: {e}"),
            ));
        }
    }
    serde_json::from_value(value)
        .map_err(|e| (ErrorCode::InvalidRequest, format!("invalid command: {e}")))
}

/// Dispatch a one-shot command, returning the JSON line to send back
/// (without trailing newline). Streaming commands (`Fetch`, `Export`) are
/// handled in [`handle_connection`] before reaching here.
async fn dispatch(cmd: Command, ctx: &NodeCtx, shutdown_tx: &broadcast::Sender<()>) -> String {
    let store = &ctx.store;
    match cmd {
        // Cheap liveness probe: touch no state, just ack.
        Command::Alive => ok_json(()),
        Command::Status => {
            match build_status(store, &ctx.endpoint, ctx.endpoint_id, ctx.started_at_unix).await {
                Ok(status) => ok_json(status),
                Err(e) => err_from_share::<Status>(e),
            }
        }
        Command::Seed {
            rid,
            release,
            cid,
            path,
            kind,
            mode,
        } => seed_response(store, rid, release, cid, &path, kind, mode, ctx.endpoint_id).await,
        Command::Unseed { rid, release, cid } => unseed_response(store, rid, release, cid).await,
        Command::IsSeeding { rid, cid } => is_seeding_response(store, &rid, &cid).await,
        Command::ListSeeded { rid } => list_seeded_response(store, rid).await,
        Command::Has { cid } => has_response(store, &cid).await,
        // Intercepted in handle_connection; never reaches dispatch.
        Command::Export { .. } | Command::Fetch { .. } | Command::Download { .. } => {
            unreachable!("streaming commands are handled before dispatch")
        }
        Command::Shutdown => {
            // ack first, then broadcast so the loop tears down after the
            // response makes it to the wire
            let resp = ok_json(());
            let _ = shutdown_tx.send(());
            resp
        }
    }
}

/// Resolve a CID to the iroh hash+format pair, or a wire error.
fn hash_and_format(cid: &Cid) -> Result<HashAndFormat, (ErrorCode, String)> {
    let hash = cid_utils::cid_to_blake3_hash(cid)
        .map_err(|e| (ErrorCode::InvalidRequest, e.to_string()))?;
    match cid_utils::artifact_kind(cid) {
        Ok(ArtifactKind::Blob) => Ok(HashAndFormat::raw(hash.into())),
        Ok(ArtifactKind::Collection) => Ok(HashAndFormat::hash_seq(hash.into())),
        Err(e) => Err((ErrorCode::InvalidRequest, e.to_string())),
    }
}

/// `Has`: report local presence/completeness for a CID. Hash-keyed, so it
/// answers regardless of which repo (if any) tagged the content.
async fn has_response(store: &FsStore, cid: &Cid) -> String {
    let haf = match hash_and_format(cid) {
        Ok(h) => h,
        Err((code, msg)) => return err_json::<HasResult>(code, msg),
    };
    match store.remote().local(haf).await {
        Ok(info) => {
            let bytes = info.local_bytes();
            ok_json(HasResult {
                present: bytes > 0,
                complete: info.is_complete(),
                bytes,
            })
        }
        Err(e) => err_json::<HasResult>(ErrorCode::Iroh, format!("local lookup: {e}")),
    }
}

/// Serialize and write one stream frame as a JSON line, then flush.
async fn write_frame<T: serde::Serialize>(
    write: &mut (impl AsyncWriteExt + Unpin),
    event: &StreamEvent<T>,
) -> io::Result<()> {
    let mut line = serde_json::to_string(event).unwrap_or_else(|e| {
        format!(r#"{{"error":{{"code":"internal","message":"encode: {e}"}}}}"#)
    });
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await
}

/// Shorthand for writing a terminal error frame.
async fn stream_error<T: serde::Serialize>(
    write: &mut (impl AsyncWriteExt + Unpin),
    code: ErrorCode,
    message: String,
) -> io::Result<()> {
    write_frame::<T>(write, &StreamEvent::Error(CommandError { code, message })).await
}

/// Drive a streaming operation: forward `FetchProgress` frames as they
/// arrive on the channel, then write the terminal okay/error frame.
///
/// Disconnect-to-abort works two ways, so a vanished client never leaves
/// `op` running: a failed frame write propagates its error, and `read` is
/// polled for EOF (or any unexpected inbound byte) even during silent
/// phases that emit no frames (a long export or HTTP body). Either path
/// returns, dropping `op` and aborting the in-flight download/export.
async fn run_stream<T: serde::Serialize>(
    read: &mut (impl AsyncReadExt + Unpin),
    write: &mut (impl AsyncWriteExt + Unpin),
    op: impl AsyncFnOnce(mpsc::UnboundedSender<FetchProgress>) -> Result<T, (ErrorCode, String)>,
) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<FetchProgress>();
    let fut = op(tx);
    tokio::pin!(fut);
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            biased;
            res = &mut fut => {
                // Flush any progress buffered before completion.
                while let Ok(p) = rx.try_recv() {
                    write_frame(write, &StreamEvent::<T>::Progress(p)).await?;
                }
                let event = match res {
                    Ok(payload) => StreamEvent::Okay(payload),
                    Err((code, message)) => StreamEvent::Error(CommandError { code, message }),
                };
                return write_frame(write, &event).await;
            }
            Some(p) = rx.recv() => {
                write_frame(write, &StreamEvent::<T>::Progress(p)).await?;
            }
            // The client should stay silent until the terminal frame; a read
            // readiness means it closed (EOF) or went away. Stop and drop
            // `op`. read() is cancel-safe here: with no inbound bytes it just
            // stays pending, so being dropped by another arm loses nothing.
            _ = read.read(&mut probe) => {
                return Ok(());
            }
        }
    }
}

/// `Export`: stream already-local bytes to `dest`. Errors with `NotLocal`
/// if the content isn't complete in the store.
async fn stream_export(
    ctx: &NodeCtx,
    read: &mut (impl AsyncReadExt + Unpin),
    write: &mut (impl AsyncWriteExt + Unpin),
    cid: Cid,
    dest: PathBuf,
) -> io::Result<()> {
    let kind = match cid_utils::artifact_kind(&cid) {
        Ok(k) => k,
        Err(e) => {
            return stream_error::<ExportReceipt>(write, ErrorCode::InvalidRequest, e.to_string())
                .await
        }
    };
    let haf = match hash_and_format(&cid) {
        Ok(h) => h,
        Err((code, msg)) => return stream_error::<ExportReceipt>(write, code, msg).await,
    };
    let hash = haf.hash;

    // Export needs the bytes already complete locally.
    match ctx.store.blobs().has(hash).await {
        Ok(true) => {}
        Ok(false) => {
            return stream_error::<ExportReceipt>(
                write,
                ErrorCode::NotLocal,
                format!("content for {cid} is not complete in the store"),
            )
            .await
        }
        Err(e) => {
            return stream_error::<ExportReceipt>(
                write,
                ErrorCode::Iroh,
                format!("local lookup: {e}"),
            )
            .await
        }
    }

    run_stream(read, write, async move |tx| {
        let on_progress = move |p| {
            let _ = tx.send(p);
        };
        let bytes = match kind {
            ArtifactKind::Blob => fetch::export_blob_to(&ctx.store, hash, &dest, on_progress).await,
            ArtifactKind::Collection => {
                fetch::export_collection_to(&ctx.store, hash, &dest, on_progress).await
            }
        }
        .map_err(|e| (share_error_to_code(&e), e.to_string()))?;
        Ok(ExportReceipt { cid, dest, bytes })
    })
    .await
}

/// Split resolved locations into iroh providers and HTTP URLs.
fn partition(locations: &[FetchLocation]) -> (Vec<EndpointId>, Vec<Url>) {
    let mut iroh = Vec::new();
    let mut urls = Vec::new();
    for loc in locations {
        match loc {
            FetchLocation::Iroh(id) => iroh.push(*id),
            FetchLocation::Url(u) => urls.push(u.clone()),
        }
    }
    (iroh, urls)
}

/// Export `hash` to `dest` per `kind`, mapping errors to a wire code.
async fn export_to_dest(
    store: &FsStore,
    hash: iroh_blobs::Hash,
    kind: ArtifactKind,
    dest: &Path,
    on_progress: impl FnMut(FetchProgress),
) -> Result<u64, (ErrorCode, String)> {
    match kind {
        ArtifactKind::Blob => fetch::export_blob_to(store, hash, dest, on_progress).await,
        ArtifactKind::Collection => {
            fetch::export_collection_to(store, hash, dest, on_progress).await
        }
    }
    .map_err(|e| (share_error_to_code(&e), e.to_string()))
}

/// An Artifact fetched complete into the store: the bytes are present and
/// held alive by a [Temp Tag](iroh_blobs::api::TempTag) until this value
/// drops. The caller keeps it until any Seeded Tag is set, so there's no GC
/// window between releasing the transient protection and the persistent tag
/// landing; an error or client disconnect drops it and GC reclaims the bytes.
struct Fetched {
    /// Artifact kind derived from the CID (for export dispatch).
    kind: ArtifactKind,
    /// BLAKE3 hash of the content.
    hash: iroh_blobs::Hash,
    /// `true` if the bytes were already complete locally (no network used).
    from_cache: bool,
    /// Transient GC protection; released on drop, never read directly.
    _tt: iroh_blobs::api::TempTag,
}

/// Fetch `cid` into the store: a no-op if the bytes are already complete
/// locally, otherwise download from `locations` (iroh providers batched into
/// one multi-provider download, HTTP URLs tried in sequence as a blob-only
/// fallback) and re-check completeness against the store. The kind and hash
/// are derived from `cid`.
///
/// Returns the complete, protected bytes — hold the result until any Seeded
/// Tag is set, then let it drop. On failure the temp tag is dropped here so
/// GC reclaims any partial.
async fn fetch_into_store(
    ctx: &NodeCtx,
    cid: &Cid,
    locations: &[FetchLocation],
    on_progress: &mut impl FnMut(FetchProgress),
) -> Result<Fetched, (ErrorCode, String)> {
    let kind =
        cid_utils::artifact_kind(cid).map_err(|e| (ErrorCode::InvalidRequest, e.to_string()))?;
    let haf = hash_and_format(cid)?;
    let hash = haf.hash;
    let store = &ctx.store;

    // Tag the CID to prevent GC before doing anything else. This covers both paths:
    // cached bytes that exist only as an untagged GC cache (a prior non-seed
    // fetch) can't be GCed between the completeness check and export/seed
    // on the fast path, and in-flight bytes are protected on the download
    // path. If the returned `Fetched` is dropped (disconnect) the temp tag
    // drops with it.
    let tt = store
        .tags()
        .temp_tag(haf)
        .await
        .map_err(|e| (ErrorCode::Iroh, format!("temp tag: {e}")))?;

    // Fast path: bytes already complete locally.
    let already = store
        .blobs()
        .has(hash)
        .await
        .map_err(|e| (ErrorCode::Iroh, format!("local lookup: {e}")))?;
    if already {
        return Ok(Fetched {
            kind,
            hash,
            from_cache: true,
            _tt: tt,
        });
    }

    on_progress(FetchProgress::Connecting);

    let (iroh_ids, urls) = partition(locations);
    let no_locations = iroh_ids.is_empty() && urls.is_empty();
    let mut errors: Vec<String> = Vec::new();
    let mut got = false;

    if !iroh_ids.is_empty() {
        match fetch::download_iroh_to_store(
            &ctx.downloader,
            store,
            haf,
            iroh_ids,
            &mut *on_progress,
        )
        .await
        {
            Ok(()) => got = true,
            Err(errs) => errors.extend(errs.into_iter().map(|e| e.to_string())),
        }
    }
    if !got {
        match kind {
            // HTTP is a blob-only fallback; stop at the first success
            // (completeness is re-checked against the store below).
            ArtifactKind::Blob => {
                for url in &urls {
                    match fetch::http_to_store(store, url, cid, &mut *on_progress).await {
                        Ok(_) => break,
                        Err(e) => errors.push(e.to_string()),
                    }
                }
            }
            ArtifactKind::Collection => {
                for url in &urls {
                    errors.push(format!("HTTP fetch unsupported for collection: {url}"));
                }
            }
        }
    }

    // Trust the store: complete means we have it, whatever individual
    // providers reported.
    let complete = store
        .remote()
        .local(haf)
        .await
        .map(|i| i.is_complete())
        .unwrap_or(false);
    if !complete {
        drop(tt); // release protection so GC reclaims the partial
        let code = if no_locations {
            ErrorCode::NoLocations
        } else {
            ErrorCode::AllFailed
        };
        let msg = if errors.is_empty() {
            "no locations succeeded".to_string()
        } else {
            errors.join("; ")
        };
        return Err((code, msg));
    }

    Ok(Fetched {
        kind,
        hash,
        from_cache: false,
        _tt: tt,
    })
}

/// `Fetch`: pull the artifact complete into the store (no-op if already
/// local, else download from `locations`), tagging as seeded if requested.
/// Writes nothing to disk — see [`stream_download`].
///
/// Abort-safe: the [`Fetched`] temp tag protects the content until the Seeded
/// Tag is set and the value drops at the end of the closure; a disconnect
/// mid-stream drops this future via [`run_stream`], releasing the tag so GC
/// reclaims any partial.
#[allow(clippy::too_many_arguments)]
async fn stream_fetch(
    ctx: &NodeCtx,
    read: &mut (impl AsyncReadExt + Unpin),
    write: &mut (impl AsyncWriteExt + Unpin),
    rid: RepoId,
    release: Option<Oid>,
    cid: Cid,
    locations: Vec<FetchLocation>,
    seed: bool,
) -> io::Result<()> {
    let endpoint_id = ctx.endpoint_id;

    run_stream(read, write, async move |tx| {
        let mut on_progress = move |p| {
            let _ = tx.send(p);
        };
        let store = &ctx.store;

        let fetched = fetch_into_store(ctx, &cid, &locations, &mut on_progress).await?;
        let seeded = tag_if_seeding(store, &rid, release.as_ref(), &cid, fetched.hash, seed)
            .await
            .map_err(|e| (share_error_to_code(&e), e.to_string()))?;

        // Logical size now complete in the store; no disk write.
        let bytes = seeder::artifact_size_for(store, &cid, fetched.hash).await;
        Ok(FetchReceipt {
            rid,
            cid,
            bytes,
            from_cache: fetched.from_cache,
            seeded,
            endpoint_id,
        })
        // `fetched` (and its temp tag) drops here, after any Seeded Tag is set.
    })
    .await
}

/// `Download`: [`Fetch`](stream_fetch) into the store, then export to `dest`.
/// Fast-path export if already local; tags as seeded if requested.
///
/// Abort-safe identically to [`stream_fetch`]: the [`Fetched`] temp tag
/// protects the content across the download AND the export (the export runs
/// before the Seeded Tag is set), so the bytes can't be reclaimed mid-export;
/// a disconnect drops this future and the protection.
#[allow(clippy::too_many_arguments)]
async fn stream_download(
    ctx: &NodeCtx,
    read: &mut (impl AsyncReadExt + Unpin),
    write: &mut (impl AsyncWriteExt + Unpin),
    rid: RepoId,
    release: Option<Oid>,
    cid: Cid,
    locations: Vec<FetchLocation>,
    dest: PathBuf,
    seed: bool,
) -> io::Result<()> {
    let endpoint_id = ctx.endpoint_id;

    run_stream(read, write, async move |tx| {
        let mut on_progress = move |p| {
            let _ = tx.send(p);
        };
        let store = &ctx.store;

        let fetched = fetch_into_store(ctx, &cid, &locations, &mut on_progress).await?;

        // Export inside the protected window, before the Seeded Tag is set.
        let bytes =
            export_to_dest(store, fetched.hash, fetched.kind, &dest, &mut on_progress).await?;
        let seeded = tag_if_seeding(store, &rid, release.as_ref(), &cid, fetched.hash, seed)
            .await
            .map_err(|e| (share_error_to_code(&e), e.to_string()))?;

        Ok(DownloadReceipt {
            rid,
            cid,
            dest,
            bytes,
            from_cache: fetched.from_cache,
            seeded,
            endpoint_id,
        })
        // `fetched` (and its temp tag) drops here, after any Seeded Tag is set.
    })
    .await
}

/// Tag `(rid, release, cid)` as seeded when a `seed: true` fetch/download
/// asked for it, returning whether a tag was actually set.
///
/// `seed` without a `release` can't form a tag key; the CLI always supplies
/// one when seeding, so this only fires on a malformed request — warn and
/// skip rather than fail the transfer the bytes already completed.
async fn tag_if_seeding(
    store: &iroh_blobs::api::Store,
    rid: &RepoId,
    release: Option<&Oid>,
    cid: &Cid,
    hash: iroh_blobs::Hash,
    seed: bool,
) -> Result<bool, ShareError> {
    if !seed {
        return Ok(false);
    }
    let Some(release) = release else {
        tracing::warn!("seed requested for {cid} without a release; not tagging");
        return Ok(false);
    };
    seeder::tag_seeded(store, rid, release, cid, hash).await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn seed_response(
    store: &FsStore,
    rid: RepoId,
    release: Oid,
    cid: Cid,
    path: &Path,
    kind: ArtifactKind,
    mode: ImportMode,
    endpoint_id: EndpointId,
) -> String {
    if !path.exists() {
        return err_json::<SeedReceipt>(
            ErrorCode::PathNotFound,
            format!("path not found: {}", path.display()),
        );
    }

    let was_already = match seeder::is_seeded(store, &rid, &release, &cid).await {
        Ok(v) => v,
        Err(e) => return err_from_share::<SeedReceipt>(e),
    };
    let hash = match seeder::seed_artifact(store, &rid, &release, &cid, path, kind, mode).await {
        Ok(hash) => hash,
        Err(e) => return err_from_share::<SeedReceipt>(e),
    };
    let bytes = seeder::artifact_size_for(store, &cid, hash).await;
    let receipt = SeedReceipt {
        rid,
        cid,
        endpoint_id,
        bytes,
        was_new: !was_already,
    };
    ok_json(receipt)
}

/// `release: Some(id)` drops that one release's tag; `None` stops seeding the
/// CID across every release of `rid`. `was_removed` reports whether anything
/// was actually tagged before the removal.
async fn unseed_response(store: &FsStore, rid: RepoId, release: Option<Oid>, cid: Cid) -> String {
    let was_removed = match &release {
        Some(release) => {
            let was = match seeder::is_seeded(store, &rid, release, &cid).await {
                Ok(v) => v,
                Err(e) => return err_from_share::<UnseedReceipt>(e),
            };
            if let Err(e) = seeder::untag_seeded(store, &rid, release, &cid).await {
                return err_from_share::<UnseedReceipt>(e);
            }
            was
        }
        None => match seeder::untag_all(store, &rid, &cid).await {
            Ok(removed) => removed > 0,
            Err(e) => return err_from_share::<UnseedReceipt>(e),
        },
    };
    ok_json(UnseedReceipt {
        rid,
        cid,
        was_removed,
    })
}

async fn is_seeding_response(store: &FsStore, rid: &RepoId, cid: &Cid) -> String {
    match seeder::is_seeded_any(store, rid, cid).await {
        Ok(v) => ok_json(v),
        Err(e) => err_from_share::<bool>(e),
    }
}

async fn list_seeded_response(store: &FsStore, rid: RepoId) -> String {
    let cids = match seeder::seeded_cids(store, &rid).await {
        Ok(v) => v,
        Err(e) => return err_from_share::<Vec<SeededEntry>>(e),
    };
    let mut out = Vec::with_capacity(cids.len());
    for (cid, hash) in cids {
        let bytes = seeder::artifact_size_for(store, &cid, hash).await;
        out.push(SeededEntry { cid, bytes });
    }
    ok_json(out)
}

async fn build_status(
    store: &FsStore,
    endpoint: &iroh::Endpoint,
    endpoint_id: EndpointId,
    started_at_unix: i64,
) -> Result<Status, ShareError> {
    let metrics = endpoint.metrics();
    // A blob shared across releases carries one tag per release; collapse to
    // distinct blobs so the count and byte total reflect what's on disk.
    let distinct: std::collections::HashMap<iroh_blobs::Hash, Cid> = seeder::all_seeded(store)
        .await?
        .into_iter()
        .map(|(_rid, _release, cid, hash)| (hash, cid))
        .collect();
    let count = distinct.len();
    let mut bytes_logical = 0u64;
    for (hash, cid) in &distinct {
        bytes_logical =
            bytes_logical.saturating_add(seeder::artifact_size_for(store, cid, *hash).await);
    }
    // Socket counters are byte totals (sends include disco frames; recv
    // counts data only) and connection/path tallies — see the field docs
    // on `ConnectionStats`/`TrafficStats` for the disco-vs-data split.
    let s = &metrics.socket;
    let opened_total = s.num_conns_opened.get();
    let closed_total = s.num_conns_closed.get();
    let connections = radicle_artifact_core::protocol::ConnectionStats {
        active: opened_total.saturating_sub(closed_total) as u32,
        opened_total,
        closed_total,
        direct_total: s.num_conns_direct.get(),
        holepunch_attempts: s.holepunch_attempts.get(),
        paths_direct: s.paths_direct.get(),
        paths_relayed: s.paths_relay.get(),
    };
    let traffic = radicle_artifact_core::protocol::TrafficStats {
        out_bytes: s
            .send_ipv4
            .get()
            .saturating_add(s.send_ipv6.get())
            .saturating_add(s.send_relay.get()),
        in_bytes: s
            .recv_data_ipv4
            .get()
            .saturating_add(s.recv_data_ipv6.get())
            .saturating_add(s.recv_data_relay.get())
            .saturating_add(s.recv_data_custom.get()),
    };
    let relay = relay_stats(endpoint);
    // A bound socket with no connected home relay is reachable only by peers
    // that can holepunch a direct path; flag it as advice.
    let relay_unreachable = !relay.relays.iter().any(|r| r.connected);
    Ok(Status {
        endpoint_id,
        started_at_unix,
        seeded: radicle_artifact_core::protocol::SeededStats {
            count,
            bytes_logical,
        },
        connections,
        traffic,
        relay,
        warnings: radicle_artifact_core::protocol::Warnings { relay_unreachable },
    })
}

/// Snapshot home-relay connectivity and latency from the live endpoint.
///
/// `home_relay_status()` gives connection state per relay; `net_report()`
/// (best-effort, may be empty before the first probe lands) supplies the
/// preferred relay, UDP reachability, and per-relay round-trip latency.
fn relay_stats(endpoint: &iroh::Endpoint) -> radicle_artifact_core::protocol::RelayStats {
    use iroh::Watcher;

    let report = endpoint.net_report().get();
    // Lowest measured latency per relay URL across probe types (v4/v6/https).
    let mut latency_ms: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if let Some(r) = &report {
        for (_probe, url, dur) in r.relay_latency.iter() {
            let ms = dur.as_millis() as u64;
            latency_ms
                .entry(url.to_string())
                .and_modify(|m| *m = (*m).min(ms))
                .or_insert(ms);
        }
    }

    let relays = endpoint
        .home_relay_status()
        .get()
        .into_iter()
        .map(|s| {
            let url = s.url().to_string();
            radicle_artifact_core::protocol::RelayHealth {
                latency_ms: latency_ms.get(&url).copied(),
                connected: s.is_connected(),
                last_error: s.last_error().map(|e| e.to_string()),
                url,
            }
        })
        .collect();

    radicle_artifact_core::protocol::RelayStats {
        relays,
        preferred: report
            .as_ref()
            .and_then(|r| r.preferred_relay.as_ref().map(|u| u.to_string())),
        udp_v4: report.as_ref().map(|r| r.udp_v4).unwrap_or(false),
        udp_v6: report.as_ref().map(|r| r.udp_v6).unwrap_or(false),
    }
}

fn ok_json<T: serde::Serialize>(v: T) -> String {
    serde_json::to_string(&CommandResult::Okay(v))
        .unwrap_or_else(|e| format!(r#"{{"error":{{"code":"internal","message":"encode: {e}"}}}}"#))
}

fn err_json<T>(code: ErrorCode, message: String) -> String
where
    T: serde::Serialize,
{
    serde_json::to_string(&CommandResult::<T>::Error(CommandError { code, message }))
        .unwrap_or_else(|e| format!(r#"{{"error":{{"code":"internal","message":"encode: {e}"}}}}"#))
}

fn err_from_share<T>(e: ShareError) -> String
where
    T: serde::Serialize,
{
    let code = share_error_to_code(&e);
    err_json::<T>(code, e.to_string())
}

fn share_error_to_code(e: &ShareError) -> ErrorCode {
    match e {
        ShareError::CidMismatch { .. } => ErrorCode::CidMismatch,
        ShareError::Io(_) => ErrorCode::Io,
        ShareError::Iroh(_) => ErrorCode::Iroh,
        _ => ErrorCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cid::multihash::Multihash;

    use super::*;
    use radicle_artifact_core::cid::{
        self as cid_utils, ArtifactKind, HASH_CODE_BLAKE3, RAW_CODEC,
    };

    /// Build a fake but well-formed blob CID over `data` so the
    /// `tag_seeded` path picks `HashAndFormat::raw`.
    fn fake_blob_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(RAW_CODEC, mh)
    }

    fn rid_a() -> RepoId {
        RepoId::from_str("rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip").unwrap()
    }

    fn release_a() -> Oid {
        Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap()
    }

    /// Spawn a node under `home`, wait for its control socket to appear, and
    /// return the socket path plus the join handle for asserting clean exit.
    async fn start_node(
        home: &Path,
        secret: iroh::SecretKey,
    ) -> (PathBuf, tokio::task::JoinHandle<Result<(), NodeError>>) {
        let home_path = home.to_path_buf();
        let handle = tokio::spawn(async move { run(&home_path, secret).await });
        let socket = home.join(ARTIFACTS_DIR).join("control.sock");
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(socket.exists(), "control socket never appeared");
        (socket, handle)
    }

    /// End-to-end client↔node round-trip covering Status, Seed (new +
    /// duplicate + CID mismatch), IsSeeding, ListSeeded, Unseed (new +
    /// duplicate), and Shutdown.
    #[test]
    fn node_round_trip() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();

            // Compute a real blob CID from a real file so the import
            // verification passes.
            let blob_path = home.path().join("payload.bin");
            let payload = b"hello rad-artifact";
            fs::write(&blob_path, payload).unwrap();
            let real_cid = cid_utils::compute_blob_cid(&blob_path).unwrap();
            let rid = rid_a();

            // Pin the secret so the test is reproducible.
            let secret = iroh::SecretKey::from_bytes(&[1u8; 32]);
            let expected_endpoint_id = EndpointId::from(secret.public());

            // Run the node on a tokio task; capture the join handle so
            // we can assert clean exit.
            let home_path = home.path().to_path_buf();
            let node_handle = tokio::spawn(async move { run(&home_path, secret).await });

            // Wait for the socket to appear (bootstrap involves iroh
            // endpoint bind, which is async).
            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(socket.exists(), "control socket never appeared");

            let client = Client::new(socket.clone());

            // is_running succeeds.
            assert!(client.is_running().await);

            // Status round-trips and reports the expected endpoint id.
            let status = client.status().await.unwrap();
            assert_eq!(status.endpoint_id, expected_endpoint_id);
            assert_eq!(status.seeded.count, 0);

            // Seed (new).
            let receipt = client
                .seed(
                    rid,
                    release_a(),
                    real_cid,
                    &blob_path,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .unwrap();
            assert!(receipt.was_new);
            assert_eq!(receipt.endpoint_id, expected_endpoint_id);
            assert_eq!(receipt.bytes, payload.len() as u64);

            // Seed again — idempotent: was_new false.
            let receipt2 = client
                .seed(
                    rid,
                    release_a(),
                    real_cid,
                    &blob_path,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .unwrap();
            assert!(!receipt2.was_new);

            // Seed with a tampered path that does not match the CID.
            let bad_path = home.path().join("tampered.bin");
            fs::write(&bad_path, b"different bytes").unwrap();
            let bad_cid = fake_blob_cid(b"not the right preimage");
            let err = client
                .seed(
                    rid,
                    release_a(),
                    bad_cid,
                    &bad_path,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .expect_err("CID mismatch must error");
            match err {
                radicle_artifact_client::ClientError::Remote(CommandError { code, .. }) => {
                    assert_eq!(code, ErrorCode::CidMismatch);
                }
                other => panic!("expected CidMismatch error, got {other:?}"),
            }

            // IsSeeding reflects the tag we set above.
            assert!(client.is_seeding(rid, real_cid).await.unwrap());

            // ListSeeded returns exactly the one entry.
            let entries = client.list_seeded(rid).await.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].cid, real_cid);
            assert_eq!(entries[0].bytes, payload.len() as u64);

            // Status now reports one seeded artifact.
            let status = client.status().await.unwrap();
            assert_eq!(status.seeded.count, 1);
            assert_eq!(status.seeded.bytes_logical, payload.len() as u64);

            // Unseed once removes the tag; second call is idempotent.
            let r1 = client
                .unseed(rid, Some(release_a()), real_cid)
                .await
                .unwrap();
            assert!(r1.was_removed);
            let r2 = client
                .unseed(rid, Some(release_a()), real_cid)
                .await
                .unwrap();
            assert!(!r2.was_removed);
            assert!(!client.is_seeding(rid, real_cid).await.unwrap());

            // Shutdown — the node acks then exits cleanly.
            client.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), node_handle)
                .await
                .expect("node did not exit within 10s")
                .expect("join error")
                .expect("node returned error");

            // Socket file is gone after shutdown.
            assert!(!socket.exists());
        });
    }

    /// Malformed `rid`/`cid` on the wire must surface as `InvalidRequest`
    /// with the offending field named in the message — so callers don't
    /// have to grep for "invalid command JSON" to recognize bad input.
    #[test]
    fn invalid_typed_fields_surface_as_invalid_request() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let secret = iroh::SecretKey::from_bytes(&[4u8; 32]);
            let home_path = home.path().to_path_buf();
            let node_handle = tokio::spawn(async move { run(&home_path, secret).await });

            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(socket.exists());

            // The typed Client can't construct these frames since the
            // fields are now RepoId / Cid, so we hand-roll the wire.
            for (frame, expected_field) in [
                (
                    br#"{"command":"list-seeded","rid":"not-a-real-rid"}"#.as_slice(),
                    "rid",
                ),
                (
                    br#"{"command":"is-seeding","rid":"rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip","cid":"not-a-real-cid"}"#.as_slice(),
                    "cid",
                ),
            ] {
                let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
                tokio::io::AsyncWriteExt::write_all(&mut stream, frame)
                    .await
                    .unwrap();
                tokio::io::AsyncWriteExt::write_all(&mut stream, b"\n")
                    .await
                    .unwrap();
                let mut buf = String::new();
                tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut buf)
                    .await
                    .unwrap();
                let parsed: CommandResult<serde_json::Value> =
                    serde_json::from_str(buf.trim()).unwrap();
                match parsed {
                    CommandResult::Error(CommandError { code, message }) => {
                        assert_eq!(code, ErrorCode::InvalidRequest);
                        assert!(
                            message.contains(expected_field),
                            "message should name {expected_field}: {message}"
                        );
                    }
                    CommandResult::Okay(_) => panic!("expected error, got ok"),
                }
            }

            // Clean shutdown.
            let client = Client::new(socket);
            client.shutdown().await.unwrap();
            node_handle.await.unwrap().unwrap();
        });
    }

    /// A second `run` on the same home while the first is alive must
    /// fail with `AlreadyRunning`, not silently steal the socket.
    #[test]
    fn double_start_errors() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let secret = iroh::SecretKey::from_bytes(&[2u8; 32]);
            let home_path = home.path().to_path_buf();
            let first = tokio::spawn(async move { run(&home_path, secret).await });

            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(socket.exists());

            // Same home, different secret to keep keys distinct.
            let secret2 = iroh::SecretKey::from_bytes(&[3u8; 32]);
            let err = run(home.path(), secret2).await.expect_err("must fail");
            assert!(matches!(err, NodeError::AlreadyRunning(_)));

            // Tear down the first one so the test cleans up.
            let client = Client::new(socket);
            client.shutdown().await.unwrap();
            first.await.unwrap().unwrap();
        });
    }

    /// Send a one-shot command on a fresh connection and decode the reply.
    async fn oneshot<T: serde::de::DeserializeOwned>(
        socket: &Path,
        cmd: &Command,
    ) -> CommandResult<T> {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let mut line = serde_json::to_string(cmd).unwrap();
        line.push('\n');
        tokio::io::AsyncWriteExt::write_all(&mut stream, line.as_bytes())
            .await
            .unwrap();
        let mut buf = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut buf)
            .await
            .unwrap();
        serde_json::from_str(buf.trim()).unwrap()
    }

    /// Send a streaming command; return (progress frame count, terminal frame).
    async fn streaming<T: serde::de::DeserializeOwned>(
        socket: &Path,
        cmd: &Command,
    ) -> (usize, StreamEvent<T>) {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let mut line = serde_json::to_string(cmd).unwrap();
        line.push('\n');
        tokio::io::AsyncWriteExt::write_all(&mut stream, line.as_bytes())
            .await
            .unwrap();
        let mut buf = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut buf)
            .await
            .unwrap();
        let mut progress = 0usize;
        let mut terminal = None;
        for l in buf.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<StreamEvent<T>>(l).unwrap() {
                StreamEvent::Progress(_) => progress += 1,
                term => terminal = Some(term),
            }
        }
        (progress, terminal.expect("a terminal frame"))
    }

    /// `Has` reflects store presence; `Export` streams local bytes to disk
    /// and reports `NotLocal` for content the store doesn't hold.
    #[test]
    fn has_and_export_round_trip() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let payload = b"hello has/export";
            let blob_path = home.path().join("payload.bin");
            fs::write(&blob_path, payload).unwrap();
            let cid = cid_utils::compute_blob_cid(&blob_path).unwrap();
            let rid = rid_a();

            let secret = iroh::SecretKey::from_bytes(&[5u8; 32]);
            let home_path = home.path().to_path_buf();
            let node_handle = tokio::spawn(async move { run(&home_path, secret).await });

            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(socket.exists(), "control socket never appeared");

            // Seed the blob so its bytes live in the store.
            let client = Client::new(socket.clone());
            client
                .seed(
                    rid,
                    release_a(),
                    cid,
                    &blob_path,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .unwrap();

            // Has: present and complete with the right size.
            match oneshot::<HasResult>(&socket, &Command::Has { cid }).await {
                CommandResult::Okay(h) => {
                    assert!(h.present);
                    assert!(h.complete);
                    assert_eq!(h.bytes, payload.len() as u64);
                }
                CommandResult::Error(e) => panic!("has errored: {e:?}"),
            }

            // Has on content the store doesn't hold: absent.
            let unknown = fake_blob_cid(b"never stored");
            match oneshot::<HasResult>(&socket, &Command::Has { cid: unknown }).await {
                CommandResult::Okay(h) => {
                    assert!(!h.present);
                    assert!(!h.complete);
                }
                CommandResult::Error(e) => panic!("has errored: {e:?}"),
            }

            // Export streams the bytes to disk; the file matches the payload.
            let dest = home.path().join("exported.bin");
            let (_progress, term) = streaming::<ExportReceipt>(
                &socket,
                &Command::Export {
                    cid,
                    dest: dest.clone(),
                },
            )
            .await;
            match term {
                StreamEvent::Okay(r) => {
                    assert_eq!(r.bytes, payload.len() as u64);
                    assert_eq!(r.dest, dest);
                }
                other => panic!("expected okay, got {other:?}"),
            }
            assert_eq!(fs::read(&dest).unwrap(), payload);

            // Export of absent content reports NotLocal.
            let (_p, term) = streaming::<ExportReceipt>(
                &socket,
                &Command::Export {
                    cid: unknown,
                    dest: home.path().join("nope.bin"),
                },
            )
            .await;
            match term {
                StreamEvent::Error(e) => assert_eq!(e.code, ErrorCode::NotLocal),
                other => panic!("expected NotLocal error, got {other:?}"),
            }

            client.shutdown().await.unwrap();
            node_handle.await.unwrap().unwrap();
        });
    }

    /// `Fetch` (store-only) and `Download` (to disk) fast paths over
    /// already-local bytes: `Fetch` writes no file and reports the logical
    /// store size; `Download` writes the file; `Fetch --seed` tags a second
    /// repo by shared hash; an empty location set on absent content reports
    /// `NoLocations`. (The networked download path needs two endpoints and is
    /// exercised via the shared core, not here.)
    #[test]
    fn fetch_and_download_fast_path_and_no_locations() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let payload = b"hello fetch fast-path";
            let blob_path = home.path().join("payload.bin");
            fs::write(&blob_path, payload).unwrap();
            let cid = cid_utils::compute_blob_cid(&blob_path).unwrap();
            let rid = rid_a();

            let secret = iroh::SecretKey::from_bytes(&[6u8; 32]);
            let home_path = home.path().to_path_buf();
            let node_handle = tokio::spawn(async move { run(&home_path, secret).await });

            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(socket.exists(), "control socket never appeared");

            // Seed so the bytes live in the store.
            let client = Client::new(socket.clone());
            client
                .seed(
                    rid,
                    release_a(),
                    cid,
                    &blob_path,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .unwrap();

            // Fetch fast path: complete locally, no locations, no seed, no
            // disk write. Reports the logical store size.
            let (_p, term) = streaming::<FetchReceipt>(
                &socket,
                &Command::Fetch {
                    rid,
                    release: None,
                    cid,
                    locations: vec![],
                    seed: false,
                },
            )
            .await;
            match term {
                StreamEvent::Okay(r) => {
                    assert!(r.from_cache);
                    assert!(!r.seeded);
                    assert_eq!(r.bytes, payload.len() as u64);
                }
                other => panic!("expected okay, got {other:?}"),
            }

            // Download fast path: same bytes, but exported to disk.
            let dest = home.path().join("downloaded.bin");
            let (_p, term) = streaming::<DownloadReceipt>(
                &socket,
                &Command::Download {
                    rid,
                    release: None,
                    cid,
                    locations: vec![],
                    dest: dest.clone(),
                    seed: false,
                },
            )
            .await;
            match term {
                StreamEvent::Okay(r) => {
                    assert!(r.from_cache);
                    assert!(!r.seeded);
                    assert_eq!(r.bytes, payload.len() as u64);
                    assert_eq!(r.dest, dest);
                }
                other => panic!("expected okay, got {other:?}"),
            }
            assert_eq!(fs::read(&dest).unwrap(), payload);

            // Fetch with seed under a second repo: the bytes are shared by
            // hash, so a (rid2, release, cid) tag is set without re-downloading.
            let rid2 = RepoId::from_str("rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5").unwrap();
            let (_p, term) = streaming::<FetchReceipt>(
                &socket,
                &Command::Fetch {
                    rid: rid2,
                    release: Some(release_a()),
                    cid,
                    locations: vec![],
                    seed: true,
                },
            )
            .await;
            match term {
                StreamEvent::Okay(r) => {
                    assert!(r.from_cache);
                    assert!(r.seeded);
                }
                other => panic!("expected okay, got {other:?}"),
            }
            assert!(client.is_seeding(rid2, cid).await.unwrap());

            // Absent content with no locations to try: NoLocations.
            let unknown = fake_blob_cid(b"absent");
            let (_p, term) = streaming::<FetchReceipt>(
                &socket,
                &Command::Fetch {
                    rid,
                    release: None,
                    cid: unknown,
                    locations: vec![],
                    seed: false,
                },
            )
            .await;
            match term {
                StreamEvent::Error(e) => assert_eq!(e.code, ErrorCode::NoLocations),
                other => panic!("expected NoLocations error, got {other:?}"),
            }

            client.shutdown().await.unwrap();
            node_handle.await.unwrap().unwrap();
        });
    }

    /// A client that vanishes mid-stream must drop the in-flight `op`, not
    /// leave it running. Driven directly over an in-memory pipe: the op parks
    /// forever, so the only way `run_stream` returns is the disconnect arm
    /// seeing EOF on the read half — at which point the op future is dropped.
    #[test]
    fn run_stream_aborts_on_client_disconnect() {
        use std::sync::atomic::AtomicBool;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Duplex pipe standing in for the connection: we keep the client
            // end, run_stream reads/writes the server end.
            let (client_end, server_end) = tokio::io::duplex(64);
            let (mut srv_read, mut srv_write) = tokio::io::split(server_end);

            // Guard whose Drop flips a flag, so we can prove the op future was
            // dropped (aborted) rather than allowed to complete.
            struct DropFlag(Arc<AtomicBool>);
            impl Drop for DropFlag {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let aborted = Arc::new(AtomicBool::new(false));
            let completed = Arc::new(AtomicBool::new(false));
            let aborted_op = aborted.clone();
            let completed_op = completed.clone();

            let op = async move |_tx: mpsc::UnboundedSender<FetchProgress>| {
                let _guard = DropFlag(aborted_op);
                // Never resolves on its own; only a drop ends it.
                std::future::pending::<()>().await;
                completed_op.store(true, Ordering::SeqCst);
                Ok::<(), (ErrorCode, String)>(())
            };

            let server =
                tokio::spawn(
                    async move { run_stream::<()>(&mut srv_read, &mut srv_write, op).await },
                );

            // Let run_stream enter its select loop (op parked) before the
            // client disconnects, so we exercise a genuinely in-flight op.
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(client_end); // EOF on the server read half

            let res = tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .expect("run_stream did not return after disconnect")
                .expect("join error");
            assert!(res.is_ok());
            assert!(
                aborted.load(Ordering::SeqCst),
                "op future was not dropped on disconnect"
            );
            assert!(
                !completed.load(Ordering::SeqCst),
                "op should have been aborted, not completed"
            );
        });
    }

    /// A leftover socket file with no live owner must be reclaimed: `run`
    /// probes it, finds nothing answering, unlinks it, and binds its own.
    #[test]
    fn stale_socket_is_reclaimed() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            fs::create_dir_all(socket.parent().unwrap()).unwrap();
            // Bind then drop: the socket file lingers but nothing listens.
            drop(UnixListener::bind(&socket).unwrap());
            assert!(socket.exists());

            let secret = iroh::SecretKey::from_bytes(&[7u8; 32]);
            let home_path = home.path().to_path_buf();
            let handle = tokio::spawn(async move { run(&home_path, secret).await });

            // Poll is_running (not just file existence) since the stale file
            // is replaced in place — only a live answer means we're up.
            let client = Client::new(socket.clone());
            let mut ready = false;
            for _ in 0..200 {
                if client.is_running().await {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(ready, "node never came up after reclaiming stale socket");

            client.shutdown().await.unwrap();
            handle.await.unwrap().unwrap();
        });
    }

    /// `Seed` against a path that doesn't exist must report `PathNotFound`
    /// before any import work.
    #[test]
    fn seed_missing_path_errors() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let secret = iroh::SecretKey::from_bytes(&[8u8; 32]);
            let (socket, handle) = start_node(home.path(), secret).await;

            let client = Client::new(socket.clone());
            let missing = home.path().join("does-not-exist.bin");
            let err = client
                .seed(
                    rid_a(),
                    release_a(),
                    fake_blob_cid(b"whatever"),
                    &missing,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .expect_err("missing path must error");
            match err {
                radicle_artifact_client::ClientError::Remote(CommandError { code, .. }) => {
                    assert_eq!(code, ErrorCode::PathNotFound);
                }
                other => panic!("expected PathNotFound, got {other:?}"),
            }

            client.shutdown().await.unwrap();
            handle.await.unwrap().unwrap();
        });
    }

    /// Bare malformed JSON (not just a bad typed field) must surface as
    /// `InvalidRequest` naming the JSON parse failure.
    #[test]
    fn malformed_json_surfaces_as_invalid_request() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let secret = iroh::SecretKey::from_bytes(&[9u8; 32]);
            let (socket, handle) = start_node(home.path(), secret).await;

            let mut stream = UnixStream::connect(&socket).await.unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut stream, b"this is not json\n")
                .await
                .unwrap();
            let mut buf = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut buf)
                .await
                .unwrap();
            let parsed: CommandResult<serde_json::Value> =
                serde_json::from_str(buf.trim()).unwrap();
            match parsed {
                CommandResult::Error(CommandError { code, message }) => {
                    assert_eq!(code, ErrorCode::InvalidRequest);
                    assert!(
                        message.contains("invalid command JSON"),
                        "message should name the JSON failure: {message}"
                    );
                }
                CommandResult::Okay(_) => panic!("expected error, got ok"),
            }

            let client = Client::new(socket);
            client.shutdown().await.unwrap();
            handle.await.unwrap().unwrap();
        });
    }
}
