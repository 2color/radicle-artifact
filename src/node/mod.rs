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

pub mod lifecycle;

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
use radicle::identity::RepoId;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::client::Client;
use crate::protocol::{
    Command, CommandError, CommandResult, ErrorCode, ExportReceipt, FetchLocation, FetchProgress,
    FetchReceipt, HasResult, ImportMode, SeedReceipt, SeededEntry, Status, StreamEvent,
    UnseedReceipt,
};
use crate::seeder::{self, ARTIFACTS_DIR};
use crate::share::cid_utils::{self, ArtifactKind};
use crate::share::keys::EndpointId;
use crate::share::{fetch, Error as ShareError};

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
            cid,
            locations,
            dest,
            seed,
        }) => {
            stream_fetch(
                ctx,
                &mut reader,
                &mut write,
                rid,
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
        Command::Status => match build_status(store, ctx.endpoint_id, ctx.started_at_unix).await {
            Ok(status) => ok_json(status),
            Err(e) => err_from_share::<Status>(e),
        },
        Command::Seed {
            rid,
            cid,
            path,
            kind,
            mode,
        } => seed_response(store, rid, cid, &path, kind, mode, ctx.endpoint_id).await,
        Command::Unseed { rid, cid } => unseed_response(store, rid, cid).await,
        Command::IsSeeding { rid, cid } => is_seeding_response(store, &rid, &cid).await,
        Command::ListSeeded { rid } => list_seeded_response(store, rid).await,
        Command::Has { cid } => has_response(store, &cid).await,
        // Intercepted in handle_connection; never reaches dispatch.
        Command::Export { .. } | Command::Fetch { .. } => {
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
        Ok(ArtifactKind::Blob) => Ok(HashAndFormat::raw(hash)),
        Ok(ArtifactKind::Collection) => Ok(HashAndFormat::hash_seq(hash)),
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
    match ctx.store.remote().local(haf).await {
        Ok(info) if info.is_complete() => {}
        Ok(_) => {
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

/// `Fetch`: fast-path export if local, else download into the store from
/// `locations`, export to `dest`, and tag as seeded if requested.
///
/// Abort-safe: a temp tag protects the content for the whole download +
/// export; the seeded tag (if any) is set only after the bytes are
/// verified complete; the temp tag is dropped last. A disconnect mid-stream
/// drops this future via [`run_stream`], releasing the temp tag so GC
/// reclaims any partial.
#[allow(clippy::too_many_arguments)]
async fn stream_fetch(
    ctx: &NodeCtx,
    read: &mut (impl AsyncReadExt + Unpin),
    write: &mut (impl AsyncWriteExt + Unpin),
    rid: RepoId,
    cid: Cid,
    locations: Vec<FetchLocation>,
    dest: PathBuf,
    seed: bool,
) -> io::Result<()> {
    let kind = match cid_utils::artifact_kind(&cid) {
        Ok(k) => k,
        Err(e) => {
            return stream_error::<FetchReceipt>(write, ErrorCode::InvalidRequest, e.to_string())
                .await
        }
    };
    let haf = match hash_and_format(&cid) {
        Ok(h) => h,
        Err((code, msg)) => return stream_error::<FetchReceipt>(write, code, msg).await,
    };
    let hash = haf.hash;
    let endpoint_id = ctx.endpoint_id;

    run_stream(read, write, async move |tx| {
        let mut on_progress = move |p| {
            let _ = tx.send(p);
        };
        let store = &ctx.store;

        // Protect the content for the whole handler BEFORE doing anything
        // else. This covers both paths: cached bytes that exist only as an
        // untagged GC cache (a prior non-seed fetch) can't be reclaimed
        // between the completeness check and export/seed on the fast path,
        // and in-flight bytes are protected on the download path. If this
        // future is dropped (disconnect) the temp tag drops with it.
        //
        // For a collection the tag covers `hash_seq(root)`; GC marks the
        // children by walking the root, so they are protected once the root
        // is complete. While the root itself is still downloading the
        // children are not yet marked — a GC sweep in that window can force
        // a re-download (progress loss), but never a false-complete, since
        // the seeded tag is set only after completeness is verified.
        let tt = store
            .tags()
            .temp_tag(haf)
            .await
            .map_err(|e| (ErrorCode::Iroh, format!("temp tag: {e}")))?;

        // Fast path: bytes already complete locally.
        let already = store
            .remote()
            .local(haf)
            .await
            .map_err(|e| (ErrorCode::Iroh, format!("local lookup: {e}")))?
            .is_complete();
        if already {
            let bytes = export_to_dest(store, hash, kind, &dest, &mut on_progress).await?;
            if seed {
                seeder::register_seeded(store, &rid, &cid, hash)
                    .await
                    .map_err(|e| (share_error_to_code(&e), e.to_string()))?;
            }
            drop(tt);
            return Ok(FetchReceipt {
                rid,
                cid,
                dest,
                bytes,
                from_cache: true,
                seeded: seed,
                endpoint_id,
            });
        }

        on_progress(FetchProgress::Connecting);

        let (iroh_ids, urls) = partition(&locations);
        let no_locations = iroh_ids.is_empty() && urls.is_empty();
        let mut errors: Vec<String> = Vec::new();
        let mut got = false;

        if !iroh_ids.is_empty() {
            match fetch::download_iroh_to_store(
                &ctx.downloader,
                store,
                haf,
                iroh_ids,
                &mut on_progress,
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
                        match fetch::http_to_store(store, url, &cid, &mut on_progress).await {
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

        // Export, then tag last so we never advertise incomplete content.
        let bytes = export_to_dest(store, hash, kind, &dest, &mut on_progress).await?;
        if seed {
            seeder::register_seeded(store, &rid, &cid, hash)
                .await
                .map_err(|e| (share_error_to_code(&e), e.to_string()))?;
        }
        // Drop after the seeded tag (if any) covers the bytes; without a
        // seed, dropping leaves the bytes for GC to reclaim (cache).
        drop(tt);
        Ok(FetchReceipt {
            rid,
            cid,
            dest,
            bytes,
            from_cache: false,
            seeded: seed,
            endpoint_id,
        })
    })
    .await
}

async fn seed_response(
    store: &FsStore,
    rid: RepoId,
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

    let was_already = match seeder::is_seeded(store, &rid, &cid).await {
        Ok(v) => v,
        Err(e) => return err_from_share::<SeedReceipt>(e),
    };
    if let Err(e) = seeder::seed_artifact(store, &rid, &cid, path, kind, mode).await {
        return err_from_share::<SeedReceipt>(e);
    }
    let bytes = seeder::artifact_size(store, &rid, &cid).await;
    let receipt = SeedReceipt {
        rid,
        cid,
        endpoint_id,
        bytes,
        was_new: !was_already,
    };
    ok_json(receipt)
}

async fn unseed_response(store: &FsStore, rid: RepoId, cid: Cid) -> String {
    let was_seeded = match seeder::is_seeded(store, &rid, &cid).await {
        Ok(v) => v,
        Err(e) => return err_from_share::<UnseedReceipt>(e),
    };
    if let Err(e) = seeder::unregister_seeded(store, &rid, &cid).await {
        return err_from_share::<UnseedReceipt>(e);
    }
    ok_json(UnseedReceipt {
        rid,
        cid,
        was_removed: was_seeded,
    })
}

async fn is_seeding_response(store: &FsStore, rid: &RepoId, cid: &Cid) -> String {
    match seeder::is_seeded(store, rid, cid).await {
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
    for cid in cids {
        let bytes = seeder::artifact_size(store, &rid, &cid).await;
        out.push(SeededEntry { cid, bytes });
    }
    ok_json(out)
}

async fn build_status(
    store: &FsStore,
    endpoint_id: EndpointId,
    started_at_unix: i64,
) -> Result<Status, ShareError> {
    let pairs = seeder::all_seeded(store).await?;
    let count = pairs.len();
    let mut bytes_logical = 0u64;
    for (rid, cid) in &pairs {
        bytes_logical = bytes_logical.saturating_add(seeder::artifact_size(store, rid, cid).await);
    }
    // Phase 2 leaves connection/traffic counters at zero; the
    // iroh-metrics wiring lands with the CLI in phase 3.
    Ok(Status {
        endpoint_id,
        started_at_unix,
        seeded: crate::protocol::SeededStats {
            count,
            bytes_logical,
        },
        disk: crate::protocol::DiskStats {
            store_bytes: 0,
            seeded_bytes_logical: bytes_logical,
        },
        connections: crate::protocol::ConnectionStats::default(),
        traffic: crate::protocol::TrafficStats::default(),
        warnings: crate::protocol::Warnings::default(),
    })
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
    use crate::share::cid_utils::{self, ArtifactKind, HASH_CODE_BLAKE3, RAW_CODEC};

    /// Build a fake but well-formed blob CID over `data` so the
    /// `register_seeded` path picks `HashAndFormat::raw`.
    fn fake_blob_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(RAW_CODEC, mh)
    }

    fn rid_a() -> RepoId {
        RepoId::from_str("rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip").unwrap()
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
                    bad_cid,
                    &bad_path,
                    ArtifactKind::Blob,
                    ImportMode::Copy,
                )
                .await
                .expect_err("CID mismatch must error");
            match err {
                crate::client::ClientError::Remote(CommandError { code, .. }) => {
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
            let r1 = client.unseed(rid, real_cid).await.unwrap();
            assert!(r1.was_removed);
            let r2 = client.unseed(rid, real_cid).await.unwrap();
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
                .seed(rid, cid, &blob_path, ArtifactKind::Blob, ImportMode::Copy)
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

    /// `Fetch` fast-path: already-local bytes export without network, seed
    /// across a second repo, and an empty location set on absent content
    /// reports `NoLocations`. (The networked download path needs two
    /// endpoints and is exercised via the shared core, not here.)
    #[test]
    fn fetch_fast_path_and_no_locations() {
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
                .seed(rid, cid, &blob_path, ArtifactKind::Blob, ImportMode::Copy)
                .await
                .unwrap();

            // Fast path: complete locally, no locations needed, no seed.
            let dest = home.path().join("fetched.bin");
            let (_p, term) = streaming::<FetchReceipt>(
                &socket,
                &Command::Fetch {
                    rid,
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

            // Fast path with seed under a second repo: the bytes are shared
            // by hash, so a (rid2, cid) tag is set without re-downloading.
            let rid2 = RepoId::from_str("rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5").unwrap();
            let (_p, term) = streaming::<FetchReceipt>(
                &socket,
                &Command::Fetch {
                    rid: rid2,
                    cid,
                    locations: vec![],
                    dest: home.path().join("fetched2.bin"),
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
                    cid: unknown,
                    locations: vec![],
                    dest: home.path().join("x.bin"),
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
}
