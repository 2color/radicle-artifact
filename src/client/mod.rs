//! Unix-socket client for talking to a running `rad-artifact` node.
//!
//! One `UnixStream` per call: write a JSON-encoded [`Command`] line,
//! read a JSON-encoded [`CommandResult`] line, close. The async API is
//! the primary surface; [`Client::call_blocking`] wraps it in a
//! short-lived tokio runtime for synchronous callers like the CLI.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{
    Command, CommandError, CommandResult, ImportMode, SeedReceipt, SeededEntry, Status,
    UnseedReceipt,
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
    /// Issues a short-deadline [`Command::Status`] and reports
    /// success/failure. False covers both "socket missing" and "owner
    /// dead but socket file lingers" — the parent CLI uses this to
    /// decide whether to unlink and rebind.
    pub async fn is_running(&self) -> bool {
        self.call::<Status>(&Command::Status, PROBE_TIMEOUT)
            .await
            .is_ok()
    }

    /// Ask the node to seed `path` against `cid` in `rid`.
    pub async fn seed(
        &self,
        rid: &str,
        cid: &str,
        path: &Path,
        kind: ArtifactKind,
        mode: ImportMode,
    ) -> Result<SeedReceipt, ClientError> {
        let cmd = Command::Seed {
            rid: rid.to_string(),
            cid: cid.to_string(),
            path: path.to_path_buf(),
            kind,
            mode,
        };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// Ask the node to stop seeding `(rid, cid)`.
    pub async fn unseed(&self, rid: &str, cid: &str) -> Result<UnseedReceipt, ClientError> {
        let cmd = Command::Unseed {
            rid: rid.to_string(),
            cid: cid.to_string(),
        };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// Whether the node currently has `(rid, cid)` tagged.
    pub async fn is_seeding(&self, rid: &str, cid: &str) -> Result<bool, ClientError> {
        let cmd = Command::IsSeeding {
            rid: rid.to_string(),
            cid: cid.to_string(),
        };
        self.call(&cmd, DEFAULT_TIMEOUT).await
    }

    /// List CIDs seeded under `rid`.
    pub async fn list_seeded(&self, rid: &str) -> Result<Vec<SeededEntry>, ClientError> {
        let cmd = Command::ListSeeded {
            rid: rid.to_string(),
        };
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
