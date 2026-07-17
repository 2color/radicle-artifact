//! Node-wide, cross-repository views over artifact releases.
//!
//! [`Releases`] is scoped to a single repository. An [`Index`] sits above it and
//! answers "where, across *every* repository in storage, does this CID live?"
//!
//! Lookups always reflect the current COB state: a cached index (see
//! `Index::open_cached`, behind the `sqlite` feature) refreshes each
//! repository's cache before querying it, and an uncached one materializes from
//! git every time. Per-repository failures are logged and skipped; only failing
//! to enumerate repositories is fatal.
//!
//! # Example
//!
//! ```no_run
//! # use radicle::Profile;
//! #
//! # use radicle_artifact::discovery::Index;
//! # use radicle_artifact::Cid;
//! # #[cfg(feature = "sqlite")]
//! # use radicle_artifact::cache_db_path;
//! #
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let profile = Profile::load()?;
//!
//! // Open a discovery index over the local node's storage, reusing its shared
//! // artifact cache (the same db `rad artifact` writes through).
//! #[cfg(feature = "sqlite")]
//! let index = Index::open_cached(&profile.storage, cache_db_path(profile.cobs()));
//!
//! // Without the `sqlite` feature there's no cache; every lookup materializes
//! // releases directly from git.
//! #[cfg(not(feature = "sqlite"))]
//! let index = Index::open(&profile.storage);
//!
//! let cid: Cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi".parse()?;
//!
//! // Every discovery URL for the CID across *every* repository in storage.
//! for location in index.locations_by_cid(&cid)? {
//!     println!("{} in {} from {}", location.url, location.repo, location.did);
//! }
//!
//! // Or the releases that contain the CID, rather than flattened URLs.
//! for m in index.releases_by_cid(&cid)? {
//!     println!("release {} in {}", m.release_id, m.repo);
//! }
//! # Ok(())
//! # }
//! ```

#[cfg(feature = "sqlite")]
use std::path::Path;

use radicle::cob;
use radicle::identity::Did;
use radicle::node::NodeId;
use radicle::prelude::{ReadRepository, ReadStorage, RepoId};
use serde::Serialize;
use url::Url;

#[cfg(feature = "sqlite")]
use crate::cache;
use crate::{Cid, Release, ReleaseId, Releases};

/// A discovery location for a CID, with the repository and release it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    /// Repository the location was published in.
    pub repo: RepoId,
    /// Release COB the location belongs to.
    pub release_id: ReleaseId,
    /// Contributor that published the location.
    pub did: Did,
    /// The discovery URL.
    pub url: Url,
}

/// A release that contains a given CID, with the repository it lives in.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseMatch {
    /// Repository the release lives in.
    pub repo: RepoId,
    /// The release COB id.
    pub release_id: ReleaseId,
    /// The materialized release.
    pub release: Release,
}

/// A node-wide view over every repository's releases.
///
/// Construct with `Index::open_cached` (behind the `sqlite` feature) to share
/// the node's artifact cache, or with [`Index::open`] to read straight from
/// git; query with [`Index::locations_by_cid`] and [`Index::releases_by_cid`].
pub struct Index<'a, S> {
    storage: &'a S,
    #[cfg(feature = "sqlite")]
    cache: Option<cache::Store>,
}

impl<'a, S> Index<'a, S> {
    /// Open a discovery index over `storage`, without a cache. Every lookup
    /// materializes releases directly from git.
    pub fn open(storage: &'a S) -> Self {
        Self {
            storage,
            #[cfg(feature = "sqlite")]
            cache: None,
        }
    }

    /// Open a discovery index over `storage`, backed by a cache at `path`.
    ///
    /// Best-effort, like [`Releases::open_cached`]: if the cache cannot be
    /// opened or migrated, a warning is logged and lookups fall back to materializing
    /// from git (correct, just slower).
    #[cfg(feature = "sqlite")]
    pub fn open_cached(storage: &'a S, path: impl AsRef<Path>) -> Self {
        let cache = match cache::open_writer(path) {
            Ok(cache) => Some(cache),
            Err(err) => {
                log::warn!(target: "artifact", "artifact cache disabled: {err}");
                None
            }
        };
        Self { storage, cache }
    }
}

impl<S> Index<'_, S>
where
    S: ReadStorage,
    S::Repository: ReadRepository + cob::Store<Namespace = NodeId>,
{
    /// Every discovery location for `cid` across all repositories.
    ///
    /// Each repository is opened and refreshed before it is queried, so results
    /// reflect the current COB state. A single repository that fails to open or
    /// read is logged and skipped, so one bad repository never fails the whole
    /// lookup; only failing to enumerate repositories is fatal.
    pub fn locations_by_cid(&self, cid: &Cid) -> Result<Vec<Location>, radicle::storage::Error> {
        let mut out = Vec::new();
        for info in self.storage.repositories()? {
            match self.repo_locations(info.rid, cid) {
                Ok(locations) => out.extend(locations),
                Err(err) => log::warn!(target: "artifact", "skipping repo {}: {err}", info.rid),
            }
        }
        Ok(out)
    }

    /// Every release containing `cid` across all repositories. Same refresh and
    /// skip-on-error semantics as [`Self::locations_by_cid`].
    pub fn releases_by_cid(&self, cid: &Cid) -> Result<Vec<ReleaseMatch>, radicle::storage::Error> {
        let mut out = Vec::new();
        for info in self.storage.repositories()? {
            match self.repo_releases(info.rid, cid) {
                Ok(matches) => out.extend(matches),
                Err(err) => log::warn!(target: "artifact", "skipping repo {}: {err}", info.rid),
            }
        }
        Ok(out)
    }

    /// Open `rid`, refresh its cache, and return its locations for `cid`.
    fn repo_locations(&self, rid: RepoId, cid: &Cid) -> Result<Vec<Location>, error::Repo> {
        let repo = self.storage.repository(rid)?;
        let releases = self.releases(&repo)?;
        Ok(releases
            .locations_for(cid)?
            .into_iter()
            .map(|(release_id, did, url)| Location {
                repo: rid,
                release_id,
                did,
                url,
            })
            .collect())
    }

    /// Open `rid`, refresh its cache, and return its releases matching `cid`.
    fn repo_releases(&self, rid: RepoId, cid: &Cid) -> Result<Vec<ReleaseMatch>, error::Repo> {
        let repo = self.storage.repository(rid)?;
        let releases = self.releases(&repo)?;
        Ok(releases
            .find_by_cid(cid)?
            .into_iter()
            .map(|(release_id, release)| ReleaseMatch {
                repo: rid,
                release_id,
                release,
            })
            .collect())
    }

    /// Build a per-repo [`Releases`] handle, sharing the cache when present.
    fn releases<'r>(
        &self,
        repo: &'r S::Repository,
    ) -> Result<Releases<'r, S::Repository>, error::Repo> {
        let releases = Releases::open(repo)?;
        #[cfg(feature = "sqlite")]
        let releases = match &self.cache {
            Some(cache) => releases.with_cache(cache.clone()),
            None => releases,
        };
        Ok(releases)
    }
}

pub(crate) mod error {
    use thiserror::Error;

    /// A per-repository failure during a cross-repo lookup; logged and skipped.
    #[derive(Debug, Error)]
    pub(crate) enum Repo {
        #[error(transparent)]
        Repository(#[from] radicle::storage::RepositoryError),
        #[error(transparent)]
        Store(#[from] radicle::cob::store::Error),
    }
}
