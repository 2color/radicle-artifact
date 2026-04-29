//! Open an artifact repository from a remote `radicle-httpd` seed without
//! requiring a local Radicle node.
//!
//! A bare git clone of the repo is maintained under
//! `$XDG_CACHE_HOME/radicle-artifact/repos/` (falling back to
//! `~/.cache/radicle-artifact/repos/`). On every call the cache is
//! re-fetched so the caller sees the seed's current state.

use std::path::PathBuf;
use std::process::Command;

use radicle::git::raw;
use radicle::prelude::RepoId;
use radicle::storage::{git::Repository, RepositoryError};
use thiserror::Error;
use url::Url;

/// Errors that can occur opening a remote-backed repository.
#[derive(Debug, Error)]
pub enum Error {
    /// Neither `XDG_CACHE_HOME` nor `HOME` is set, so we can't pick a cache directory.
    #[error("could not determine cache directory; set XDG_CACHE_HOME or HOME")]
    NoCacheDir,
    /// Could not create the cache directory.
    #[error("failed to create cache directory {path}")]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        err: std::io::Error,
    },
    /// Could not init or open the bare cache repository.
    #[error("failed to initialize cache repository at {path}")]
    Init {
        /// Cache path.
        path: PathBuf,
        /// Underlying git error.
        #[source]
        err: raw::Error,
    },
    /// Could not spawn the `git` binary.
    #[error("failed to spawn `git`; is it installed and on PATH?")]
    GitSpawn(#[source] std::io::Error),
    /// `git fetch` against the seed exited with an error.
    #[error("`git fetch {url}` failed: {stderr}")]
    Fetch {
        /// Remote URL.
        url: String,
        /// Captured stderr from the git invocation.
        stderr: String,
    },
    /// Opening the cache repo as a `radicle::storage::git::Repository` failed.
    #[error("failed to open cached repository at {path}")]
    Open {
        /// Cache path.
        path: PathBuf,
        /// Underlying radicle storage error.
        #[source]
        err: RepositoryError,
    },
}

/// Resolve the radicle-artifact cache root.
///
/// Honors `XDG_CACHE_HOME` first, falls back to `~/.cache`. We don't use the
/// `dirs` crate here — five lines of `std::env` matches what cargo / rustup
/// do on macOS and avoids an extra dep for one path lookup.
fn cache_dir() -> Result<PathBuf, Error> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Ok(PathBuf::from(xdg).join("radicle-artifact"))
    } else if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        Ok(PathBuf::from(home).join(".cache").join("radicle-artifact"))
    } else {
        Err(Error::NoCacheDir)
    }
}

/// Open `rid` from a `radicle-httpd` seed, fetching the latest refs into the
/// local cache.
///
/// The fetched refs cover everything `Releases::open` needs: COBs live under
/// `refs/namespaces/<nid>/refs/cobs/...`, the canonical identity under
/// `refs/rad/id`, plus heads and tags for revision lookups.
pub fn open(seed: &Url, rid: RepoId) -> Result<Repository, Error> {
    let dir = cache_dir()?.join("repos").join(format!("{rid}.git"));
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|err| Error::CreateDir {
            path: parent.to_path_buf(),
            err,
        })?;
    }

    if !dir.exists() {
        raw::Repository::init_bare(&dir).map_err(|err| Error::Init {
            path: dir.clone(),
            err,
        })?;
    }

    // String-concat the URL: `Url::join` would replace the last path
    // segment when the seed lacks a trailing slash, which is the common
    // form users will pass.
    let url = format!("{}/{}.git", seed.as_str().trim_end_matches('/'), rid);
    fetch(&dir, &url)?;

    Repository::open(&dir, rid).map_err(|err| Error::Open { path: dir, err })
}

fn fetch(dir: &std::path::Path, url: &str) -> Result<(), Error> {
    // Shelling out to `git` avoids depending on libgit2 being built with
    // TLS support (the bundled libgit2-sys often isn't), and `git` is
    // already a hard requirement for any Radicle workflow. Explicit
    // refspecs cover everything `Releases::open` reads: branches/tags for
    // revspec resolution, `refs/namespaces/*` for COBs (release ops live
    // there), and `refs/rad/*` for canonical identity / delegate refs.
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .arg("fetch")
        .arg("--quiet")
        .arg(url)
        .arg("+refs/heads/*:refs/heads/*")
        .arg("+refs/tags/*:refs/tags/*")
        .arg("+refs/namespaces/*:refs/namespaces/*")
        .arg("+refs/rad/*:refs/rad/*")
        .output()
        .map_err(Error::GitSpawn)?;
    if !output.status.success() {
        return Err(Error::Fetch {
            url: url.to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}
