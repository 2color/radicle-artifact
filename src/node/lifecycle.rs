//! Parent-CLI helpers for `rad-artifact node start`.
//!
//! In foreground mode the CLI calls [`crate::node::run`] directly. In
//! detached mode the parent CLI:
//! 1. resolves the keystore passphrase from `RAD_PASSPHRASE` or prompts
//! 2. rotates `<home>/artifacts/node.log`
//! 3. spawns its own binary with `node start --foreground`, redirecting
//!    stdout/stderr to the log file and exporting `RAD_PASSPHRASE` to
//!    the child
//! 4. polls [`Client::is_running`] for ~6 s to confirm a healthy start
//!
//! No PID file. The socket alone is the lifecycle marker; the parent
//! decides "is a node up?" via the socket probe.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use radicle::crypto::ssh::keystore::{Keystore, Passphrase};

use crate::client::Client;
use crate::seeder::ARTIFACTS_DIR;

/// Environment variable carrying the keystore passphrase from the
/// parent CLI to the spawned foreground node.
pub const PASSPHRASE_ENV: &str = "RAD_PASSPHRASE";

/// How long the parent CLI waits for the child to come online before
/// surfacing a "see node logs" error.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(6);

/// Polling interval inside [`wait_until_running`].
pub const POLL_INTERVAL: Duration = Duration::from_millis(60);

/// Failure modes for parent-side lifecycle operations.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// Local I/O failure (mkdir, file open, spawn).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Could not query the keystore.
    #[error("keystore error: {0}")]
    Keystore(String),
    /// Encrypted keystore needs a passphrase but no terminal is
    /// available for prompting.
    #[error("encrypted keystore requires RAD_PASSPHRASE (no terminal for prompt)")]
    PassphraseNoTerminal,
    /// Interactive passphrase prompt was cancelled or failed.
    #[error("passphrase prompt failed: {0}")]
    PassphrasePrompt(String),
    /// Child exited or never bound the socket within [`STARTUP_TIMEOUT`].
    #[error("node did not come online within {0:?}; see `{1}` or run `rad-artifact node start --foreground` to diagnose")]
    StartupTimeout(Duration, PathBuf),
}

/// Path to the node's log file: `<home>/artifacts/node.log`.
pub fn log_path(home: &Path) -> PathBuf {
    home.join(ARTIFACTS_DIR).join("node.log")
}

/// Path to the rotated previous log: `<home>/artifacts/node.log.1`.
pub fn rotated_log_path(home: &Path) -> PathBuf {
    home.join(ARTIFACTS_DIR).join("node.log.1")
}

/// Rotate the log on each restart: `node.log` → `node.log.1`. Drops
/// the previous rotation if one exists. No-op when the current log
/// doesn't exist.
pub fn rotate_log(home: &Path) -> Result<(), LifecycleError> {
    let current = log_path(home);
    let rotated = rotated_log_path(home);
    if let Some(parent) = current.parent() {
        fs::create_dir_all(parent)?;
    }
    if current.exists() {
        // Discard any prior rotation, then move current → .1.
        let _ = fs::remove_file(&rotated);
        fs::rename(&current, &rotated)?;
    }
    Ok(())
}

/// Open the log file for the detached child's stdout/stderr in append
/// mode. Append (rather than truncate) preserves any text the user
/// directed into the file out-of-band.
fn open_log_file(home: &Path) -> Result<fs::File, LifecycleError> {
    let path = log_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(LifecycleError::Io)
}

/// Resolve the keystore passphrase for starting the daemon.
///
/// Returns `Ok(None)` for non-encrypted keystores (the caller passes
/// `None` to [`crate::share::keys::radicle_secret_to_iroh`]). For
/// encrypted keystores: prefer `RAD_PASSPHRASE`, fall back to an
/// interactive prompt — fails when stderr is not a terminal.
pub fn resolve_passphrase(keystore: &Keystore) -> Result<Option<Passphrase>, LifecycleError> {
    let encrypted = keystore
        .is_encrypted()
        .map_err(|e| LifecycleError::Keystore(e.to_string()))?;
    if !encrypted {
        return Ok(None);
    }
    if let Some(p) = radicle::profile::env::passphrase() {
        return Ok(Some(p));
    }
    if !std::io::stderr().is_terminal() {
        return Err(LifecycleError::PassphraseNoTerminal);
    }
    let entered = inquire::Password::new("Enter passphrase to unlock your radicle key:")
        .with_display_mode(inquire::PasswordDisplayMode::Masked)
        .without_confirmation()
        .prompt()
        .map_err(|e| LifecycleError::PassphrasePrompt(e.to_string()))?;
    Ok(Some(Passphrase::from(entered)))
}

/// Spawn `rad-artifact node start --foreground` detached from the
/// current process.
///
/// The child inherits no stdin, gets stdout/stderr redirected to
/// `<home>/artifacts/node.log`, and receives `RAD_PASSPHRASE` when
/// `passphrase` is `Some`. The child handle is dropped immediately so
/// the parent does not wait on the daemon — once this function returns
/// the child runs independently.
pub fn spawn_detached(
    home: &Path,
    passphrase: Option<&Passphrase>,
    force: bool,
) -> Result<(), LifecycleError> {
    let exe = std::env::current_exe()?;
    let log = open_log_file(home)?;
    // The Command takes ownership of the writer; clone the fd so both
    // stdout and stderr land in the same file.
    let stderr_dup = log.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("node").arg("start").arg("--foreground");
    if force {
        cmd.arg("--force");
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(stderr_dup));
    if let Some(p) = passphrase {
        cmd.env(PASSPHRASE_ENV, p.as_str());
    }
    cmd.spawn()?;
    Ok(())
}

/// Poll the control socket until the node answers a [`Client::is_running`]
/// probe, or `deadline` elapses.
///
/// Builds a single-threaded tokio runtime; safe to call from a
/// synchronous parent CLI command.
pub fn wait_until_running(socket: &Path, deadline: Duration) -> bool {
    let client = Client::new(socket.to_path_buf());
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    rt.block_on(async {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if client.is_running().await {
                return true;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn rotate_log_moves_current_to_dot_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let log = log_path(home);
        let rotated = rotated_log_path(home);

        // First rotation with no log present is a no-op.
        rotate_log(home).unwrap();
        assert!(!log.exists());
        assert!(!rotated.exists());

        // Create a log, rotate — should land at .1.
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, b"first run").unwrap();
        rotate_log(home).unwrap();
        assert!(!log.exists());
        assert_eq!(fs::read(&rotated).unwrap(), b"first run");

        // Second rotation drops the older .1 and replaces with the new one.
        fs::write(&log, b"second run").unwrap();
        rotate_log(home).unwrap();
        assert_eq!(fs::read(&rotated).unwrap(), b"second run");
    }

    #[test]
    fn log_path_is_under_artifacts_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = log_path(tmp.path());
        let parent = path.parent().unwrap().file_name().unwrap();
        assert_eq!(parent, ARTIFACTS_DIR);
        assert_eq!(path.file_name().unwrap(), "node.log");
    }
}
