//! Async control-socket client over tokio sockets, for embedders that
//! already run a tokio runtime (e.g. a desktop app). Enabled by the
//! `tokio` feature, which adds tokio only — never iroh.

use std::path::{Path, PathBuf};
use std::time::Duration;

use radicle::git::Oid;
use radicle::identity::RepoId;
use radicle_artifact_core::cid::Cid;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use radicle_artifact_core::cid::ArtifactKind;
use radicle_artifact_core::protocol::{
    Command, DownloadReceipt, ExportReceipt, FetchProgress, FetchReceipt, HasResult, ImportMode,
    SeedReceipt, SeededEntry, Status, StreamEvent, UnseedReceipt,
};

use crate::{codec, ClientError, DownloadArgs, FetchArgs, DEFAULT_TIMEOUT, PROBE_TIMEOUT};

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
        crate::default_socket(home)
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

        write
            .write_all(codec::encode_command(cmd)?.as_bytes())
            .await?;
        write.flush().await?;

        let mut reader = BufReader::new(read);
        let mut response = String::new();
        let n = reader.read_line(&mut response).await?;
        if n == 0 {
            return Err(ClientError::Eof);
        }
        codec::decode_result(&response)
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

    /// Ask the node to seed `path` against `cid` for `release` in `rid`.
    pub async fn seed(
        &self,
        rid: RepoId,
        release: Oid,
        cid: Cid,
        path: &Path,
        kind: ArtifactKind,
        mode: ImportMode,
    ) -> Result<SeedReceipt, ClientError> {
        let cmd = Command::Seed {
            rid,
            release,
            cid,
            path: path.to_path_buf(),
            kind,
            mode,
        };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// Ask the node to stop seeding `cid` in `rid`. `release: Some(id)` drops
    /// one release's tag; `None` stops seeding the CID across all releases.
    pub async fn unseed(
        &self,
        rid: RepoId,
        release: Option<Oid>,
        cid: Cid,
    ) -> Result<UnseedReceipt, ClientError> {
        self.call(&Command::Unseed { rid, release, cid }, DEFAULT_TIMEOUT)
            .await
    }

    /// Whether the node currently has `(rid, cid)` tagged.
    pub async fn is_seeding(&self, rid: RepoId, cid: Cid) -> Result<bool, ClientError> {
        self.call(&Command::IsSeeding { rid, cid }, DEFAULT_TIMEOUT)
            .await
    }

    /// List CIDs seeded under `rid`.
    pub async fn list_seeded(&self, rid: RepoId) -> Result<Vec<SeededEntry>, ClientError> {
        self.call(&Command::ListSeeded { rid }, DEFAULT_TIMEOUT)
            .await
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
    /// progress to `on_progress`. See `call_streaming` for the
    /// timeout model. Use [`Self::download`] to also export to disk.
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
    /// streaming progress to `on_progress`. See `call_streaming`
    /// for the timeout model.
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

        write
            .write_all(codec::encode_command(cmd)?.as_bytes())
            .await?;
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
            match codec::decode_stream_event::<T>(&buf)? {
                StreamEvent::Progress(p) => on_progress(&p),
                StreamEvent::Okay(v) => return Ok(v),
                StreamEvent::Error(e) => return Err(ClientError::Remote(e)),
            }
        }
    }
}
