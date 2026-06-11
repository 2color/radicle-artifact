//! Unix-socket client for talking to a running `rad-artifact` node.
//!
//! One `UnixStream` per call: write a JSON-encoded
//! [`Command`](radicle_artifact_core::protocol::Command) line, read a
//! JSON-encoded [`CommandResult`](radicle_artifact_core::protocol::CommandResult)
//! line, close. Two transports over the same wire format:
//!
//! - [`sync`] (always available): `std` sockets, no runtime. The CLI's
//!   transport.
//! - [`tokio`] (feature `tokio`): async, for embedders that already run
//!   a tokio runtime. The feature adds tokio only — never iroh.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cid::Cid;
use radicle::identity::RepoId;
use radicle_artifact_core::protocol::{CommandError, FetchLocation};

pub mod codec;
pub mod sync;
#[cfg(feature = "tokio")]
pub mod tokio;

/// Default per-call timeout when callers don't pick their own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Short timeout used by `is_running` probes — keep it bounded so a
/// daemon-down probe returns quickly.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Environment variable that overrides the control-socket path.
pub const SOCKET_ENV: &str = "RAD_ARTIFACT_SOCKET";

/// Resolve the control-socket path from `RAD_ARTIFACT_SOCKET` if set,
/// otherwise `<home>/artifacts/control.sock`.
pub fn default_socket(home: &Path) -> PathBuf {
    if let Ok(s) = std::env::var(SOCKET_ENV) {
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    home.join(radicle_artifact_core::ARTIFACTS_DIR)
        .join("control.sock")
}

/// Arguments for a fetch call; mirrors `Command::Fetch`.
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

/// Arguments for a download call; mirrors `Command::Download`.
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
