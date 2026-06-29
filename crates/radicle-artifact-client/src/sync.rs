//! Synchronous control-socket client over `std` sockets — no async
//! runtime. This is the CLI's transport.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use radicle::git::Oid;
use radicle::identity::RepoId;
use radicle_artifact_core::cid::Cid;
use serde::de::DeserializeOwned;

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

    /// Connect with `timeout` applied to both socket directions.
    ///
    /// Unlike the async transport's wall-clock cap on the whole
    /// round-trip, the timeout here bounds each blocking socket
    /// operation; a worst-case call can take a small multiple of it.
    fn connect(&self, timeout: Duration) -> Result<UnixStream, ClientError> {
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(stream)
    }

    /// Map a read-timeout I/O error onto [`ClientError::Timeout`].
    fn map_timeout(e: ClientError, timeout: Duration) -> ClientError {
        match e {
            ClientError::Io(io)
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                ClientError::Timeout(timeout)
            }
            other => other,
        }
    }

    /// Send `cmd` and decode the response as `T`.
    ///
    /// Connects, writes one line, reads one line, closes.
    pub fn call<T>(&self, cmd: &Command, timeout: Duration) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        self.call_inner(cmd, timeout)
            .map_err(|e| Self::map_timeout(e, timeout))
    }

    fn call_inner<T>(&self, cmd: &Command, timeout: Duration) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let mut stream = self.connect(timeout)?;
        stream.write_all(codec::encode_command(cmd)?.as_bytes())?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        let n = reader.read_line(&mut response)?;
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
    pub fn is_running(&self) -> bool {
        self.call::<()>(&Command::Alive, PROBE_TIMEOUT).is_ok()
    }

    /// Ask the node to seed `path` against `cid` for `release` in `rid`.
    pub fn seed(
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
        self.call(&cmd, DEFAULT_TIMEOUT)
    }

    /// Ask the node to stop seeding `cid` in `rid`. `release: Some(id)` drops
    /// one release's tag; `None` stops seeding the CID across all releases.
    pub fn unseed(
        &self,
        rid: RepoId,
        release: Option<Oid>,
        cid: Cid,
    ) -> Result<UnseedReceipt, ClientError> {
        self.call(&Command::Unseed { rid, release, cid }, DEFAULT_TIMEOUT)
    }

    /// Whether the node currently has `(rid, cid)` tagged.
    pub fn is_seeding(&self, rid: RepoId, cid: Cid) -> Result<bool, ClientError> {
        self.call(&Command::IsSeeding { rid, cid }, DEFAULT_TIMEOUT)
    }

    /// List CIDs seeded under `rid`.
    pub fn list_seeded(&self, rid: RepoId) -> Result<Vec<SeededEntry>, ClientError> {
        self.call(&Command::ListSeeded { rid }, DEFAULT_TIMEOUT)
    }

    /// Fetch node status.
    pub fn status(&self) -> Result<Status, ClientError> {
        self.call(&Command::Status, DEFAULT_TIMEOUT)
    }

    /// Ask the node to shut down. Returns once the node acknowledges,
    /// not once it has fully exited.
    pub fn shutdown(&self) -> Result<(), ClientError> {
        self.call(&Command::Shutdown, DEFAULT_TIMEOUT)
    }

    /// Whether the node holds complete (or partial) bytes for `cid`.
    pub fn has(&self, cid: Cid) -> Result<HasResult, ClientError> {
        self.call(&Command::Has { cid }, DEFAULT_TIMEOUT)
    }

    /// Fetch an artifact into the node's store (no disk write), streaming
    /// progress to `on_progress`. See `call_streaming` for the
    /// timeout model. Use [`Self::download`] to also export to disk.
    pub fn fetch(
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
        self.call_streaming(&cmd, idle, on_progress)
    }

    /// Download an artifact through the node and export it to `args.dest`,
    /// streaming progress to `on_progress`. See `call_streaming`
    /// for the timeout model.
    pub fn download(
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
        self.call_streaming(&cmd, idle, on_progress)
    }

    /// Export already-local bytes to `dest`, streaming progress.
    pub fn export(
        &self,
        cid: Cid,
        dest: PathBuf,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<ExportReceipt, ClientError> {
        self.call_streaming(&Command::Export { cid, dest }, idle, on_progress)
    }

    /// Drive a streaming command: read frames until the terminal one,
    /// invoking `on_progress` per progress frame and returning the terminal
    /// payload.
    ///
    /// `idle` bounds the wait for *each* frame (the socket read timeout),
    /// not the whole transfer — a download that keeps making progress
    /// never times out, but a stall longer than `idle` does.
    fn call_streaming<T>(
        &self,
        cmd: &Command,
        idle: Duration,
        mut on_progress: impl FnMut(&FetchProgress),
    ) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let mut inner = || -> Result<T, ClientError> {
            let mut stream = self.connect(idle)?;
            stream.write_all(codec::encode_command(cmd)?.as_bytes())?;
            stream.flush()?;

            let mut reader = BufReader::new(stream);
            loop {
                let mut buf = String::new();
                let n = reader.read_line(&mut buf)?;
                if n == 0 {
                    return Err(ClientError::Eof);
                }
                match codec::decode_stream_event::<T>(&buf)? {
                    StreamEvent::Progress(p) => on_progress(&p),
                    StreamEvent::Okay(v) => return Ok(v),
                    StreamEvent::Error(e) => return Err(ClientError::Remote(e)),
                }
            }
        };
        inner().map_err(|e| Self::map_timeout(e, idle))
    }
}
