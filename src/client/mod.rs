//! Unix-socket client for talking to a running `rad-artifact` node.
//!
//! One `UnixStream` per call: write a JSON-encoded [`Command`] line,
//! read a JSON-encoded [`CommandResult`] line, close. The async API is
//! the primary surface; [`Client::call_blocking`] wraps it in a
//! short-lived tokio runtime for synchronous callers like the CLI.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cid::Cid;
use radicle::identity::RepoId;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{
    Command, CommandError, CommandResult, DownloadReceipt, ExportReceipt, FetchLocation,
    FetchProgress, FetchReceipt, HasResult, ImportMode, SeedReceipt, SeededEntry, Status,
    StreamEvent, UnseedReceipt,
};
use crate::share::cid_utils::ArtifactKind;

/// Default per-call timeout when callers don't pick their own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Short timeout used by [`Client::is_running`] — keep it bounded so a
/// daemon-down probe returns quickly.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Environment variable that overrides the control-socket path.
pub const SOCKET_ENV: &str = "RAD_ARTIFACT_SOCKET";

/// Control-socket client.
///
/// Cheap to clone — wraps only the socket path.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    /// Build a client bound to the given socket path.
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Path the client will dial. Useful for error messages.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Resolve the control-socket path from `RAD_ARTIFACT_SOCKET` if set,
    /// otherwise `<home>/artifacts/control.sock`.
    pub fn default_socket(home: &Path) -> PathBuf {
        if let Ok(s) = std::env::var(SOCKET_ENV) {
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
        home.join(crate::seeder::ARTIFACTS_DIR).join("control.sock")
    }

    /// Send `cmd` and decode the response as `T`.
    ///
    /// Connects, writes one line, reads one line, closes. Honours
    /// `timeout` as a wall-clock cap on the whole round-trip.
    pub async fn call<T>(&self, cmd: &Command, timeout: Duration) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let fut = self.call_inner::<T>(cmd);
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ClientError::Timeout(timeout))?
    }

    async fn call_inner<T>(&self, cmd: &Command) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let stream = UnixStream::connect(&self.socket).await?;
        let (read, mut write) = stream.into_split();

        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        write.write_all(line.as_bytes()).await?;
        write.flush().await?;

        let mut reader = BufReader::new(read);
        let mut response = String::new();
        let n = reader.read_line(&mut response).await?;
        if n == 0 {
            return Err(ClientError::Eof);
        }

        let parsed: CommandResult<T> = serde_json::from_str(response.trim_end())?;
        match parsed {
            CommandResult::Okay(v) => Ok(v),
            CommandResult::Error(e) => Err(ClientError::Remote(e)),
        }
    }

    /// Blocking variant of [`Self::call`] for synchronous callers.
    ///
    /// Builds a single-threaded tokio runtime for the duration of the
    /// call. Cheap enough to use once per CLI invocation; do not call
    /// in a hot loop.
    pub fn call_blocking<T>(&self, cmd: &Command, timeout: Duration) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ClientError::Io)?;
        rt.block_on(self.call(cmd, timeout))
    }

    /// Probe whether a node is reachable on the configured socket.
    ///
    /// Issues a short-deadline [`Command::Alive`] — the cheapest command,
    /// touching no state — and reports success/failure. False covers both
    /// "socket missing" and "owner dead but socket file lingers" — the
    /// parent CLI uses this to decide whether to unlink and rebind.
    pub async fn is_running(&self) -> bool {
        self.call::<()>(&Command::Alive, PROBE_TIMEOUT)
            .await
            .is_ok()
    }

    /// Ask the node to seed `path` against `cid` in `rid`.
    pub async fn seed(
        &self,
        rid: RepoId,
        cid: Cid,
        path: &Path,
        kind: ArtifactKind,
        mode: ImportMode,
    ) -> Result<SeedReceipt, ClientError> {
        let cmd = Command::Seed {
            rid,
            cid,
            path: path.to_path_buf(),
            kind,
            mode,
        };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// Ask the node to stop seeding `(rid, cid)`.
    pub async fn unseed(&self, rid: RepoId, cid: Cid) -> Result<UnseedReceipt, ClientError> {
        let cmd = Command::Unseed { rid, cid };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// Whether the node currently has `(rid, cid)` tagged.
    pub async fn is_seeding(&self, rid: RepoId, cid: Cid) -> Result<bool, ClientError> {
        let cmd = Command::IsSeeding { rid, cid };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// List CIDs seeded under `rid`.
    pub async fn list_seeded(&self, rid: RepoId) -> Result<Vec<SeededEntry>, ClientError> {
        let cmd = Command::ListSeeded { rid };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// Fetch node status.
    pub async fn status(&self) -> Result<Status, ClientError> {
        self.call(&Command::Status, DEFAULT_TIMEOUT).await
    }

    /// Ask the node to shut down. Returns once the node acknowledges,
    /// not once it has fully exited.
    pub async fn shutdown(&self) -> Result<(), ClientError> {
        self.call(&Command::Shutdown, DEFAULT_TIMEOUT).await
    }

    /// Whether the node holds complete (or partial) bytes for `cid`.
    pub async fn has(&self, cid: Cid) -> Result<HasResult, ClientError> {
        self.call(&Command::Has { cid }, DEFAULT_TIMEOUT).await
    }

    /// Fetch an artifact into the node's store (no disk write), streaming
    /// progress to `on_progress`. See `Self::call_streaming` for the timeout
    /// model. Use [`Self::download`] to also export to disk.
    pub async fn fetch(
        &self,
        args: FetchArgs,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<FetchReceipt, ClientError> {
        let cmd = Command::Fetch {
            rid: args.rid,
            cid: args.cid,
            locations: args.locations,
            seed: args.seed,
        };
        self.call_streaming(&cmd, idle, on_progress).await
    }

    /// Download an artifact through the node and export it to `args.dest`,
    /// streaming progress to `on_progress`. See `Self::call_streaming` for
    /// the timeout model.
    pub async fn download(
        &self,
        args: DownloadArgs,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<DownloadReceipt, ClientError> {
        let cmd = Command::Download {
            rid: args.rid,
            cid: args.cid,
            locations: args.locations,
            dest: args.dest,
            seed: args.seed,
        };
        self.call_streaming(&cmd, idle, on_progress).await
    }

    /// Export already-local bytes to `dest`, streaming progress.
    pub async fn export(
        &self,
        cid: Cid,
        dest: PathBuf,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<ExportReceipt, ClientError> {
        self.call_streaming(&Command::Export { cid, dest }, idle, on_progress)
            .await
    }

    /// Drive a streaming command: read frames until the terminal one,
    /// invoking `on_progress` per progress frame and returning the terminal
    /// payload.
    ///
    /// `idle` bounds the wait for *each* frame (reset on every frame),
    /// not the whole transfer — a download that keeps making progress
    /// never times out, but a stall longer than `idle` does.
    async fn call_streaming<T>(
        &self,
        cmd: &Command,
        idle: Duration,
        mut on_progress: impl FnMut(&FetchProgress),
    ) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let stream = UnixStream::connect(&self.socket).await?;
        let (read, mut write) = stream.into_split();

        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        write.write_all(line.as_bytes()).await?;
        write.flush().await?;

        let mut reader = BufReader::new(read);
        loop {
            let mut buf = String::new();
            let n = match tokio::time::timeout(idle, reader.read_line(&mut buf)).await {
                Ok(res) => res?,
                Err(_) => return Err(ClientError::Timeout(idle)),
            };
            if n == 0 {
                return Err(ClientError::Eof);
            }
            match serde_json::from_str::<StreamEvent<T>>(buf.trim_end())? {
                StreamEvent::Progress(p) => on_progress(&p),
                StreamEvent::Okay(v) => return Ok(v),
                StreamEvent::Error(e) => return Err(ClientError::Remote(e)),
            }
        }
    }

    /// Blocking variant of [`Self::fetch`] for synchronous callers (CLI).
    pub fn fetch_blocking(
        &self,
        args: FetchArgs,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<FetchReceipt, ClientError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ClientError::Io)?;
        rt.block_on(self.fetch(args, idle, on_progress))
    }

    /// Blocking variant of [`Self::download`] for synchronous callers (CLI).
    pub fn download_blocking(
        &self,
        args: DownloadArgs,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<DownloadReceipt, ClientError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ClientError::Io)?;
        rt.block_on(self.download(args, idle, on_progress))
    }

    /// Blocking variant of [`Self::export`] for synchronous callers (CLI).
    pub fn export_blocking(
        &self,
        cid: Cid,
        dest: PathBuf,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<ExportReceipt, ClientError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ClientError::Io)?;
        rt.block_on(self.export(cid, dest, idle, on_progress))
    }
}

/// Arguments for [`Client::fetch`]; mirrors [`Command::Fetch`].
#[derive(Debug, Clone)]
pub struct FetchArgs {
    /// Repository the artifact belongs to (for the seeded tag).
    pub rid: RepoId,
    /// Content identifier to fetch.
    pub cid: Cid,
    /// Resolved providers/URLs to try.
    pub locations: Vec<FetchLocation>,
    /// Whether to tag the artifact as seeded after fetching.
    pub seed: bool,
}

/// Arguments for [`Client::download`]; mirrors [`Command::Download`].
#[derive(Debug, Clone)]
pub struct DownloadArgs {
    /// Repository the artifact belongs to (for the seeded tag).
    pub rid: RepoId,
    /// Content identifier to download.
    pub cid: Cid,
    /// Resolved providers/URLs to try.
    pub locations: Vec<FetchLocation>,
    /// Destination path the bytes are exported to.
    pub dest: PathBuf,
    /// Whether to tag the artifact as seeded after downloading.
    pub seed: bool,
}

/// Failure modes when calling the node.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Local I/O failure (e.g. socket does not exist, connection refused).
    #[error("client I/O error: {0}")]
    Io(#[from] io::Error),
    /// JSON encode/decode failure on the wire.
    #[error("client JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Server closed the connection before sending a response.
    #[error("node closed the connection without responding")]
    Eof,
    /// Round-trip exceeded the supplied timeout.
    #[error("call timed out after {0:?}")]
    Timeout(Duration),
    /// Structured error returned by the node.
    #[error("node error: {0:?}: {message}", message = .0.message)]
    Remote(CommandError),
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::seeder::ARTIFACTS_DIR;
    use crate::share::cid_utils::{self, ArtifactKind};

    /// Exercise the streaming client reader (`has`, `export`, `fetch`,
    /// `download`) end-to-end against a real node.
    #[test]
    fn streaming_methods_round_trip() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let payload = b"hello client streaming";
            let blob_path = home.path().join("payload.bin");
            std::fs::write(&blob_path, payload).unwrap();
            let cid = cid_utils::compute_blob_cid(&blob_path).unwrap();
            let rid = RepoId::from_str("rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip").unwrap();

            let secret = iroh::SecretKey::from_bytes(&[8u8; 32]);
            let home_path = home.path().to_path_buf();
            let node = tokio::spawn(async move { crate::node::run(&home_path, secret).await });

            let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(socket.exists());

            let client = Client::new(socket);
            client
                .seed(rid, cid, &blob_path, ArtifactKind::Blob, ImportMode::Copy)
                .await
                .unwrap();

            // has
            let h = client.has(cid).await.unwrap();
            assert!(h.present && h.complete);
            assert_eq!(h.bytes, payload.len() as u64);

            // export streams to disk; count the progress frames.
            let dest = home.path().join("exported.bin");
            let mut progress = 0;
            let receipt = client
                .export(cid, dest.clone(), Duration::from_secs(30), |_| {
                    progress += 1
                })
                .await
                .unwrap();
            assert_eq!(receipt.bytes, payload.len() as u64);
            assert_eq!(std::fs::read(&dest).unwrap(), payload);
            assert!(progress >= 1, "expected at least one progress frame");

            // fetch fast-path: already local, no locations, no disk write.
            let fetched = client
                .fetch(
                    FetchArgs {
                        rid,
                        cid,
                        locations: vec![],
                        seed: false,
                    },
                    Duration::from_secs(30),
                    |_| {},
                )
                .await
                .unwrap();
            assert!(fetched.from_cache);
            assert_eq!(fetched.bytes, payload.len() as u64);

            // download fast-path: already local, exported to disk.
            let dl_dest = home.path().join("downloaded.bin");
            let downloaded = client
                .download(
                    DownloadArgs {
                        rid,
                        cid,
                        locations: vec![],
                        dest: dl_dest.clone(),
                        seed: false,
                    },
                    Duration::from_secs(30),
                    |_| {},
                )
                .await
                .unwrap();
            assert!(downloaded.from_cache);
            assert_eq!(downloaded.bytes, payload.len() as u64);
            assert_eq!(std::fs::read(&dl_dest).unwrap(), payload);

            client.shutdown().await.unwrap();
            node.await.unwrap().unwrap();
        });
    }
}
