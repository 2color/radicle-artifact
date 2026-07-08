//! SQLite cache for the artifact [`Release`] COB.
//!
//! Reads of the `Release` COB otherwise re-fold every action from git storage
//! on every call. This module stores each materialized `Release` as a JSON blob
//! plus a normalized `locations` index, so steady-state reads become SQLite
//! queries with, at most, a cheap git-ref walk to check freshness (see the
//! `Releases` read paths in the crate root).
//!
//! The cache mirrors heartwood's issue/patch caches (`radicle::cob::cache`) but
//! is owned here, with its own database file and migrations. Because reads
//! populate the cache lazily (a stale entry is re-folded on read), the handle is
//! always writable; there is no read-only variant. Freshness is validated on
//! read against the COB's tip refs rather than via a git-fetch hook (which this
//! crate has no access to).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use radicle::git::Oid;
use radicle::identity::Did;
use radicle::prelude::RepoId;
use sqlite as sql;
use url::Url;

use crate::{Cid, Release, ReleaseId};

/// How long to wait for the database lock before failing.
const DB_TIMEOUT: Duration = Duration::from_secs(6);

/// Filename of the cache database within a node's COBs directory. An internal
/// detail of this module; callers resolve the full path via [`db_path`] rather
/// than hardcoding the name.
const DB_FILE: &str = "artifacts.db";

/// Ordered database migrations. Each entry is applied once, in order, bumping
/// `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[include_str!("cache/migrations/1.sql")];

/// A cache error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An error from the underlying SQLite database.
    #[error("sqlite: {0}")]
    Sql(#[from] sql::Error),
    /// A cached blob failed to (de)serialize.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A cached value could not be parsed back into its type.
    #[error("parse: {0}")]
    Parse(String),
    /// A query that expected a row returned none.
    #[error("no rows returned")]
    NoRows,
}

/// A file-backed SQLite cache of materialized [`Release`]s.
#[derive(Clone)]
pub struct Store {
    db: Arc<sql::ConnectionThreadSafe>,
}

/// Resolve the cache database path within a node's COBs directory (e.g.
/// `profile.cobs()`). Keeps the [`DB_FILE`] name owned by this module.
pub fn db_path(cobs_dir: impl AsRef<Path>) -> PathBuf {
    cobs_dir.as_ref().join(DB_FILE)
}

/// Open a cache at `path` and migrate it to the latest schema.
pub fn open_writer(path: impl AsRef<Path>) -> Result<Store, Error> {
    let mut store = Store::open(path)?;
    store.migrate()?;
    Ok(store)
}

impl Store {
    /// Open (creating if needed) a cache at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let mut db = sql::Connection::open_thread_safe(path)?;
        db.set_busy_timeout(DB_TIMEOUT.as_millis() as usize)?;
        // WAL lets the daemon and CLI read concurrently with a single writer;
        // synchronous=NORMAL is the safe, fast pairing for WAL (the cache is
        // rebuildable, so the small crash-durability tradeoff is irrelevant).
        // Both are harmless no-ops for in-memory databases.
        let _ = db.execute("PRAGMA journal_mode = WAL");
        let _ = db.execute("PRAGMA synchronous = NORMAL");
        Ok(Self { db: Arc::new(db) })
    }

    /// Create a new in-memory cache (for tests).
    #[cfg(test)]
    pub fn memory() -> Result<Self, Error> {
        Ok(Self {
            db: Arc::new(sql::Connection::open_thread_safe(":memory:")?),
        })
    }

    /// Builder variant of [`Self::migrate`] (for tests).
    #[cfg(test)]
    pub fn with_migrations(mut self) -> Result<Self, Error> {
        self.migrate()?;
        Ok(self)
    }

    /// Migrate to the latest schema; returns the resulting version. Idempotent,
    /// and safe against concurrent processes (a single write transaction
    /// serializes migration).
    pub fn migrate(&mut self) -> Result<usize, Error> {
        transaction(&self.db, |db| {
            let mut version = user_version(db)?;
            while version < MIGRATIONS.len() {
                db.execute(MIGRATIONS[version])?;
                db.execute(format!("PRAGMA user_version = {}", version + 1))?;
                version += 1;
            }
            Ok(version)
        })
    }

    /// Insert or replace a cached release, rebuilding its `locations` rows.
    pub fn update(
        &self,
        repo: &RepoId,
        id: &ReleaseId,
        head: &str,
        release: &Release,
    ) -> Result<(), Error> {
        transaction(&self.db, |db| {
            let mut stmt = db.prepare(
                "INSERT INTO releases (id, repo, head, timestamp, release)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE
                 SET repo = ?2, head = ?3, timestamp = ?4, release = ?5",
            )?;
            stmt.bind((1, sql::Value::String(id.to_string())))?;
            stmt.bind((2, sql::Value::String(repo.to_string())))?;
            stmt.bind((3, sql::Value::String(head.to_string())))?;
            stmt.bind((4, sql::Value::Integer(release.timestamp() as i64)))?;
            stmt.bind((5, sql::Value::String(serde_json::to_string(release)?)))?;
            stmt.next()?;

            // Rebuild the locations index for this release.
            delete_locations(db, repo, id)?;
            for (cid, artifact) in release.artifacts() {
                let cid = cid.to_string();
                if artifact.locations().is_empty() {
                    // Sentinel row so `releases_for_cid` still matches
                    // location-less artifacts; `locations_for` filters it out.
                    insert_location(db, repo, &cid, id, "", "")?;
                } else {
                    for (did, urls) in artifact.locations() {
                        let did = did.to_string();
                        for url in urls {
                            insert_location(db, repo, &cid, id, &did, url.as_str())?;
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Remove a cached release and its `locations` rows.
    pub fn remove(&self, repo: &RepoId, id: &ReleaseId) -> Result<(), Error> {
        transaction(&self.db, |db| {
            let mut stmt = db.prepare("DELETE FROM releases WHERE id = ?1")?;
            stmt.bind((1, sql::Value::String(id.to_string())))?;
            stmt.next()?;
            delete_locations(db, repo, id)?;
            Ok(())
        })
    }

    /// The `(head, release)` for a cached release, if present.
    pub fn get(&self, repo: &RepoId, id: &ReleaseId) -> Result<Option<(String, Release)>, Error> {
        let mut stmt = self.db.prepare(
            "SELECT id, head, timestamp, release FROM releases WHERE repo = ?1 AND id = ?2",
        )?;
        stmt.bind((1, sql::Value::String(repo.to_string())))?;
        stmt.bind((2, sql::Value::String(id.to_string())))?;
        match stmt.into_iter().next().transpose()? {
            None => Ok(None),
            Some(row) => {
                let (_, head, release) = parse_release_row(&row)?;
                Ok(Some((head, release)))
            }
        }
    }

    /// Every cached release for a repository.
    pub fn list(&self, repo: &RepoId) -> Result<Vec<(ReleaseId, Release)>, Error> {
        let mut stmt = self
            .db
            .prepare("SELECT id, head, timestamp, release FROM releases WHERE repo = ?1")?;
        stmt.bind((1, sql::Value::String(repo.to_string())))?;
        let mut out = Vec::new();
        for row in stmt.into_iter() {
            let (id, _head, release) = parse_release_row(&row?)?;
            out.push((id, release));
        }
        Ok(out)
    }

    /// A map of cached release id to its freshness token, for a repository.
    pub fn heads(&self, repo: &RepoId) -> Result<HashMap<ReleaseId, String>, Error> {
        let mut stmt = self
            .db
            .prepare("SELECT id, head FROM releases WHERE repo = ?1")?;
        stmt.bind((1, sql::Value::String(repo.to_string())))?;
        let mut out = HashMap::new();
        for row in stmt.into_iter() {
            let row = row?;
            let id = parse_release_id(row.try_read::<&str, _>("id")?)?;
            out.insert(id, row.try_read::<&str, _>("head")?.to_string());
        }
        Ok(out)
    }

    /// Number of cached releases for a repository.
    pub fn count(&self, repo: &RepoId) -> Result<usize, Error> {
        let mut stmt = self
            .db
            .prepare("SELECT COUNT(*) AS count FROM releases WHERE repo = ?1")?;
        stmt.bind((1, sql::Value::String(repo.to_string())))?;
        match stmt.into_iter().next().transpose()? {
            Some(row) => Ok(row.try_read::<i64, _>("count")? as usize),
            None => Ok(0),
        }
    }

    /// Release ids that contain an artifact with `cid`.
    pub fn releases_for_cid(&self, repo: &RepoId, cid: &Cid) -> Result<Vec<ReleaseId>, Error> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT release FROM locations WHERE repo = ?1 AND cid = ?2")?;
        stmt.bind((1, sql::Value::String(repo.to_string())))?;
        stmt.bind((2, sql::Value::String(cid.to_string())))?;
        let mut out = Vec::new();
        for row in stmt.into_iter() {
            out.push(parse_release_id(row?.try_read::<&str, _>("release")?)?);
        }
        Ok(out)
    }

    /// All `(release, contributor, url)` locations for `cid` in a repository.
    pub fn locations_for(
        &self,
        repo: &RepoId,
        cid: &Cid,
    ) -> Result<Vec<(ReleaseId, Did, Url)>, Error> {
        let mut stmt = self.db.prepare(
            "SELECT release, did, url FROM locations
             WHERE repo = ?1 AND cid = ?2 AND url <> ''",
        )?;
        stmt.bind((1, sql::Value::String(repo.to_string())))?;
        stmt.bind((2, sql::Value::String(cid.to_string())))?;
        let mut out = Vec::new();
        for row in stmt.into_iter() {
            let row = row?;
            let id = parse_release_id(row.try_read::<&str, _>("release")?)?;
            let did = Did::from_str(row.try_read::<&str, _>("did")?)
                .map_err(|e| Error::Parse(format!("did: {e}")))?;
            let url = Url::parse(row.try_read::<&str, _>("url")?)
                .map_err(|e| Error::Parse(format!("url: {e}")))?;
            out.push((id, did, url));
        }
        Ok(out)
    }
}

/// Derive a stable freshness token from a COB's tip OIDs. Equality of the token
/// across two reads means the COB's git state is unchanged.
pub fn head_token(tips: impl IntoIterator<Item = Oid>) -> String {
    let mut tips: Vec<String> = tips.into_iter().map(|o| o.to_string()).collect();
    tips.sort();
    tips.join(",")
}

/// Run `f` inside an immediate write transaction, committing on `Ok` and rolling
/// back on `Err`. `BEGIN IMMEDIATE` takes the write lock upfront to avoid
/// upgrade deadlocks between processes.
fn transaction<T>(
    db: &sql::ConnectionThreadSafe,
    f: impl FnOnce(&sql::ConnectionThreadSafe) -> Result<T, Error>,
) -> Result<T, Error> {
    db.execute("BEGIN IMMEDIATE")?;
    match f(db) {
        Ok(value) => {
            db.execute("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            let _ = db.execute("ROLLBACK");
            Err(err)
        }
    }
}

/// Read `PRAGMA user_version`.
fn user_version(db: &sql::ConnectionThreadSafe) -> Result<usize, Error> {
    let version = db
        .prepare("PRAGMA user_version")?
        .into_iter()
        .next()
        .ok_or(Error::NoRows)??
        .read::<i64, _>(0);
    Ok(version as usize)
}

/// Delete a release's rows from the `locations` index.
fn delete_locations(
    db: &sql::ConnectionThreadSafe,
    repo: &RepoId,
    id: &ReleaseId,
) -> Result<(), Error> {
    let mut stmt = db.prepare("DELETE FROM locations WHERE repo = ?1 AND release = ?2")?;
    stmt.bind((1, sql::Value::String(repo.to_string())))?;
    stmt.bind((2, sql::Value::String(id.to_string())))?;
    stmt.next()?;
    Ok(())
}

/// Insert one row into the `locations` index.
fn insert_location(
    db: &sql::ConnectionThreadSafe,
    repo: &RepoId,
    cid: &str,
    release: &ReleaseId,
    did: &str,
    url: &str,
) -> Result<(), Error> {
    let mut stmt = db.prepare(
        "INSERT OR IGNORE INTO locations (repo, cid, release, did, url)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    stmt.bind((1, sql::Value::String(repo.to_string())))?;
    stmt.bind((2, sql::Value::String(cid.to_string())))?;
    stmt.bind((3, sql::Value::String(release.to_string())))?;
    stmt.bind((4, sql::Value::String(did.to_string())))?;
    stmt.bind((5, sql::Value::String(url.to_string())))?;
    stmt.next()?;
    Ok(())
}

/// Parse a [`ReleaseId`] from a cached hex OID string.
fn parse_release_id(s: &str) -> Result<ReleaseId, Error> {
    ReleaseId::from_str(s).map_err(|e| Error::Parse(format!("release id: {e}")))
}

/// Parse a `(id, head, release)` triple from a `releases` row, restoring the
/// serde-skipped `timestamp` from its column.
fn parse_release_row(row: &sql::Row) -> Result<(ReleaseId, String, Release), Error> {
    let id = parse_release_id(row.try_read::<&str, _>("id")?)?;
    let head = row.try_read::<&str, _>("head")?.to_string();
    let timestamp = row.try_read::<i64, _>("timestamp")? as u64;
    let mut release: Release = serde_json::from_str(row.try_read::<&str, _>("release")?)?;
    // `Release::timestamp` is `#[serde(skip)]`; restore it from its column.
    // Accessible here because `cache` is a submodule of the crate root where
    // `Release` is defined.
    release.timestamp = timestamp;
    Ok((id, head, release))
}
