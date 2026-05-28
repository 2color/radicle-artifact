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
use iroh_blobs::store::fs::FsStore;
use radicle::identity::RepoId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;

use crate::client::Client;
use crate::protocol::{
    Command, CommandError, CommandResult, ErrorCode, ImportMode, SeedReceipt, SeededEntry, Status,
    UnseedReceipt,
};
use crate::seeder::{self, ARTIFACTS_DIR};
use crate::share::cid_utils::ArtifactKind;
use crate::share::keys::EndpointId;
use crate::share::Error as ShareError;

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
    let store: FsStore = seeder.blobs.clone();

    tracing::info!(
        endpoint_id = %endpoint_id,
        socket = %socket_path.display(),
        "rad-artifact node ready"
    );

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
                let store = store.clone();
                let shutdown_tx = shutdown_tx.clone();
                let in_flight = in_flight.clone();
                in_flight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(
                        stream,
                        &store,
                        started_at_unix,
                        endpoint_id,
                        &shutdown_tx,
                    )
                    .await
                    {
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

/// Read one command, write one response, close.
async fn handle_connection(
    stream: UnixStream,
    store: &FsStore,
    started_at_unix: i64,
    endpoint_id: EndpointId,
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

    let mut response = match serde_json::from_str::<Command>(line.trim_end()) {
        Ok(cmd) => dispatch(cmd, store, started_at_unix, endpoint_id, shutdown_tx).await,
        Err(e) => err_json::<()>(ErrorCode::Internal, format!("invalid command JSON: {e}")),
    };
    response.push('\n');
    write.write_all(response.as_bytes()).await?;
    write.flush().await
}

/// Dispatch a parsed command, returning the JSON line to send back
/// (without trailing newline).
async fn dispatch(
    cmd: Command,
    store: &FsStore,
    started_at_unix: i64,
    endpoint_id: EndpointId,
    shutdown_tx: &broadcast::Sender<()>,
) -> String {
    match cmd {
        Command::Status => match build_status(store, endpoint_id, started_at_unix).await {
            Ok(status) => ok_json(status),
            Err(e) => err_from_share::<Status>(e),
        },
        Command::Seed {
            rid,
            cid,
            path,
            kind,
            mode,
        } => seed_response(store, &rid, &cid, &path, kind, mode, endpoint_id).await,
        Command::Unseed { rid, cid } => unseed_response(store, &rid, &cid).await,
        Command::IsSeeding { rid, cid } => is_seeding_response(store, &rid, &cid).await,
        Command::ListSeeded { rid } => list_seeded_response(store, &rid).await,
        Command::Shutdown => {
            // ack first, then broadcast so the loop tears down after the
            // response makes it to the wire
            let resp = ok_json(());
            let _ = shutdown_tx.send(());
            resp
        }
    }
}

async fn seed_response(
    store: &FsStore,
    rid_s: &str,
    cid_s: &str,
    path: &Path,
    kind: ArtifactKind,
    mode: ImportMode,
    endpoint_id: EndpointId,
) -> String {
    let rid = match RepoId::from_str(rid_s) {
        Ok(v) => v,
        Err(e) => return err_json::<SeedReceipt>(ErrorCode::Internal, format!("invalid rid: {e}")),
    };
    let cid = match Cid::from_str(cid_s) {
        Ok(v) => v,
        Err(e) => return err_json::<SeedReceipt>(ErrorCode::Internal, format!("invalid cid: {e}")),
    };
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
        rid: rid.to_string(),
        cid: cid.to_string(),
        endpoint_id,
        bytes,
        was_new: !was_already,
    };
    ok_json(receipt)
}

async fn unseed_response(store: &FsStore, rid_s: &str, cid_s: &str) -> String {
    let rid = match RepoId::from_str(rid_s) {
        Ok(v) => v,
        Err(e) => {
            return err_json::<UnseedReceipt>(ErrorCode::Internal, format!("invalid rid: {e}"))
        }
    };
    let cid = match Cid::from_str(cid_s) {
        Ok(v) => v,
        Err(e) => {
            return err_json::<UnseedReceipt>(ErrorCode::Internal, format!("invalid cid: {e}"))
        }
    };
    let was_seeded = match seeder::is_seeded(store, &rid, &cid).await {
        Ok(v) => v,
        Err(e) => return err_from_share::<UnseedReceipt>(e),
    };
    if let Err(e) = seeder::unregister_seeded(store, &rid, &cid).await {
        return err_from_share::<UnseedReceipt>(e);
    }
    ok_json(UnseedReceipt {
        rid: rid.to_string(),
        cid: cid.to_string(),
        was_removed: was_seeded,
    })
}

async fn is_seeding_response(store: &FsStore, rid_s: &str, cid_s: &str) -> String {
    let rid = match RepoId::from_str(rid_s) {
        Ok(v) => v,
        Err(e) => return err_json::<bool>(ErrorCode::Internal, format!("invalid rid: {e}")),
    };
    let cid = match Cid::from_str(cid_s) {
        Ok(v) => v,
        Err(e) => return err_json::<bool>(ErrorCode::Internal, format!("invalid cid: {e}")),
    };
    match seeder::is_seeded(store, &rid, &cid).await {
        Ok(v) => ok_json(v),
        Err(e) => err_from_share::<bool>(e),
    }
}

async fn list_seeded_response(store: &FsStore, rid_s: &str) -> String {
    let rid = match RepoId::from_str(rid_s) {
        Ok(v) => v,
        Err(e) => {
            return err_json::<Vec<SeededEntry>>(ErrorCode::Internal, format!("invalid rid: {e}"))
        }
    };
    let cids = match seeder::seeded_cids(store, &rid).await {
        Ok(v) => v,
        Err(e) => return err_from_share::<Vec<SeededEntry>>(e),
    };
    let mut out = Vec::with_capacity(cids.len());
    for cid in cids {
        let bytes = seeder::artifact_size(store, &rid, &cid).await;
        out.push(SeededEntry {
            cid: cid.to_string(),
            bytes,
        });
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
            let cid_str = real_cid.to_string();
            let rid = rid_a();
            let rid_str = rid.to_string();

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
                    &rid_str,
                    &cid_str,
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
                    &rid_str,
                    &cid_str,
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
                    &rid_str,
                    &bad_cid.to_string(),
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
            assert!(client.is_seeding(&rid_str, &cid_str).await.unwrap());

            // ListSeeded returns exactly the one entry.
            let entries = client.list_seeded(&rid_str).await.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].cid, cid_str);
            assert_eq!(entries[0].bytes, payload.len() as u64);

            // Status now reports one seeded artifact.
            let status = client.status().await.unwrap();
            assert_eq!(status.seeded.count, 1);
            assert_eq!(status.seeded.bytes_logical, payload.len() as u64);

            // Unseed once removes the tag; second call is idempotent.
            let r1 = client.unseed(&rid_str, &cid_str).await.unwrap();
            assert!(r1.was_removed);
            let r2 = client.unseed(&rid_str, &cid_str).await.unwrap();
            assert!(!r2.was_removed);
            assert!(!client.is_seeding(&rid_str, &cid_str).await.unwrap());

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
}
