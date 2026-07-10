//! Radicle "artifact COB".
//!
//! A [`Release`] records content-addressed artifacts associated with a Git
//! commit or annotated tag, along with discovery locations where each artifact
//! can be retrieved.
//!
//! Each artifact is identified by a [`Cid`] (content identifier) and has a
//! human-readable name. Each user can contribute multiple discovery [`Url`]s for
//! any artifact, enabling decentralized mirroring.
//!
//! # Example
//!
//! ```
//! # use radicle::crypto::Signer;
//! # use radicle::git::{raw::Repository, Oid};
//! # use radicle::identity::Did;
//! # use radicle::test;
//! # use url::Url;
//! #
//! # use radicle_artifact::{Cid, Releases};
//! #
//! # fn commit(repo: &Repository, message: &str) -> Oid {
//! #     let tree = {
//! #         let tree = repo.treebuilder(None).unwrap();
//! #         let oid = tree.write().unwrap();
//! #         repo.find_tree(oid).unwrap()
//! #     };
//! #
//! #     let author = repo.signature().unwrap();
//! #     repo.commit(None, &author, &author, message, &tree, &[])
//! #         .unwrap()
//! #         .into()
//! # }
//! #
//! # let test::setup::NodeWithRepo {
//! #     node: alice, repo, ..
//! # } = test::setup::NodeWithRepo::default();
//! # let oid = commit(&repo.backend, "Test Commit");
//! # let repo = (&*repo).clone();
//! let mut releases = Releases::open(repo).unwrap();
//!
//! // Create a fresh release COB for this commit.
//! let mut release = releases.create(oid, None, &alice.signer).unwrap();
//!
//! let cid: Cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi".parse().unwrap();
//! let url = Url::parse("https://example.com/artifacts/linux-amd64.tar.gz").unwrap();
//! release.register_artifact(cid, "linux-amd64 binary".into(), &alice.signer).unwrap();
//! release.add_location(cid, url.clone(), &alice.signer).unwrap();
//!
//! // Read back the discovery locations a specific user contributed.
//! let alice_did = Did::from(alice.signer.public_key());
//! let locations = release.artifact(&cid).unwrap().locations_of(&alice_did).unwrap();
//! assert!(locations.contains(&url));
//! ```

#![deny(missing_docs)]

pub mod lifecycle;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

use indexmap::IndexMap;
use radicle::cob::store::Cob;
use radicle::cob::{self, store, EntryId, Evaluate, ObjectId, Op, TypeName};
use radicle::crypto;
use radicle::crypto::signature::Signer;
use radicle::identity::Did;
use radicle::node::device::Device;
use radicle::node::NodeId;
use radicle::prelude::ReadRepository;
use radicle::storage::{RepositoryError, SignRepository, WriteRepository};
use radicle::{cob::store::CobAction, git::Oid};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use url::Url;

// Re-export the project content identifier type. A newtype over cid::Cid
// that serializes as its canonical multibase string; see the type's docs.
pub use radicle_artifact_core::cid::Cid;

// Resolve the cache database path from a node's COBs directory, without
// exposing the filename the `cache` module owns.
pub use cache::db_path as cache_db_path;

pub mod discovery;
pub mod display;
pub mod error;

pub(crate) mod cache;

/// Type name of an artifact release.
pub static TYPENAME: LazyLock<TypeName> =
    LazyLock::new(|| FromStr::from_str("dev.radicle.artifact").expect("type name is valid"));

/// Maximum byte length for a redaction reason string.
pub const MAX_REDACT_REASON_LEN: usize = 2048;

/// Maximum byte length for a metadata key.
pub const MAX_METADATA_KEY_LEN: usize = 256;

/// Maximum byte length for a serialized metadata value.
pub const MAX_METADATA_VALUE_LEN: usize = 8 * 1024;

/// Metadata key recording an artifact's size hint in bytes (set on register
/// from a local path unless suppressed).
///
/// camelCase like all keys we author (see `docs/adr/0001-json-casing.md`).
/// Renamed from the legacy `size-bytes`; artifacts registered before the
/// rename keep the old key and render their size as a raw integer.
pub const METADATA_KEY_SIZE_BYTES: &str = "sizeBytes";

/// The identifier for a given [`Release`] collaborative object.
///
/// When a [`Release`] is created, through [`Releases::create`], the identifier
/// is also returned as part of [`ReleaseMut::id`].
///
/// Identifiers can be used to retrieve a [`Release`] or [`ReleaseMut`] through
/// [`Releases::get`] and [`Releases::get_mut`], respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReleaseId(ObjectId);

impl ReleaseId {
    fn as_object_id(&self) -> &ObjectId {
        &self.0
    }

    /// The underlying Git object id (the Release COB's head).
    ///
    /// Lets callers outside this crate (notably the seeder's tag layer)
    /// fold a release into a binary key without depending on `ObjectId`.
    pub fn oid(&self) -> Oid {
        *self.0
    }
}

impl From<Oid> for ReleaseId {
    fn from(oid: Oid) -> Self {
        Self(ObjectId::from(oid))
    }
}

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ReleaseId {
    type Err = <ObjectId as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ObjectId::from_str(s).map(Self)
    }
}

impl From<ReleaseId> for ObjectId {
    fn from(ReleaseId(oid): ReleaseId) -> Self {
        oid
    }
}

impl From<ObjectId> for ReleaseId {
    fn from(oid: ObjectId) -> Self {
        Self(oid)
    }
}

/// A `Release` groups content-addressed artifacts under a single Git commit.
///
/// May record an annotated tag OID alongside the commit (the COB itself
/// is always commit-keyed; the tag is metadata). The creator's DID is
/// preserved so callers can apply visibility and trust policies (e.g.
/// preferring delegate-authored releases when duplicates exist).
/// Per-artifact attribution lives on [`Artifact::author`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    oid: Oid,
    /// Set once at creation from the initial `Action::Create`'s `tag`
    /// field — first-writer-wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<Oid>,
    /// Signer of the initial op. Persisted; not derived from the
    /// action stream.
    creator: Did,
    artifacts: IndexMap<Cid, Artifact>,
    /// Creation time, from the root op's timestamp; not in the action payload.
    timestamp: cob::Timestamp,
}

/// A single artifact identified by its [`Cid`].
///
/// Each artifact has a human-readable `name` describing what it is, and a set
/// of discovery locations contributed by various users identified by their DIDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// The DID that originally added this artifact.
    author: Did,
    name: String,
    locations: BTreeMap<Did, BTreeSet<Url>>,
    /// Users that have independently verified this artifact's CID.
    #[serde(default)]
    attestations: BTreeSet<Did>,
    /// Users that have redacted this artifact, with their stated reason.
    #[serde(default)]
    redactions: BTreeMap<Did, String>,
    /// Free-form key/value annotations contributed by the artifact's
    /// author or repository delegates. Keys are strings; values are
    /// arbitrary JSON. Shared keyspace, last-writer-wins.
    /// Authorization is enforced at the CLI layer; the COB itself accepts
    /// any signed action for replay determinism. Per-entry attribution is
    /// not stored — the COB entry log retains signatures for audit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, serde_json::Value>,
}

impl Artifact {
    /// Get the [`Did`] of the user that added this artifact.
    pub fn author(&self) -> &Did {
        &self.author
    }

    /// Get the human-readable name of this artifact.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the discovery locations, keyed by the DID that contributed them.
    pub fn locations(&self) -> &BTreeMap<Did, BTreeSet<Url>> {
        &self.locations
    }

    /// Get all discovery URLs across all users.
    ///
    /// Note: the same URL may appear more than once if multiple users
    /// contributed it.
    pub fn all_locations(&self) -> Vec<&Url> {
        self.locations.values().flatten().collect()
    }

    /// Get the discovery URLs contributed by a specific DID.
    pub fn locations_of(&self, user: &Did) -> Option<&BTreeSet<Url>> {
        self.locations.get(user)
    }

    /// Get the set of DIDs that have attested to this artifact.
    pub fn attestations(&self) -> &BTreeSet<Did> {
        &self.attestations
    }

    /// Check whether a specific DID has attested to this artifact.
    pub fn is_attested_by(&self, user: &Did) -> bool {
        self.attestations.contains(user)
    }

    /// Get all redactions, keyed by the DID that issued them.
    pub fn redactions(&self) -> &BTreeMap<Did, String> {
        &self.redactions
    }

    /// Check whether a specific DID has redacted this artifact.
    pub fn is_redacted_by(&self, user: &Did) -> bool {
        self.redactions.contains_key(user)
    }

    /// Get the redaction reason from a specific DID, if any.
    pub fn redaction_by(&self, user: &Did) -> Option<&str> {
        self.redactions.get(user).map(|s| s.as_str())
    }

    /// Check whether any DID has redacted this artifact.
    pub fn is_redacted(&self) -> bool {
        !self.redactions.is_empty()
    }

    /// Get all metadata entries.
    pub fn metadata(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.metadata
    }

    /// Get locations filtered by URL scheme, with the contributing DID.
    pub fn locations_by_scheme<'a>(&'a self, scheme: &str) -> Vec<(&'a Url, &'a Did)> {
        self.locations
            .iter()
            .flat_map(|(did, urls)| {
                urls.iter()
                    .filter(|u| u.scheme() == scheme)
                    .map(move |u| (u, did))
            })
            .collect()
    }
}

/// The collaborative object actions for artifact releases.
///
/// This is the persisted COB format: each action serializes to canonical
/// JSON that is committed to git, replicated to peers, and signed. Its
/// variant tags (PascalCase) and field names (snake_case) are therefore a
/// frozen wire format and are deliberately NOT camelCased like the CLI /
/// control-socket output. Do not add `rename_all` here or rename variants
/// without a compatibility path; see `docs/adr/0001-json-casing.md`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    /// Create a [`Release`] for the given commit [`Oid`].
    ///
    /// Must be the first action. Subsequent `Create` actions are ignored.
    Create {
        /// The commit OID this release is keyed by. Must be a commit
        /// object — the COB store rejects non-commit parents.
        oid: Oid,
        /// Optional annotated tag object OID. When `Some`, the tag's
        /// target is expected to peel to `oid`; the COB records the
        /// link as durable metadata. `None` for plain commit releases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<Oid>,
    },
    /// Register an artifact in the release.
    ///
    /// If the CID already exists, the name is updated. The serialized
    /// action name stays `AddArtifact` for backward compatibility with
    /// existing COBs.
    #[serde(rename = "AddArtifact")]
    RegisterArtifact {
        /// The content identifier for this artifact.
        cid: Cid,
        /// A human-readable description of the artifact.
        name: String,
    },
    /// Add a discovery location for an existing artifact.
    ///
    /// The authoring user is recorded as the contributor of this location.
    /// Ignored if the CID does not exist in the release.
    AddLocation {
        /// The content identifier of the artifact.
        cid: Cid,
        /// A URL where the artifact can be retrieved.
        location: Url,
    },
    /// Remove a discovery location previously added by this user.
    ///
    /// No-op if the location or CID is not found.
    RemoveLocation {
        /// The content identifier of the artifact.
        cid: Cid,
        /// The URL to remove.
        location: Url,
    },
    /// Attest that this user has independently verified the artifact.
    ///
    /// Idempotent — attesting the same CID twice from the same user is a no-op.
    /// Silent no-op if the user is the artifact's author (authorship implies
    /// endorsement) or if the CID does not exist in the release.
    Attest {
        /// The content identifier of the artifact to attest.
        cid: Cid,
    },
    /// Redact an artifact, indicating it should not be used.
    ///
    /// The reason is a free-form string explaining why (e.g. supply chain
    /// compromise, build reproducibility failure). If the same DID redacts
    /// again, the reason is updated — the act of redaction is permanent but
    /// the reason text can be amended. A redaction supersedes any prior
    /// attestation from the same DID, and prevents future attestations from
    /// taking effect.
    ///
    /// Silent no-op if the CID does not exist in the release (this is
    /// intentional for COB replay consistency; the [`ReleaseMut`] API
    /// validates the CID before creating the action).
    Redact {
        /// The content identifier of the artifact to redact.
        cid: Cid,
        /// A human-readable reason for the redaction.
        reason: String,
    },
    /// Set or overwrite a metadata entry on an artifact.
    ///
    /// The keyspace is shared across all contributors; last-writer wins.
    /// The signer is recorded as the entry's author so display filtering
    /// can hide entries from contributors who are no longer authorized.
    /// Authorization (artifact author or current repo delegate) is
    /// enforced at the CLI layer, not here, so replay stays deterministic.
    /// Silent no-op if the CID does not exist in the release.
    SetMetadata {
        /// The content identifier of the artifact.
        cid: Cid,
        /// Metadata key.
        key: String,
        /// JSON value (any shape).
        value: serde_json::Value,
    },
    /// Remove a metadata entry from an artifact.
    ///
    /// Any authorized contributor can remove any key. Silent no-op if the
    /// CID or the key is not found.
    RemoveMetadata {
        /// The content identifier of the artifact.
        cid: Cid,
        /// Metadata key to remove.
        key: String,
    },
}

impl CobAction for Action {
    fn parents(&self) -> Vec<radicle::git::Oid> {
        match self {
            // Only the commit OID is exposed: the COB store rejects any
            // non-commit parent. The optional tag OID is metadata.
            Action::Create { oid, .. } => vec![*oid],
            _ => Vec::new(),
        }
    }
}

impl Release {
    /// Construct a new [`Release`].
    fn new(oid: Oid, tag: Option<Oid>, creator: Did, timestamp: cob::Timestamp) -> Self {
        Self {
            oid,
            tag,
            creator,
            artifacts: IndexMap::new(),
            timestamp,
        }
    }

    /// Get the commit [`Oid`] this release is keyed by.
    pub fn oid(&self) -> &Oid {
        &self.oid
    }

    /// Annotated tag [`Oid`] linked to this release, if any. The tag's
    /// target peels to [`Self::oid`].
    pub fn tag(&self) -> Option<&Oid> {
        self.tag.as_ref()
    }

    /// [`Did`] of the user who created this release COB. Callers use
    /// this to apply trust policies (e.g. preferring delegate-authored
    /// releases when multiple exist for the same commit).
    pub fn creator(&self) -> &Did {
        &self.creator
    }

    /// Get the timestamp when this release was created.
    pub fn timestamp(&self) -> cob::Timestamp {
        self.timestamp
    }

    /// Get all artifacts in this release.
    pub fn artifacts(&self) -> &IndexMap<Cid, Artifact> {
        &self.artifacts
    }

    /// Get a specific artifact by its [`Cid`].
    pub fn artifact(&self, cid: &Cid) -> Option<&Artifact> {
        self.artifacts.get(cid)
    }

    /// Apply an action to the release state.
    fn action(&mut self, user: Did, action: Action) {
        match action {
            // Subsequent Create actions are ignored after initialization.
            Action::Create { .. } => {}
            Action::RegisterArtifact { cid, name } => {
                // Insert if new, or update the name if the original author resends.
                match self.artifacts.entry(cid) {
                    indexmap::map::Entry::Occupied(mut e) => {
                        if e.get().author == user {
                            e.get_mut().name = name;
                        }
                    }
                    indexmap::map::Entry::Vacant(e) => {
                        e.insert(Artifact {
                            author: user,
                            name,
                            locations: BTreeMap::new(),
                            attestations: BTreeSet::new(),
                            redactions: BTreeMap::new(),
                            metadata: BTreeMap::new(),
                        });
                    }
                }
            }
            Action::AddLocation { cid, location } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    artifact.locations.entry(user).or_default().insert(location);
                }
            }
            Action::RemoveLocation { cid, location } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    if let Entry::Occupied(mut e) = artifact.locations.entry(user) {
                        e.get_mut().remove(&location);
                        if e.get().is_empty() {
                            e.remove();
                        }
                    }
                }
            }
            Action::Attest { cid } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    // A prior redaction from this user supersedes any attestation.
                    // The author implicitly vouches by creating the artifact;
                    // a self-attestation is a no-op to avoid inflating counts.
                    if user != artifact.author && !artifact.redactions.contains_key(&user) {
                        artifact.attestations.insert(user);
                    }
                }
            }
            Action::Redact { cid, reason } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    artifact.redactions.insert(user, reason);
                    // A redaction supersedes any prior attestation from the same user.
                    artifact.attestations.remove(&user);
                }
            }
            Action::SetMetadata { cid, key, value } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    artifact.metadata.insert(key, value);
                }
            }
            Action::RemoveMetadata { cid, key } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    artifact.metadata.remove(&key);
                }
            }
        }
    }
}

impl store::CobWithType for Release {
    fn type_name() -> &'static TypeName {
        &TYPENAME
    }
}

impl store::Cob for Release {
    type Action = Action;
    type Error = error::Build;

    fn from_root<R: ReadRepository>(op: Op<Self::Action>, repo: &R) -> Result<Self, Self::Error> {
        let mut actions = op.actions.into_iter();
        let Some(Action::Create { oid, tag }) = actions.next() else {
            return Err(error::Build::Initial);
        };
        repo.commit(oid)
            .map_err(|err| error::Build::MissingCommit { oid, err })?;
        // Initial op's signer becomes the creator and the per-action author.
        let author = Did::from(op.author);
        let mut release = Self::new(oid, tag, author, op.timestamp);
        for action in actions {
            release.action(author, action);
        }
        Ok(release)
    }

    fn op<'a, R: ReadRepository, I: IntoIterator<Item = &'a radicle::cob::Entry>>(
        &mut self,
        op: Op<Self::Action>,
        _concurrent: I,
        _repo: &R,
    ) -> Result<(), Self::Error> {
        let author = Did::from(op.author);
        for action in op.actions {
            self.action(author, action);
        }
        Ok(())
    }
}

impl<R: ReadRepository> Evaluate<R> for Release {
    type Error = error::Apply;

    fn init(entry: &radicle::cob::Entry, store: &R) -> Result<Self, Self::Error> {
        let op = Op::try_from(entry)?;
        let object = Release::from_root(op, store)?;
        Ok(object)
    }

    fn apply<'a, I: Iterator<Item = (&'a Oid, &'a radicle::cob::Entry)>>(
        &mut self,
        entry: &radicle::cob::Entry,
        concurrent: I,
        store: &R,
    ) -> Result<(), Self::Error> {
        let op = Op::try_from(entry)?;
        self.op(op, concurrent.map(|(_, e)| e), store)
            .map_err(error::Apply::from)
    }
}

/// The storage for all [`Release`] items.
///
/// To get a handle for [`Releases`] use [`Releases::open`].
///
/// The read-only operations for [`Releases`] are:
///
///   - [`Releases::count`]
///   - [`Releases::get`]
///
/// The write operations for [`Releases`] are:
///
///   - [`Releases::create`]
///   - [`Releases::get_mut`]
pub struct Releases<'a, R> {
    repo: &'a R,
    identity: Oid,
    /// Optional SQLite cache. When present, reads are served from it after a
    /// cheap freshness check that re-materializes stale entries. It is
    /// populated lazily by reads, not by writes: a write advances the COB's git
    /// tips, so the next read sees the freshness token change and re-materializes.
    /// Best-effort — on any cache error, reads fall back to git.
    cache: Option<cache::Store>,
}

impl<'a, R> Releases<'a, R>
where
    R: ReadRepository + cob::Store<Namespace = NodeId>,
{
    /// Open a releases store.
    pub fn open(repository: &'a R) -> Result<Self, RepositoryError> {
        let identity = repository.identity_head()?;
        Ok(Self {
            repo: repository,
            identity,
            cache: None,
        })
    }

    /// Open a releases store backed by a SQLite cache at `path`.
    ///
    /// Reads are served from the cache after a cheap check that the COB's git
    /// tips are unchanged, re-materializing only stale objects. The cache is a
    /// best-effort optimization: if it cannot be opened or migrated, a warning
    /// is logged and the store falls back to reading directly from git.
    pub fn open_cached(
        repository: &'a R,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, RepositoryError> {
        let mut store = Self::open(repository)?;
        match cache::open_writer(path) {
            Ok(cache) => store.cache = Some(cache),
            Err(err) => log::warn!(target: "artifact", "artifact cache disabled: {err}"),
        }
        Ok(store)
    }

    /// Attach an already-open cache, sharing its connection. Used by the
    /// node-wide [`discovery`](crate::discovery) index to reuse one cache across
    /// every repository, and by tests.
    pub(crate) fn with_cache(mut self, cache: cache::Store) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Build a read-only view of the underlying COB store.
    fn read_store(
        &self,
    ) -> Result<store::Store<'a, Release, R, store::access::ReadOnly>, store::Error> {
        Ok(store::Store::open(self.repo, store::access::ReadOnly)?.identity(self.identity))
    }

    /// Build a write-capable view of the underlying COB store, bound to `signer`.
    fn write_store<'s, G>(
        &self,
        signer: &'s Device<G>,
    ) -> Result<store::Store<'a, Release, R, store::access::WriteAs<'s, Device<G>>>, store::Error>
    {
        Ok(
            store::Store::open(self.repo, store::access::WriteAs::new(signer))?
                .identity(self.identity),
        )
    }

    /// Return the number of [`Release`]s in the repository.
    ///
    /// Counts the distinct COB objects of the release type from a git ref walk;
    /// it materializes no release, so it is O(refs) and ignores the cache. Note
    /// this counts ref-backed objects: one that no longer materializes (e.g. a
    /// fully-redacted COB) is still counted here but excluded by [`Self::all`].
    pub fn count(&self) -> Result<usize, store::Error> {
        // Mirror how `cob::list` surfaces a `types()` failure, minus the fold.
        self.repo
            .types(&TYPENAME)
            .map(|objects| objects.len())
            .map_err(|err| {
                store::Error::Retrieve(cob::error::Retrieve::Refs { err: Box::new(err) })
            })
    }

    /// Iterate over every [`Release`] in the store.
    ///
    /// With a cache, releases are served from SQLite after a cheap freshness
    /// check that re-materializes only the objects whose git tips changed.
    pub fn all(
        &self,
    ) -> Result<
        impl ExactSizeIterator<Item = Result<(ObjectId, Release), store::Error>> + use<'a, R>,
        store::Error,
    > {
        let items: Vec<Result<(ObjectId, Release), store::Error>> = if self.cache.is_some() {
            match self.cached_all() {
                Ok(list) => list.into_iter().map(Ok).collect(),
                Err(CacheOpError::Store(err)) => return Err(err),
                Err(CacheOpError::Cache(msg)) => {
                    log::warn!(target: "artifact", "cache list failed ({msg}); using git");
                    self.read_store()?.all()?.collect()
                }
            }
        } else {
            self.read_store()?.all()?.collect()
        };
        Ok(items.into_iter())
    }

    /// Get a [`Release`], given its [`ReleaseId`] identifier.
    pub fn get(&self, id: &ReleaseId) -> Result<Option<Release>, store::Error> {
        if self.cache.is_some() {
            match self.cached_get(id) {
                Ok(release) => return Ok(release),
                Err(CacheOpError::Store(err)) => return Err(err),
                Err(CacheOpError::Cache(msg)) => {
                    log::warn!(target: "artifact", "cache get failed ({msg}); using git");
                }
            }
        }
        self.read_store()?.get(id.as_object_id())
    }

    /// Find the [`Release`]s that are associated with the `wanted` commit.
    pub fn find_by_commit(&self, wanted: Oid) -> Result<FindByCommit<'a>, store::Error> {
        FindByCommit::new(self, wanted)
    }

    /// Return every release containing an artifact with the given CID.
    ///
    /// The same CID may appear in multiple releases — either across different
    /// commits, or within duplicate release COBs for the same commit when two
    /// users concurrently created the release before syncing. Retrieval should
    /// union locations across all of them, so callers building a fetch plan
    /// should aggregate across the returned releases.
    pub fn find_by_cid(&self, cid: &Cid) -> Result<Vec<(ReleaseId, Release)>, cob::store::Error> {
        if self.cache.is_some() {
            match self.cached_find_by_cid(cid) {
                Ok(out) => return Ok(out),
                Err(CacheOpError::Store(err)) => return Err(err),
                Err(CacheOpError::Cache(msg)) => {
                    log::warn!(target: "artifact", "cache find_by_cid failed ({msg}); using git");
                }
            }
        }
        let mut out = Vec::new();
        for result in self.read_store()?.all()? {
            let (id, release) = result?;
            if release.artifact(cid).is_some() {
                out.push((ReleaseId::from(id), release));
            }
        }
        Ok(out)
    }

    /// Return every discovery location for `cid` across all releases in the
    /// repository, as `(release, contributor, url)` tuples.
    ///
    /// Backed by the cache's normalized locations index when available (a
    /// direct indexed lookup); otherwise scans every release from git. The
    /// same URL may appear under multiple releases or contributors, so callers
    /// building a fetch plan should aggregate.
    pub fn locations_for(
        &self,
        cid: &Cid,
    ) -> Result<Vec<(ReleaseId, Did, Url)>, cob::store::Error> {
        if self.cache.is_some() {
            match self.cached_locations_for(cid) {
                Ok(out) => return Ok(out),
                Err(CacheOpError::Store(err)) => return Err(err),
                Err(CacheOpError::Cache(msg)) => {
                    log::warn!(target: "artifact", "cache locations_for failed ({msg}); using git");
                }
            }
        }
        let mut out = Vec::new();
        for result in self.read_store()?.all()? {
            let (id, release) = result?;
            if let Some(artifact) = release.artifact(cid) {
                for (did, urls) in artifact.locations() {
                    for url in urls {
                        out.push((ReleaseId::from(id), *did, url.clone()));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// [`Iterator`] for finding each [`Release`] where the [`Release::oid`] matches
/// the wanted commit. See [`Releases::find_by_commit`].
pub struct FindByCommit<'a> {
    releases: Box<dyn Iterator<Item = Result<(ObjectId, Release), cob::store::Error>> + 'a>,
    needle: Oid,
}

impl<'a> FindByCommit<'a> {
    fn new<R>(releases: &Releases<'a, R>, needle: Oid) -> Result<Self, cob::store::Error>
    where
        R: ReadRepository + cob::Store<Namespace = NodeId>,
    {
        Ok(Self {
            releases: Box::new(releases.all()?),
            needle,
        })
    }

    fn wanted(&self, release: &Release) -> bool {
        self.needle == *release.oid()
    }
}

impl Iterator for FindByCommit<'_> {
    type Item = Result<(ReleaseId, Release), cob::store::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        // Use a loop instead of recursion to avoid stack overflow on large stores.
        loop {
            let result = self.releases.next()?;
            match result {
                Ok((id, release)) if self.wanted(&release) => {
                    return Some(Ok((ReleaseId::from(id), release)));
                }
                Ok(_) => continue,
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

/// Internal result of a cache-backed read.
///
/// Distinguishes a real git/store error (propagated to the caller) from a soft
/// cache/refs error (logged, then the caller falls back to the git path).
enum CacheOpError {
    /// A git/store error; propagate it.
    Store(store::Error),
    /// A cache or refs-read failure; fall back to git.
    Cache(String),
}

/// Private cache-backed read helpers for [`Releases`]. Each returns
/// [`CacheOpError`] so the public methods can fall back to git on soft errors.
impl<'a, R> Releases<'a, R>
where
    R: ReadRepository + cob::Store<Namespace = NodeId>,
{
    /// Ensure every cached release for this repo matches the COB's current git
    /// tips, re-materializing only the objects whose tips changed and pruning removed
    /// ones. Cheap in steady state: one ref walk plus token comparisons.
    fn refresh_repo(&self, cache: &cache::Store) -> Result<(), CacheOpError> {
        let repo_id = self.repo.id();
        let current = self
            .repo
            .types(&TYPENAME)
            .map_err(|e| CacheOpError::Cache(e.to_string()))?;
        let cached = cache
            .heads(&repo_id)
            .map_err(|e| CacheOpError::Cache(e.to_string()))?;
        let store = self.read_store().map_err(CacheOpError::Store)?;

        for (oid, objects) in &current {
            let id = ReleaseId::from(*oid);
            let token = cache::head_token(objects.iter().map(|r| r.target.id));
            if cached.get(&id).is_some_and(|head| head == &token) {
                continue;
            }
            // Stale or missing: re-materialize this one object. A broken object is
            // skipped (matching `cob::list`), not treated as fatal.
            match store.get(oid) {
                Ok(Some(release)) => cache
                    .update(&repo_id, &id, &token, &release)
                    .map_err(|e| CacheOpError::Cache(e.to_string()))?,
                Ok(None) => cache
                    .remove(&repo_id, &id)
                    .map_err(|e| CacheOpError::Cache(e.to_string()))?,
                Err(err) => {
                    log::warn!(target: "artifact", "skipping unreadable release {id}: {err}");
                }
            }
        }
        // Prune cached releases whose objects no longer exist in git.
        for id in cached.keys() {
            if !current.contains_key(id.as_object_id()) {
                cache
                    .remove(&repo_id, id)
                    .map_err(|e| CacheOpError::Cache(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn cached_all(&self) -> Result<Vec<(ObjectId, Release)>, CacheOpError> {
        let cache = self.cache.as_ref().expect("cache is present");
        self.refresh_repo(cache)?;
        let list = cache
            .list(&self.repo.id())
            .map_err(|e| CacheOpError::Cache(e.to_string()))?;
        Ok(list
            .into_iter()
            .map(|(id, release)| (*id.as_object_id(), release))
            .collect())
    }

    fn cached_get(&self, id: &ReleaseId) -> Result<Option<Release>, CacheOpError> {
        let cache = self.cache.as_ref().expect("cache is present");
        let repo_id = self.repo.id();
        let objects = self
            .repo
            .objects(&TYPENAME, id.as_object_id())
            .map_err(|e| CacheOpError::Cache(e.to_string()))?;
        let tips: Vec<Oid> = objects.iter().map(|r| r.target.id).collect();
        if tips.is_empty() {
            // Object no longer exists in git.
            cache
                .remove(&repo_id, id)
                .map_err(|e| CacheOpError::Cache(e.to_string()))?;
            return Ok(None);
        }
        let token = cache::head_token(tips);
        if let Some((head, release)) = cache
            .get(&repo_id, id)
            .map_err(|e| CacheOpError::Cache(e.to_string()))?
        {
            if head == token {
                return Ok(Some(release));
            }
        }
        // Stale or missing: re-materialize.
        let store = self.read_store().map_err(CacheOpError::Store)?;
        match store.get(id.as_object_id()).map_err(CacheOpError::Store)? {
            Some(release) => {
                cache
                    .update(&repo_id, id, &token, &release)
                    .map_err(|e| CacheOpError::Cache(e.to_string()))?;
                Ok(Some(release))
            }
            None => {
                cache
                    .remove(&repo_id, id)
                    .map_err(|e| CacheOpError::Cache(e.to_string()))?;
                Ok(None)
            }
        }
    }

    fn cached_find_by_cid(&self, cid: &Cid) -> Result<Vec<(ReleaseId, Release)>, CacheOpError> {
        let cache = self.cache.as_ref().expect("cache is present");
        self.refresh_repo(cache)?;
        let repo_id = self.repo.id();
        let ids = cache
            .releases_for_cid(&repo_id, cid)
            .map_err(|e| CacheOpError::Cache(e.to_string()))?;
        let mut out = Vec::new();
        for id in ids {
            if let Some((_, release)) = cache
                .get(&repo_id, &id)
                .map_err(|e| CacheOpError::Cache(e.to_string()))?
            {
                if release.artifact(cid).is_some() {
                    out.push((id, release));
                }
            }
        }
        Ok(out)
    }

    fn cached_locations_for(&self, cid: &Cid) -> Result<Vec<(ReleaseId, Did, Url)>, CacheOpError> {
        let cache = self.cache.as_ref().expect("cache is present");
        self.refresh_repo(cache)?;
        cache
            .locations_for(&self.repo.id(), cid)
            .map_err(|e| CacheOpError::Cache(e.to_string()))
    }
}

impl<'a, R> Releases<'a, R>
where
    R: ReadRepository + SignRepository + cob::Store<Namespace = NodeId>,
{
    /// Get a [`ReleaseMut`], given its [`ReleaseId`] identifier.
    pub fn get_mut<'g>(
        &'g mut self,
        id: &ReleaseId,
    ) -> Result<ReleaseMut<'a, 'g, R>, store::Error> {
        let release = self
            .get(id)?
            .ok_or_else(move || store::Error::NotFound(TYPENAME.clone(), (*id).into()))?;

        Ok(ReleaseMut {
            id: *id,
            release,
            store: self,
        })
    }

    /// Create a new [`Release`] in the repository.
    ///
    /// `tag` is an optional annotated tag OID to record alongside the
    /// commit; see [`Release::tag`]. When `Some`, the OID must identify
    /// an annotated tag object whose target peels to `oid`; otherwise
    /// [`error::Create::MissingTag`] or [`error::Create::TagMismatch`]
    /// is returned.
    ///
    /// The signer's DID confers no release-level privilege — it's the git
    /// committer of the wrapping COB op, not the "owner" of the release.
    ///
    /// Callers that want to add to an existing release for the same
    /// commit should look it up via [`Releases::find_by_commit`] first
    /// and fall back to `create` only when no release exists.
    pub fn create<'g, G>(
        &'g mut self,
        oid: Oid,
        tag: Option<Oid>,
        signer: &Device<G>,
    ) -> Result<ReleaseMut<'a, 'g, R>, error::Create>
    where
        G: Signer<crypto::Signature>,
        R: WriteRepository,
    {
        if let Some(tag_oid) = tag {
            // Reject anything but an annotated tag whose target peels
            // to oid.
            use radicle::git::raw::ObjectType;
            let raw = self.repo.raw();
            let object = raw
                .find_object(tag_oid.into(), Some(ObjectType::Tag))
                .map_err(|err| error::Create::MissingTag { tag: tag_oid, err })?;
            let peeled = object
                .peel(ObjectType::Commit)
                .map_err(|err| error::Create::PeelFailed { tag: tag_oid, err })?;
            let actual: Oid = peeled.id().into();
            if actual != oid {
                return Err(error::Create::TagMismatch {
                    tag: tag_oid,
                    expected: oid,
                    actual,
                });
            }
        }

        let mut store = self.write_store(signer)?;
        let (id, release) = store::Transaction::initial::<_, Transaction<R>, _>(
            "Create release",
            &mut store,
            |tx, _| {
                tx.create(oid, tag)?;
                Ok(())
            },
        )?;
        let id = ReleaseId::from(id);

        Ok(ReleaseMut {
            id,
            release,
            store: self,
        })
    }
}

/// A `ReleaseMut` is a [`Release`] where the underlying `Release` can be
/// mutated by applying actions to it.
pub struct ReleaseMut<'a, 'g, R> {
    /// The COB identifier for this release.
    pub id: ReleaseId,

    release: Release,
    store: &'g mut Releases<'a, R>,
}

impl<R> Deref for ReleaseMut<'_, '_, R> {
    type Target = Release;

    fn deref(&self) -> &Self::Target {
        &self.release
    }
}

impl<'a, 'g, R> ReleaseMut<'a, 'g, R>
where
    R: WriteRepository + cob::Store<Namespace = NodeId>,
{
    /// The COB identifier for the underlying [`Release`].
    pub fn id(&self) -> &ReleaseId {
        &self.id
    }

    /// Reload the [`Release`] data from underlying storage.
    pub fn reload(&mut self) -> Result<(), store::Error> {
        self.release = self
            .store
            .get(&self.id)?
            .ok_or_else(|| store::Error::NotFound(TYPENAME.clone(), *self.id.as_object_id()))?;

        Ok(())
    }

    /// Register an artifact in the release.
    pub fn register_artifact<G>(
        &mut self,
        cid: Cid,
        name: String,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Register artifact", signer, |tx| {
            tx.register_artifact(cid, name)
        })
    }

    /// Register an artifact and record its size hint in the same transaction.
    ///
    /// The size is stored under [`METADATA_KEY_SIZE_BYTES`] as a JSON integer,
    /// which always fits within [`MAX_METADATA_VALUE_LEN`], so no value
    /// validation is needed. Both actions land in one signed COB entry.
    pub fn register_artifact_with_size<G>(
        &mut self,
        cid: Cid,
        name: String,
        size_bytes: u64,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Register artifact", signer, |tx| {
            tx.register_artifact(cid, name)?;
            tx.set_metadata(
                cid,
                METADATA_KEY_SIZE_BYTES.to_string(),
                serde_json::json!(size_bytes),
            )
        })
    }

    /// Add a discovery location for an artifact.
    pub fn add_location<G>(
        &mut self,
        cid: Cid,
        location: Url,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Add location", signer, |tx| tx.add_location(cid, location))
    }

    /// Remove a discovery location for an artifact.
    pub fn remove_location<G>(
        &mut self,
        cid: Cid,
        location: Url,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Remove location", signer, |tx| {
            tx.remove_location(cid, location)
        })
    }

    /// Attest that this user has independently verified an artifact.
    pub fn attest<G>(&mut self, cid: Cid, signer: &Device<G>) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Attest artifact", signer, |tx| tx.attest(cid))
    }

    /// Set or overwrite a metadata entry on an artifact.
    ///
    /// The key is validated here (non-empty, within [`MAX_METADATA_KEY_LEN`]
    /// bytes, no control characters) and the serialized value must fit
    /// within [`MAX_METADATA_VALUE_LEN`] so malformed or oversized entries
    /// never enter the COB log. Authorization (artifact author or current
    /// repo delegate) must still be enforced by the caller; the COB layer
    /// is permissive so replay stays deterministic across nodes that may
    /// disagree on the delegate set.
    pub fn set_metadata<G>(
        &mut self,
        cid: Cid,
        key: String,
        value: serde_json::Value,
        signer: &Device<G>,
    ) -> Result<EntryId, error::Metadata>
    where
        G: Signer<crypto::Signature>,
    {
        validate_metadata_key(&key)?;
        validate_metadata_value_size(&value)?;
        self.transaction("Set metadata", signer, |tx| {
            tx.set_metadata(cid, key, value)
        })
        .map_err(error::Metadata::from)
    }

    /// Remove a metadata entry from an artifact.
    ///
    /// Authorization is the caller's responsibility (see [`Self::set_metadata`]).
    pub fn remove_metadata<G>(
        &mut self,
        cid: Cid,
        key: String,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Remove metadata", signer, |tx| tx.remove_metadata(cid, key))
    }

    /// Redact an artifact, indicating it should not be used.
    ///
    /// Returns an error if the CID does not exist in the release or if the
    /// reason exceeds [`MAX_REDACT_REASON_LEN`] bytes.
    pub fn redact<G>(
        &mut self,
        cid: Cid,
        reason: String,
        signer: &Device<G>,
    ) -> Result<EntryId, error::Redact>
    where
        G: Signer<crypto::Signature>,
    {
        if self.artifact(&cid).is_none() {
            return Err(error::Redact::NotFound { cid });
        }
        if reason.len() > MAX_REDACT_REASON_LEN {
            return Err(error::Redact::ReasonTooLong {
                actual: reason.len(),
                max: MAX_REDACT_REASON_LEN,
            });
        }
        self.transaction("Redact artifact", signer, |tx| tx.redact(cid, reason))
            .map_err(error::Redact::from)
    }

    /// Apply COB operations to a `ReleaseMut`.
    fn transaction<G, F>(
        &mut self,
        message: &str,
        signer: &Device<G>,
        operations: F,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
        F: FnOnce(&mut Transaction<R>) -> Result<(), store::Error>,
    {
        let mut tx = Transaction::default();
        operations(&mut tx)?;

        let mut store = self.store.write_store(signer)?;
        let (release, commit) = tx.0.commit(message, self.id.into(), &mut store)?;
        self.release = release;

        Ok(commit)
    }
}

/// Reject metadata values whose serialized form exceeds
/// [`MAX_METADATA_VALUE_LEN`].
fn validate_metadata_value_size(value: &serde_json::Value) -> Result<(), error::Metadata> {
    let actual = serde_json::to_vec(value)
        .expect("serde_json::Value always serializes")
        .len();
    if actual > MAX_METADATA_VALUE_LEN {
        return Err(error::Metadata::ValueTooLarge {
            actual,
            max: MAX_METADATA_VALUE_LEN,
        });
    }
    Ok(())
}

/// Enforce the metadata key rules applied by [`ReleaseMut::set_metadata`].
fn validate_metadata_key(key: &str) -> Result<(), error::Metadata> {
    if key.is_empty() {
        return Err(error::Metadata::EmptyKey);
    }
    if key.len() > MAX_METADATA_KEY_LEN {
        return Err(error::Metadata::KeyTooLong {
            actual: key.len(),
            max: MAX_METADATA_KEY_LEN,
        });
    }
    if let Some(ch) = key.chars().find(|c| c.is_control()) {
        return Err(error::Metadata::KeyControlChar { ch });
    }
    Ok(())
}

/// An update for the `Release` COB.
struct Transaction<R: ReadRepository>(store::Transaction<Release, R>);

impl<R> From<store::Transaction<Release, R>> for Transaction<R>
where
    R: ReadRepository,
{
    fn from(tx: store::Transaction<Release, R>) -> Self {
        Self(tx)
    }
}

impl<R> From<Transaction<R>> for store::Transaction<Release, R>
where
    R: ReadRepository,
{
    fn from(Transaction(tx): Transaction<R>) -> Self {
        tx
    }
}

impl<R> Default for Transaction<R>
where
    R: ReadRepository,
{
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<R> Deref for Transaction<R>
where
    R: ReadRepository,
{
    type Target = store::Transaction<Release, R>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<R> DerefMut for Transaction<R>
where
    R: ReadRepository,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<R> Transaction<R>
where
    R: ReadRepository,
{
    /// Add a create operation to the transaction.
    fn create(&mut self, oid: Oid, tag: Option<Oid>) -> Result<(), store::Error> {
        self.0.push(Action::Create { oid, tag })
    }

    /// Register an artifact in the transaction.
    fn register_artifact(&mut self, cid: Cid, name: String) -> Result<(), store::Error> {
        self.0.push(Action::RegisterArtifact { cid, name })
    }

    /// Add a location for an artifact.
    fn add_location(&mut self, cid: Cid, location: Url) -> Result<(), store::Error> {
        self.0.push(Action::AddLocation { cid, location })
    }

    /// Remove a location for an artifact.
    fn remove_location(&mut self, cid: Cid, location: Url) -> Result<(), store::Error> {
        self.0.push(Action::RemoveLocation { cid, location })
    }

    /// Attest to an artifact.
    fn attest(&mut self, cid: Cid) -> Result<(), store::Error> {
        self.0.push(Action::Attest { cid })
    }

    /// Redact an artifact with a reason.
    fn redact(&mut self, cid: Cid, reason: String) -> Result<(), store::Error> {
        self.0.push(Action::Redact { cid, reason })
    }

    /// Set or overwrite a metadata entry.
    fn set_metadata(
        &mut self,
        cid: Cid,
        key: String,
        value: serde_json::Value,
    ) -> Result<(), store::Error> {
        self.0.push(Action::SetMetadata { cid, key, value })
    }

    /// Remove a metadata entry.
    fn remove_metadata(&mut self, cid: Cid, key: String) -> Result<(), store::Error> {
        self.0.push(Action::RemoveMetadata { cid, key })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod test {
    use std::collections::BTreeSet;

    use radicle::git::{raw::Repository, Oid};
    use radicle::identity::Did;
    use radicle::prelude::ReadStorage;
    use radicle::test;
    use url::Url;

    use crate::{Cid, Releases, METADATA_KEY_SIZE_BYTES};

    /// Create a valid CIDv1 (raw codec, sha2-256) from a distinguishing byte.
    fn test_cid(n: u8) -> Cid {
        use cid::multihash::Multihash;
        let mut digest = [0u8; 32];
        digest[0] = n;
        // 0x12 = sha2-256 hash code, 0x55 = raw codec
        let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
        Cid::from(cid::Cid::new_v1(0x55, mh))
    }

    #[test]
    fn action_cid_serializes_as_string() {
        use crate::Action;

        let cid = test_cid(1);
        let action = Action::RegisterArtifact {
            cid,
            name: "binary".into(),
        };

        // The CID must be a multibase string, not a JSON byte array.
        let value: serde_json::Value = serde_json::to_value(&action).unwrap();
        let serialized = value["AddArtifact"]["cid"].as_str().unwrap();
        assert_eq!(serialized, cid.to_string());

        // And it round-trips back to the same action.
        let json = serde_json::to_string(&action).unwrap();
        let decoded: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, action);
    }

    fn commit(repo: &Repository, message: &str) -> Oid {
        let tree = {
            let tree = repo.treebuilder(None).unwrap();
            let oid = tree.write().unwrap();
            repo.find_tree(oid).unwrap()
        };

        let author = repo.signature().unwrap();
        repo.commit(None, &author, &author, message, &tree, &[])
            .unwrap()
            .into()
    }

    /// Create an annotated tag object pointing at `target` and return its OID.
    fn annotate_tag(repo: &Repository, name: &str, target: Oid, message: &str) -> Oid {
        let object = repo.find_object(target.into(), None).unwrap();
        let tagger = repo.signature().unwrap();
        repo.tag(name, &object, &tagger, message, false)
            .unwrap()
            .into()
    }

    #[test]
    fn e2e() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();

        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        // Alice registers an artifact.
        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Alice adds a location for the artifact.
        let alice_url =
            Url::parse("https://alice.example.com/artifacts/linux-amd64.tar.gz").unwrap();
        release
            .add_location(cid, alice_url.clone(), &alice.signer)
            .unwrap();

        // Bob adds a mirror location for the same artifact.
        let bob_url = Url::parse("https://bob.example.com/mirror/linux-amd64.tar.gz").unwrap();
        release
            .add_location(cid, bob_url.clone(), &bob.signer)
            .unwrap();

        // Verify the artifact exists with both locations.
        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.name(), "linux-amd64 binary");
        assert!(artifact
            .locations_of(&Did::from(alice.signer.public_key()))
            .is_some_and(|urls| urls.contains(&alice_url)));
        assert!(artifact
            .locations_of(&Did::from(bob.signer.public_key()))
            .is_some_and(|urls| urls.contains(&bob_url)));

        // Alice removes her location.
        release
            .remove_location(cid, alice_url, &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact
            .locations_of(&Did::from(alice.signer.public_key()))
            .is_none());
        assert!(artifact
            .locations_of(&Did::from(bob.signer.public_key()))
            .is_some());
    }

    #[test]
    fn missing_commit() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let mut releases = Releases::open(&*repo).unwrap();
        let oid = test::arbitrary::oid();
        let release = releases.create(oid, None, &alice.signer);
        assert!(release.is_err());
    }

    #[test]
    fn idempotent_create() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let r1 = {
            let r1 = releases.create(oid, None, &alice.signer).unwrap();
            r1.id
        };
        let r2 = {
            let r2 = releases.create(oid, None, &alice.signer).unwrap();
            r2.id
        };

        // COB store deduplicates: same OID + same signer = same release.
        assert_eq!(r1, r2);
        assert_eq!(releases.get(&r1).unwrap(), releases.get(&r2).unwrap());
    }

    #[test]
    fn idempotent_register_artifact() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "first name".into(), &alice.signer)
            .unwrap();
        // Second add with different name updates it.
        release
            .register_artifact(cid, "second name".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.name(), "second name");
        assert_eq!(release.artifacts().len(), 1);
    }

    #[test]
    fn register_artifact_records_author() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.author(), &Did::from(alice.signer.public_key()));
    }

    #[test]
    fn register_artifact_with_size_records_hint() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact_with_size(cid, "binary".into(), 4096, &alice.signer)
            .unwrap();

        // Both the artifact entry and the size hint land from one call.
        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.name(), "binary");
        assert_eq!(
            artifact.metadata().get(METADATA_KEY_SIZE_BYTES),
            Some(&serde_json::json!(4096))
        );
    }

    #[test]
    fn non_author_cannot_rename_artifact() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "original name".into(), &alice.signer)
            .unwrap();

        // Bob tries to rename — should be ignored.
        release
            .register_artifact(cid, "bobs name".into(), &bob.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.name(), "original name");
        assert_eq!(artifact.author(), &Did::from(alice.signer.public_key()));
    }

    #[test]
    fn add_location_for_missing_cid_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(99);
        let url = Url::parse("https://example.com/file.tar.gz").unwrap();
        // Should succeed but have no effect since the CID doesn't exist.
        release.add_location(cid, url, &alice.signer).unwrap();

        assert!(release.artifact(&cid).is_none());
    }

    #[test]
    fn find_by_commit_returns_matching_releases() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid1 = commit(&repo.backend, "Commit A");
        let oid2 = commit(&repo.backend, "Commit B");
        let mut releases = Releases::open(&*repo).unwrap();

        let id1 = releases.create(oid1, None, &alice.signer).unwrap().id;
        let _id2 = releases.create(oid2, None, &alice.signer).unwrap().id;

        // find_by_commit should return only the release matching oid1.
        let results: Vec<_> = releases
            .find_by_commit(oid1)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id1);
        assert_eq!(results[0].1.oid(), &oid1);
    }

    #[test]
    fn find_by_commit_returns_empty_for_no_match() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Commit A");
        let other_oid = commit(&repo.backend, "Commit B");
        let mut releases = Releases::open(&*repo).unwrap();

        releases.create(oid, None, &alice.signer).unwrap();

        let results: Vec<_> = releases
            .find_by_commit(other_oid)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn multiple_locations_per_node() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Alice adds two different URLs for the same artifact.
        let url1 = Url::parse("https://alice.example.com/primary/linux-amd64.tar.gz").unwrap();
        let url2 = Url::parse("https://alice.example.com/mirror/linux-amd64.tar.gz").unwrap();
        release
            .add_location(cid, url1.clone(), &alice.signer)
            .unwrap();
        release
            .add_location(cid, url2.clone(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let urls = artifact
            .locations_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&url1));
        assert!(urls.contains(&url2));

        // Adding the same URL again is a no-op.
        release
            .add_location(cid, url1.clone(), &alice.signer)
            .unwrap();
        let artifact = release.artifact(&cid).unwrap();
        let urls = artifact
            .locations_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert_eq!(urls.len(), 2);

        // Removing one URL leaves the other intact.
        release.remove_location(cid, url1, &alice.signer).unwrap();
        let artifact = release.artifact(&cid).unwrap();
        let urls = artifact
            .locations_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls.contains(&url2));

        // Removing the last URL cleans up the DID entry entirely.
        release.remove_location(cid, url2, &alice.signer).unwrap();
        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact
            .locations_of(&Did::from(alice.signer.public_key()))
            .is_none());
    }

    #[test]
    fn remove_location_for_node_that_never_added_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        let url = Url::parse("https://example.com/file.tar.gz").unwrap();
        // Bob never added a location, so removing should be a no-op.
        release.remove_location(cid, url, &bob.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact.locations().is_empty());
    }

    #[test]
    fn reload_refreshes_from_store() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // Reload from store and verify the artifact is still present.
        release.reload().unwrap();
        assert!(release.artifact(&cid).is_some());
        assert_eq!(release.artifact(&cid).unwrap().name(), "test artifact");
    }

    #[test]
    fn multi_delegate_attestation() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: carol, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Bob and Carol attest; Alice's self-attestation is a no-op (she's the author).
        release.attest(cid, &alice.signer).unwrap();
        release.attest(cid, &bob.signer).unwrap();
        release.attest(cid, &carol.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.attestations().len(), 2);
        assert!(!artifact.is_attested_by(&Did::from(alice.signer.public_key())));
        assert!(artifact.is_attested_by(&Did::from(bob.signer.public_key())));
        assert!(artifact.is_attested_by(&Did::from(carol.signer.public_key())));
    }

    #[test]
    fn author_self_attestation_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // The author already vouches by creating the artifact.
        release.attest(cid, &alice.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert!(!artifact.is_attested_by(&Did::from(alice.signer.public_key())));
        assert_eq!(artifact.attestations().len(), 0);
    }

    #[test]
    fn idempotent_attestation() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // Attesting twice from the same non-author node should be a no-op.
        release.attest(cid, &bob.signer).unwrap();
        release.attest(cid, &bob.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.attestations().len(), 1);
    }

    #[test]
    fn attest_missing_cid_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        // Attest a CID that doesn't exist in the release.
        let cid = test_cid(99);
        release.attest(cid, &alice.signer).unwrap();

        assert!(release.artifact(&cid).is_none());
    }

    #[test]
    fn attestation_persists_through_reload() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();
        release.attest(cid, &bob.signer).unwrap();

        // Reload and verify attestation is still present.
        release.reload().unwrap();
        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact.is_attested_by(&Did::from(bob.signer.public_key())));
        assert_eq!(artifact.attestations().len(), 1);
    }

    #[test]
    fn redact_artifact() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();
        release
            .redact(cid, "compromised build".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_did = Did::from(alice.signer.public_key());
        assert!(artifact.is_redacted());
        assert!(artifact.is_redacted_by(&alice_did));
        assert_eq!(artifact.redaction_by(&alice_did), Some("compromised build"));
        assert_eq!(artifact.redactions().len(), 1);
    }

    #[test]
    fn multi_user_redaction() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        release
            .redact(cid, "supply chain attack".into(), &alice.signer)
            .unwrap();
        release
            .redact(cid, "failed reproducibility check".into(), &bob.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.redactions().len(), 2);
        assert_eq!(
            artifact.redaction_by(&Did::from(alice.signer.public_key())),
            Some("supply chain attack")
        );
        assert_eq!(
            artifact.redaction_by(&Did::from(bob.signer.public_key())),
            Some("failed reproducibility check")
        );
    }

    #[test]
    fn multi_user_same_reason() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Both users redact with the same reason — both are recorded independently.
        release
            .redact(cid, "malware detected".into(), &alice.signer)
            .unwrap();
        release
            .redact(cid, "malware detected".into(), &bob.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.redactions().len(), 2);
        assert!(artifact.is_redacted_by(&Did::from(alice.signer.public_key())));
        assert!(artifact.is_redacted_by(&Did::from(bob.signer.public_key())));
    }

    #[test]
    fn redact_updates_reason() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();
        release
            .redact(cid, "initial reason".into(), &alice.signer)
            .unwrap();
        release
            .redact(cid, "updated reason".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_did = Did::from(alice.signer.public_key());
        assert_eq!(artifact.redactions().len(), 1);
        assert_eq!(artifact.redaction_by(&alice_did), Some("updated reason"));
    }

    #[test]
    fn redaction_persists_through_reload() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();
        release
            .redact(cid, "compromised".into(), &alice.signer)
            .unwrap();

        // Reload and verify redaction is still present.
        release.reload().unwrap();
        let artifact = release.artifact(&cid).unwrap();
        let alice_did = Did::from(alice.signer.public_key());
        assert!(artifact.is_redacted_by(&alice_did));
        assert_eq!(artifact.redaction_by(&alice_did), Some("compromised"));
    }

    #[test]
    fn redact_removes_attestation() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Attest then redact — redaction should supersede the attestation.
        release.attest(cid, &bob.signer).unwrap();
        release
            .redact(cid, "source was compromised".into(), &bob.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let bob_did = Did::from(bob.signer.public_key());
        assert!(!artifact.is_attested_by(&bob_did));
        assert!(artifact.is_redacted_by(&bob_did));
    }

    #[test]
    fn redact_then_attest_is_blocked() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Redact first, then attempt to attest — the attestation should be
        // silently ignored because redactions are permanent and supersede.
        release
            .redact(cid, "suspected issue".into(), &bob.signer)
            .unwrap();
        release.attest(cid, &bob.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let bob_did = Did::from(bob.signer.public_key());
        assert!(!artifact.is_attested_by(&bob_did));
        assert!(artifact.is_redacted_by(&bob_did));
    }

    #[test]
    fn redact_only_removes_own_attestation() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Bob attests; Alice's self-attestation is a no-op (she's the author).
        // Then Alice redacts — Bob's attestation should remain.
        release.attest(cid, &bob.signer).unwrap();
        release
            .redact(cid, "compromised".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact.is_redacted_by(&Did::from(alice.signer.public_key())));
        assert!(artifact.is_attested_by(&Did::from(bob.signer.public_key())));
    }

    #[test]
    fn redact_empty_reason() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();
        release.redact(cid, "".into(), &alice.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_did = Did::from(alice.signer.public_key());
        assert!(artifact.is_redacted_by(&alice_did));
        assert_eq!(artifact.redaction_by(&alice_did), Some(""));
    }

    #[test]
    fn redact_reason_too_long() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        let long_reason = "x".repeat(crate::MAX_REDACT_REASON_LEN + 1);
        let result = release.redact(cid, long_reason, &alice.signer);
        assert!(result.is_err());
        // Artifact should not be redacted.
        assert!(!release.artifact(&cid).unwrap().is_redacted());
    }

    #[test]
    fn redact_nonexistent_cid_errors() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        // Try to redact a CID that was never added — should error.
        let cid = test_cid(99);
        let result = release.redact(cid, "does not matter".into(), &alice.signer);
        assert!(result.is_err());
    }

    #[test]
    fn get_mut_not_found() {
        let test::setup::NodeWithRepo {
            node: _alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let mut releases = Releases::open(&*repo).unwrap();

        let oid: radicle::cob::ObjectId = test::arbitrary::oid().into();
        let fake_id = crate::ReleaseId::from(oid);
        let result = releases.get_mut(&fake_id);
        assert!(result.is_err());
    }

    #[test]
    fn locations_by_scheme_filters_correctly() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        let https_url = Url::parse("https://example.com/file.tar.gz").unwrap();
        let iroh_url = Url::parse("radiroh://abc123").unwrap();
        let http_url = Url::parse("http://mirror.example.com/file.tar.gz").unwrap();
        release
            .add_location(cid, https_url.clone(), &alice.signer)
            .unwrap();
        release
            .add_location(cid, iroh_url.clone(), &alice.signer)
            .unwrap();
        release
            .add_location(cid, http_url.clone(), &bob.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();

        // Filter by "radiroh" returns only the iroh URL.
        let iroh_locations = artifact.locations_by_scheme("radiroh");
        assert_eq!(iroh_locations.len(), 1);
        assert_eq!(iroh_locations[0].0, &iroh_url);
        assert_eq!(iroh_locations[0].1, &Did::from(alice.signer.public_key()));

        // Filter by "https" returns only the https URL.
        let https_locations = artifact.locations_by_scheme("https");
        assert_eq!(https_locations.len(), 1);
        assert_eq!(https_locations[0].0, &https_url);

        // Filter by "http" returns only Bob's http URL.
        let http_locations = artifact.locations_by_scheme("http");
        assert_eq!(http_locations.len(), 1);
        assert_eq!(http_locations[0].0, &http_url);
        assert_eq!(http_locations[0].1, &Did::from(bob.signer.public_key()));

        // Filter by unknown scheme returns empty.
        assert!(artifact.locations_by_scheme("ftp").is_empty());
    }

    #[test]
    fn locations_by_scheme_duplicate_url_from_two_dids() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // Both Alice and Bob add the same bare radiroh:// URL.
        let iroh_url = Url::parse("radiroh://").unwrap();
        release
            .add_location(cid, iroh_url.clone(), &alice.signer)
            .unwrap();
        release
            .add_location(cid, iroh_url.clone(), &bob.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let iroh_locations = artifact.locations_by_scheme("radiroh");

        // Both DIDs contributed the same URL, so we get two entries.
        assert_eq!(iroh_locations.len(), 2);
        let dids: std::collections::BTreeSet<&Did> =
            iroh_locations.iter().map(|(_, did)| *did).collect();
        assert!(dids.contains(&Did::from(alice.signer.public_key())));
        assert!(dids.contains(&Did::from(bob.signer.public_key())));

        // Both entries point to the same URL.
        assert!(iroh_locations.iter().all(|(url, _)| *url == &iroh_url));
    }

    #[test]
    fn find_by_cid_finds_across_releases() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid1 = commit(&repo.backend, "Release 1");
        let oid2 = commit(&repo.backend, "Release 2");
        let mut releases = Releases::open(&*repo).unwrap();

        let cid1 = test_cid(1);
        let cid2 = test_cid(2);
        let cid_missing = test_cid(99);

        // Create two releases with different artifacts.
        {
            let mut r1 = releases.create(oid1, None, &alice.signer).unwrap();
            r1.register_artifact(cid1, "artifact-one".into(), &alice.signer)
                .unwrap();
        }
        {
            let mut r2 = releases.create(oid2, None, &alice.signer).unwrap();
            r2.register_artifact(cid2, "artifact-two".into(), &alice.signer)
                .unwrap();
        }

        // find_by_cid locates cid1 in the first release.
        let found = releases.find_by_cid(&cid1).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.oid(), &oid1);
        assert!(found[0].1.artifact(&cid1).is_some());

        // find_by_cid locates cid2 in the second release.
        let found = releases.find_by_cid(&cid2).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.oid(), &oid2);
        assert!(found[0].1.artifact(&cid2).is_some());

        // A CID not in any release returns an empty vec.
        assert!(releases.find_by_cid(&cid_missing).unwrap().is_empty());
    }

    #[test]
    fn find_by_cid_aggregates_duplicate_oid_releases() {
        // Two releases exist for the same OID (legacy state from before the
        // refactor, or concurrent creation across unsynced nodes), and both
        // contain an artifact with the same CID. find_by_cid returns both.
        let test::setup::Network {
            alice, bob, rid, ..
        } = test::setup::Network::default();
        let repo = alice.storage.repository(rid).unwrap();
        let oid = commit(&repo.backend, "v1.0");
        let mut releases = Releases::open(&repo).unwrap();

        let cid = test_cid(1);
        {
            let mut r = releases.create(oid, None, &alice.signer).unwrap();
            r.register_artifact(cid, "alice-built".into(), &alice.signer)
                .unwrap();
        }
        {
            let mut r = releases.create(oid, None, &bob.signer).unwrap();
            r.register_artifact(cid, "bob-built".into(), &bob.signer)
                .unwrap();
        }

        let found = releases.find_by_cid(&cid).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|(_, r)| r.oid() == &oid));
    }

    #[test]
    fn find_by_cid_aggregates_across_different_oids() {
        // The same artifact CID is attached to releases for two different
        // commits (e.g. an artifact that's identical across versions).
        // find_by_cid surfaces both so retrieval can union locations.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid1 = commit(&repo.backend, "v1.0");
        let oid2 = commit(&repo.backend, "v1.1");
        let mut releases = Releases::open(&*repo).unwrap();

        let cid = test_cid(1);
        {
            let mut r = releases.create(oid1, None, &alice.signer).unwrap();
            r.register_artifact(cid, "shared".into(), &alice.signer)
                .unwrap();
        }
        {
            let mut r = releases.create(oid2, None, &alice.signer).unwrap();
            r.register_artifact(cid, "shared".into(), &alice.signer)
                .unwrap();
        }

        let found = releases.find_by_cid(&cid).unwrap();
        assert_eq!(found.len(), 2);
        let oids: BTreeSet<_> = found.iter().map(|(_, r)| *r.oid()).collect();
        assert!(oids.contains(&oid1));
        assert!(oids.contains(&oid2));
    }

    #[test]
    fn create_records_tag_oid() {
        // The tag argument passed to create must be persisted on the
        // resulting release and survive a subsequent lookup.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Tagged release");
        let tag_oid = annotate_tag(&repo.backend, "v1", oid, "release v1");
        let mut releases = Releases::open(&*repo).unwrap();

        let id = *releases
            .create(oid, Some(tag_oid), &alice.signer)
            .unwrap()
            .id();
        let release = releases.get(&id).unwrap().unwrap();
        assert_eq!(release.tag(), Some(&tag_oid));
        assert_eq!(release.oid(), &oid);
    }

    #[test]
    fn create_rejects_tag_pointing_at_other_commit() {
        // Recording a tag whose target is a different commit would leave
        // the COB permanently inconsistent, so create must refuse it.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let other = commit(&repo.backend, "unrelated commit");
        let tag_oid = annotate_tag(&repo.backend, "v1", other, "tag points elsewhere");
        let mut releases = Releases::open(&*repo).unwrap();

        let result = releases.create(oid, Some(tag_oid), &alice.signer);
        match result {
            Err(crate::error::Create::TagMismatch {
                tag,
                expected,
                actual,
            }) => {
                assert_eq!(tag, tag_oid);
                assert_eq!(expected, oid);
                assert_eq!(actual, other);
            }
            Err(other) => panic!("expected TagMismatch, got {other:?}"),
            Ok(_) => panic!("expected TagMismatch, got Ok"),
        }
    }

    #[test]
    fn create_rejects_unknown_tag_oid() {
        // An OID that isn't actually a tag object in the repo must be
        // rejected rather than silently stored as opaque bytes.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let bogus = test::arbitrary::oid();
        let mut releases = Releases::open(&*repo).unwrap();

        let result = releases.create(oid, Some(bogus), &alice.signer);
        match result {
            Err(crate::error::Create::MissingTag { tag, .. }) => assert_eq!(tag, bogus),
            Err(other) => panic!("expected MissingTag, got {other:?}"),
            Ok(_) => panic!("expected MissingTag, got Ok"),
        }
    }

    #[test]
    fn create_rejects_commit_oid_as_tag() {
        // A commit OID supplied where an annotated tag is expected
        // (e.g. lightweight tag confusion) must error rather than be
        // recorded as a tag.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let mut releases = Releases::open(&*repo).unwrap();

        let result = releases.create(oid, Some(oid), &alice.signer);
        match result {
            Err(crate::error::Create::MissingTag { tag, .. }) => assert_eq!(tag, oid),
            Err(other) => panic!("expected MissingTag, got {other:?}"),
            Ok(_) => panic!("expected MissingTag, got Ok"),
        }
    }

    #[test]
    fn create_without_tag_leaves_none() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Plain commit release");
        let mut releases = Releases::open(&*repo).unwrap();

        let id = *releases.create(oid, None, &alice.signer).unwrap().id();
        let release = releases.get(&id).unwrap().unwrap();
        assert_eq!(release.tag(), None);
    }

    #[test]
    fn tag_persists_through_reload() {
        // Reopening the store from disk must still surface the tag
        // recorded at creation time.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Tagged release");
        let tag_oid = annotate_tag(&repo.backend, "v1", oid, "release v1");

        let id = {
            let mut releases = Releases::open(&*repo).unwrap();
            *releases
                .create(oid, Some(tag_oid), &alice.signer)
                .unwrap()
                .id()
        };
        let releases = Releases::open(&*repo).unwrap();
        let release = releases.get(&id).unwrap().unwrap();
        assert_eq!(release.tag(), Some(&tag_oid));
    }

    #[test]
    fn creator_persists_through_reload() {
        // The creator DID is recorded once at creation and must survive
        // a store reopen — it's the input to delegate-priority rules.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let alice_did = Did::from(*alice.signer.public_key());

        let id = {
            let mut releases = Releases::open(&*repo).unwrap();
            *releases.create(oid, None, &alice.signer).unwrap().id()
        };
        let releases = Releases::open(&*repo).unwrap();
        let release = releases.get(&id).unwrap().unwrap();
        assert_eq!(release.creator(), &alice_did);
    }

    #[test]
    fn tag_field_default_none_on_old_actions() {
        // A persisted Action::Create JSON without the `tag` field must
        // deserialize back into an Action::Create whose tag is None,
        // ensuring backward-compatibility with COBs created before
        // this field existed.
        use crate::Action;
        let oid = test::arbitrary::oid();
        let json = format!(r#"{{"Create":{{"oid":"{oid}"}}}}"#);
        let action: Action = serde_json::from_str(&json).unwrap();
        match action {
            Action::Create { oid: parsed, tag } => {
                assert_eq!(parsed, oid);
                assert_eq!(tag, None);
            }
            _ => panic!("expected Action::Create"),
        }
    }

    #[test]
    fn register_artifact_wire_name_stays_add_artifact() {
        // RegisterArtifact must serialize under the legacy `AddArtifact`
        // tag so COBs written before the rename still deserialize. Guards
        // the #[serde(rename)] on the variant.
        use crate::Action;
        let action = Action::RegisterArtifact {
            cid: test_cid(1),
            name: "linux-amd64 binary".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(
            json.contains(r#""AddArtifact""#),
            "expected legacy wire tag, got {json}"
        );
        // A legacy-tagged payload round-trips back into RegisterArtifact.
        let parsed: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, action);
    }

    /// Visual smoke test: render a release with one artifact, one location and
    /// one attestation in both compact (list) and detailed (show) styles. We
    /// assert key visible substrings so the structure is locked in, and dump
    /// the rendered output via `eprintln!` so `cargo test -- --nocapture` can
    /// be used as a quick eyeball check during development.
    #[test]
    fn pretty_renders_compact_and_detailed() {
        use std::collections::HashMap;

        use crate::display;

        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Initial commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();
        let cid = test_cid(1);
        release
            .register_artifact(cid, "linux-amd64.tar.gz".into(), &alice.signer)
            .unwrap();
        release
            .add_location(
                cid,
                Url::parse("https://alice.example.com/linux-amd64.tar.gz").unwrap(),
                &alice.signer,
            )
            .unwrap();
        // Bob attests; alice's self-attestation would be a no-op.
        release.attest(cid, &bob.signer).unwrap();
        let id = *release.id();
        drop(release);

        let release = releases.get(&id).unwrap().unwrap();
        use radicle::storage::ReadRepository;
        let delegates: BTreeSet<Did> = repo.delegates().unwrap().into_iter().collect();
        let aliases: HashMap<radicle::node::NodeId, radicle::node::Alias> = HashMap::new();
        let filters = display::Filters {
            delegates: &delegates,
            redacted: false,
            all_authors: false,
            local: None,
        };
        let title = display::CommitTitle::title(&*repo, release.oid());
        let shown = display::Release::new(id, &release, &aliases, filters, title, None);

        let plain = display::Style::plain(false);
        let compact = shown.pretty_compact(plain);
        let detailed = shown.pretty(plain);

        eprintln!("=== compact ===\n{compact}");
        eprintln!("=== detailed ===\n{detailed}");

        // Compact carries the bullet, the artifact name, and the attestation
        // badge with its count.
        assert!(compact.contains('●'));
        assert!(compact.contains("linux-amd64.tar.gz"));
        assert!(compact.contains("✓1"));
        assert!(compact.contains("Initial commit"));

        // Detailed surfaces labeled fields and the location URL.
        assert!(detailed.contains("Artifacts (1 item)"));
        assert!(detailed.contains("cid"));
        assert!(detailed.contains("author"));
        assert!(detailed.contains("locations"));
        assert!(detailed.contains("attestations"));
        assert!(detailed.contains("alice.example.com/linux-amd64.tar.gz"));
    }

    #[test]
    fn set_metadata_basic() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "build-env".into(), "nix --pure".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(
            artifact.metadata().get("build-env"),
            Some(&serde_json::json!("nix --pure")),
        );
    }

    #[test]
    fn set_metadata_accepts_json_object() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        let value = serde_json::json!({
            "format": "cyclonedx",
            "url": "https://example.com/sbom.json",
        });
        release
            .set_metadata(cid, "sbom".into(), value.clone(), &alice.signer)
            .unwrap();

        assert_eq!(
            release.artifact(&cid).unwrap().metadata().get("sbom"),
            Some(&value),
        );
    }

    #[test]
    fn set_metadata_last_writer_wins() {
        // The COB layer is permissive: any signer may overwrite any key.
        // CLI-level authorization is what gates real-world contributions.
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "key".into(), "first".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "key".into(), "second".into(), &bob.signer)
            .unwrap();

        assert_eq!(
            release.artifact(&cid).unwrap().metadata().get("key"),
            Some(&serde_json::json!("second")),
        );
    }

    #[test]
    fn remove_metadata_drops_key() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "a".into(), "1".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "b".into(), "2".into(), &alice.signer)
            .unwrap();
        release
            .remove_metadata(cid, "a".into(), &alice.signer)
            .unwrap();

        let metadata = release.artifact(&cid).unwrap().metadata();
        assert!(metadata.get("a").is_none());
        assert_eq!(metadata.get("b"), Some(&serde_json::json!("2")));
    }

    #[test]
    fn set_metadata_rejects_invalid_keys() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();

        assert!(matches!(
            release.set_metadata(cid, "".into(), "v".into(), &alice.signer),
            Err(crate::error::Metadata::EmptyKey),
        ));

        let too_long = "x".repeat(crate::MAX_METADATA_KEY_LEN + 1);
        assert!(matches!(
            release.set_metadata(cid, too_long, "v".into(), &alice.signer),
            Err(crate::error::Metadata::KeyTooLong { .. }),
        ));

        assert!(matches!(
            release.set_metadata(cid, "bad\nkey".into(), "v".into(), &alice.signer),
            Err(crate::error::Metadata::KeyControlChar { ch: '\n' }),
        ));

        // None of the rejected keys should have been recorded.
        assert!(release.artifact(&cid).unwrap().metadata().is_empty());
    }

    #[test]
    fn set_metadata_rejects_oversized_value() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();

        // A string of MAX+1 bytes serializes to MAX+3 with the surrounding quotes.
        let big = serde_json::Value::String("x".repeat(crate::MAX_METADATA_VALUE_LEN + 1));
        assert!(matches!(
            release.set_metadata(cid, "k".into(), big, &alice.signer),
            Err(crate::error::Metadata::ValueTooLarge { .. }),
        ));
        assert!(release.artifact(&cid).unwrap().metadata().is_empty());
    }

    #[test]
    fn set_metadata_for_missing_cid_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(99);
        release
            .set_metadata(cid, "k".into(), "v".into(), &alice.signer)
            .unwrap();

        assert!(release.artifact(&cid).is_none());
    }

    #[test]
    fn remove_metadata_for_missing_key_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        release
            .remove_metadata(cid, "missing".into(), &alice.signer)
            .unwrap();

        assert!(release.artifact(&cid).unwrap().metadata().is_empty());
    }

    #[test]
    fn metadata_persists_through_reload() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "build".into(), "ok".into(), &alice.signer)
            .unwrap();

        let id = *release.id();
        drop(release);
        let release = releases.get(&id).unwrap().unwrap();
        assert_eq!(
            release.artifact(&cid).unwrap().metadata().get("build"),
            Some(&serde_json::json!("ok")),
        );
    }

    #[test]
    fn display_renders_metadata() {
        use std::collections::HashMap;

        use crate::display;

        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .register_artifact(cid, "binary".into(), &alice.signer)
            .unwrap();
        release
            .set_metadata(cid, "build".into(), "ok".into(), &alice.signer)
            .unwrap();
        let id = *release.id();
        drop(release);

        let release = releases.get(&id).unwrap().unwrap();
        let delegates: BTreeSet<Did> = BTreeSet::new();
        let aliases: HashMap<radicle::node::NodeId, radicle::node::Alias> = HashMap::new();
        let filters = display::Filters {
            delegates: &delegates,
            redacted: false,
            all_authors: true,
            local: None,
        };
        let shown = display::Release::new(id, &release, &aliases, filters, None, None);
        let detailed = shown.pretty(display::Style::plain(false));
        assert!(detailed.contains("build"));
        assert!(detailed.contains("ok"));
    }

    #[test]
    fn display_marks_seeding_for_local_endpoint_location() {
        use std::collections::HashMap;

        use crate::display;
        use radicle_artifact_core::keys::EndpointId;

        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, None, &alice.signer).unwrap();

        let alice_did = Did::from(alice.signer.public_key());
        // Our own radiroh:// endpoint URL, derived from our DID.
        let endpoint_url = EndpointId::try_from(&alice_did).unwrap().to_url();

        let seeded = test_cid(1);
        let unseeded = test_cid(2);
        release
            .register_artifact(seeded, "seeded".into(), &alice.signer)
            .unwrap();
        release
            .register_artifact(unseeded, "unseeded".into(), &alice.signer)
            .unwrap();
        // Advertise ourselves as a provider for `seeded` only.
        release
            .add_location(seeded, endpoint_url, &alice.signer)
            .unwrap();
        let id = *release.id();
        drop(release);

        let release = releases.get(&id).unwrap().unwrap();
        let delegates: BTreeSet<Did> = BTreeSet::new();
        let aliases: HashMap<radicle::node::NodeId, radicle::node::Alias> = HashMap::new();
        let filters = display::Filters {
            delegates: &delegates,
            redacted: false,
            all_authors: true,
            local: Some(&alice_did),
        };
        let shown = display::Release::new(id, &release, &aliases, filters, None, None);
        let out = shown.pretty(display::Style::plain(false));
        // The seedling appears once: for the artifact we provide.
        assert_eq!(out.matches('🌱').count(), 1);

        // Without a local DID nothing is marked as seeded.
        let anon = display::Filters {
            local: None,
            ..filters
        };
        let anon_out = display::Release::new(id, &release, &aliases, anon, None, None)
            .pretty(display::Style::plain(false));
        assert!(!anon_out.contains('🌱'));
    }

    /// A migrated in-memory cache for tests.
    fn memory_cache() -> crate::cache::Store {
        crate::cache::Store::memory()
            .unwrap()
            .with_migrations()
            .unwrap()
    }

    #[test]
    fn cache_populated_on_read() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let cache = memory_cache();
        let mut releases = Releases::open(&*repo).unwrap().with_cache(cache.clone());

        let cid = test_cid(1);
        let url = Url::parse("https://alice.example.com/bin.tar.gz").unwrap();
        let (id, timestamp) = {
            let mut release = releases.create(oid, None, &alice.signer).unwrap();
            release
                .register_artifact(cid, "linux binary".into(), &alice.signer)
                .unwrap();
            release
                .add_location(cid, url.clone(), &alice.signer)
                .unwrap();
            (*release.id(), release.timestamp())
        };

        // Writes never touch the cache; it is populated lazily on the first read.
        let repo_id = repo.id;
        assert!(cache.get(&repo_id, &id).unwrap().is_none());

        // A cache-backed read validates the COB's git tips and materializes it.
        releases.get(&id).unwrap().expect("release exists");

        let (_, cached) = cache
            .get(&repo_id, &id)
            .unwrap()
            .expect("release is cached after read");
        assert_eq!(cached.artifact(&cid).unwrap().name(), "linux binary");
        // The timestamp round-trips via the release blob.
        assert_eq!(cached.timestamp(), timestamp);

        // The locations index was rebuilt on read.
        let locations = cache.locations_for(&repo_id, &cid).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].2, url);

        // Public cache-backed lookups agree.
        assert_eq!(releases.count().unwrap(), 1);
        assert_eq!(releases.find_by_cid(&cid).unwrap().len(), 1);
        let via_releases = releases.locations_for(&cid).unwrap();
        assert_eq!(via_releases.len(), 1);
        assert_eq!(via_releases[0].2, url);
    }

    #[test]
    fn cache_reflects_external_change() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let cache = memory_cache();
        let mut releases = Releases::open(&*repo).unwrap().with_cache(cache);

        let id = {
            let mut release = releases.create(oid, None, &alice.signer).unwrap();
            release
                .register_artifact(test_cid(1), "first".into(), &alice.signer)
                .unwrap();
            *release.id()
        };
        assert_eq!(releases.get(&id).unwrap().unwrap().artifacts().len(), 1);

        // A change that does NOT go through the cached handle, simulating a
        // remote op fetched into git. The cache is now stale.
        {
            let mut uncached = Releases::open(&*repo).unwrap();
            uncached
                .get_mut(&id)
                .unwrap()
                .register_artifact(test_cid(2), "second".into(), &alice.signer)
                .unwrap();
        }

        // The freshness check detects the moved tip and re-materializes on read.
        let refreshed = releases.get(&id).unwrap().unwrap();
        assert_eq!(refreshed.artifacts().len(), 2);
        let all: Vec<_> = releases.all().unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.artifacts().len(), 2);
    }

    #[test]
    fn cache_locations_index_tracks_removals() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let mut releases = Releases::open(&*repo).unwrap().with_cache(memory_cache());

        let cid = test_cid(1);
        let a = Url::parse("https://a.example.com/x").unwrap();
        let b = Url::parse("https://b.example.com/x").unwrap();
        {
            let mut release = releases.create(oid, None, &alice.signer).unwrap();
            release
                .register_artifact(cid, "bin".into(), &alice.signer)
                .unwrap();
            release.add_location(cid, a.clone(), &alice.signer).unwrap();
            release.add_location(cid, b.clone(), &alice.signer).unwrap();
            // Removing a location must be reflected in the rebuilt index.
            release.remove_location(cid, a, &alice.signer).unwrap();
        }

        let locations = releases.locations_for(&cid).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].2, b);
    }

    /// Initialize a second repository in `node`'s storage with a distinct name
    /// (distinct identity doc, hence distinct RID), returning its handle. No
    /// test fixture gives a single storage with multiple repos, so we init one.
    fn init_repo(node: &test::setup::Node, name: &str) -> radicle::storage::git::Repository {
        let (working, _) = test::fixtures::repository(node.root.join(format!("working-{name}")));
        let (rid, _, _) = radicle::rad::init(
            &working,
            name.try_into().unwrap(),
            "second project",
            radicle::git::fmt::refname!("master"),
            radicle::identity::Visibility::default(),
            &node.signer,
            &node.storage,
        )
        .unwrap();
        node.storage.repository(rid).unwrap()
    }

    #[test]
    fn index_aggregates_across_repos() {
        let test::setup::NodeWithRepo { node, repo } = test::setup::NodeWithRepo::default();
        let repo2 = init_repo(&node, "beta");

        let cid = test_cid(1);
        let url1 = Url::parse("https://one.example.com/x").unwrap();
        let url2 = Url::parse("https://two.example.com/x").unwrap();

        // A release referencing the same `cid` in each repository.
        let oid1 = commit(&repo.backend, "r1");
        {
            let mut releases = Releases::open(&*repo).unwrap();
            let mut r = releases.create(oid1, None, &node.signer).unwrap();
            r.register_artifact(cid, "one".into(), &node.signer)
                .unwrap();
            r.add_location(cid, url1.clone(), &node.signer).unwrap();
        }
        let oid2 = commit(&repo2.backend, "r2");
        {
            let mut releases = Releases::open(&repo2).unwrap();
            let mut r = releases.create(oid2, None, &node.signer).unwrap();
            r.register_artifact(cid, "two".into(), &node.signer)
                .unwrap();
            r.add_location(cid, url2.clone(), &node.signer).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let index = crate::discovery::Index::open(&node.storage, tmp.path().join("db"));

        // Locations are aggregated across both repositories.
        let locations = index.locations_by_cid(&cid).unwrap();
        assert_eq!(locations.len(), 2);
        let repos: BTreeSet<_> = locations.iter().map(|l| l.repo).collect();
        assert!(repos.contains(&repo.id) && repos.contains(&repo2.id));
        let urls: BTreeSet<_> = locations.iter().map(|l| l.url.clone()).collect();
        assert!(urls.contains(&url1) && urls.contains(&url2));

        // As are releases.
        let matches = index.releases_by_cid(&cid).unwrap();
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|m| m.release.artifact(&cid).is_some()));
    }

    #[test]
    fn index_reflects_fresh_writes() {
        let test::setup::NodeWithRepo { node, repo } = test::setup::NodeWithRepo::default();
        let cid = test_cid(1);
        let a = Url::parse("https://a.example.com/x").unwrap();
        let b = Url::parse("https://b.example.com/x").unwrap();

        let oid = commit(&repo.backend, "r1");
        let id = {
            let mut releases = Releases::open(&*repo).unwrap();
            let mut r = releases.create(oid, None, &node.signer).unwrap();
            r.register_artifact(cid, "one".into(), &node.signer)
                .unwrap();
            r.add_location(cid, a.clone(), &node.signer).unwrap();
            *r.id()
        };

        let tmp = tempfile::tempdir().unwrap();
        let index = crate::discovery::Index::open(&node.storage, tmp.path().join("db"));

        // A cold query materializes and caches: one location.
        assert_eq!(index.locations_by_cid(&cid).unwrap().len(), 1);

        // Add a location WITHOUT going through the index's cache.
        {
            let mut releases = Releases::open(&*repo).unwrap();
            releases
                .get_mut(&id)
                .unwrap()
                .add_location(cid, b.clone(), &node.signer)
                .unwrap();
        }

        // The index refreshes each repo before querying, so the write shows up.
        assert_eq!(index.locations_by_cid(&cid).unwrap().len(), 2);
    }

    #[test]
    fn open_cached_degrades_to_git_when_cache_unavailable() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let cid = test_cid(1);
        let url = Url::parse("https://x.example.com/x").unwrap();
        {
            let mut releases = Releases::open(&*repo).unwrap();
            let mut r = releases.create(oid, None, &alice.signer).unwrap();
            r.register_artifact(cid, "bin".into(), &alice.signer)
                .unwrap();
            r.add_location(cid, url.clone(), &alice.signer).unwrap();
        }

        // A cache path whose parent directory does not exist cannot be opened;
        // open_cached must log and disable the cache, not fail.
        let unusable = alice.root.join("no-such-dir").join("cache.db");
        let releases = Releases::open_cached(&*repo, unusable).unwrap();

        // Reads still succeed, materialized straight from git.
        assert_eq!(releases.find_by_cid(&cid).unwrap().len(), 1);
        let locations = releases.locations_for(&cid).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].2, url);
    }

    #[test]
    fn index_aggregates_without_cache() {
        let test::setup::NodeWithRepo { node, repo } = test::setup::NodeWithRepo::default();
        let repo2 = init_repo(&node, "beta");
        let cid = test_cid(1);
        let url1 = Url::parse("https://one.example.com/x").unwrap();
        let url2 = Url::parse("https://two.example.com/x").unwrap();

        let oid1 = commit(&repo.backend, "r1");
        {
            let mut releases = Releases::open(&*repo).unwrap();
            let mut r = releases.create(oid1, None, &node.signer).unwrap();
            r.register_artifact(cid, "one".into(), &node.signer)
                .unwrap();
            r.add_location(cid, url1, &node.signer).unwrap();
        }
        let oid2 = commit(&repo2.backend, "r2");
        {
            let mut releases = Releases::open(&repo2).unwrap();
            let mut r = releases.create(oid2, None, &node.signer).unwrap();
            r.register_artifact(cid, "two".into(), &node.signer)
                .unwrap();
            r.add_location(cid, url2, &node.signer).unwrap();
        }

        // An unopenable cache path forces the Index onto the git-materialization path; the
        // cross-repo aggregation must still work without a cache.
        let index =
            crate::discovery::Index::open(&node.storage, node.root.join("nope").join("cache.db"));
        assert_eq!(index.locations_by_cid(&cid).unwrap().len(), 2);
    }

    #[test]
    fn cache_prunes_releases_absent_from_git() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let cache = memory_cache();
        let mut releases = Releases::open(&*repo).unwrap().with_cache(cache.clone());

        // A real release, then a read to materialize it into the cache.
        let real_id = {
            let mut r = releases.create(oid, None, &alice.signer).unwrap();
            r.register_artifact(test_cid(1), "bin".into(), &alice.signer)
                .unwrap();
            *r.id()
        };
        releases.get(&real_id).unwrap().expect("release exists");

        // Inject a cached entry for a COB id that does not exist in git.
        let bogus_id = crate::ReleaseId::from(commit(&repo.backend, "not a cob"));
        let release = cache.get(&repo.id, &real_id).unwrap().unwrap().1;
        cache
            .update(&repo.id, &bogus_id, "stale-head", &release)
            .unwrap();
        assert!(cache.get(&repo.id, &bogus_id).unwrap().is_some());

        // A repo-wide cached read refreshes and prunes entries absent from git.
        assert_eq!(releases.all().unwrap().count(), 1);
        assert!(cache.get(&repo.id, &bogus_id).unwrap().is_none());
    }

    #[test]
    fn count_counts_release_cobs_without_folding() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let cid = test_cid(1);

        let first = {
            let mut releases = Releases::open(&*repo).unwrap();
            let ids: Vec<_> = (0..3)
                .map(|i| {
                    let oid = commit(&repo.backend, &format!("r{i}"));
                    let mut r = releases.create(oid, None, &alice.signer).unwrap();
                    r.register_artifact(cid, format!("bin-{i}"), &alice.signer)
                        .unwrap();
                    *r.id()
                })
                .collect();
            ids[0]
        };

        // Count is the number of release COBs, from a ref walk. A cache-backed
        // handle agrees, and neither materializes a release to answer.
        assert_eq!(Releases::open(&*repo).unwrap().count().unwrap(), 3);
        let cached = Releases::open(&*repo).unwrap().with_cache(memory_cache());
        assert_eq!(cached.count().unwrap(), 3);

        // Redacting an artifact rewrites a release's contents but not the set of
        // COBs, so the count is unchanged.
        {
            let mut releases = Releases::open(&*repo).unwrap();
            releases
                .get_mut(&first)
                .unwrap()
                .redact(cid, "oops".into(), &alice.signer)
                .unwrap();
        }
        assert_eq!(Releases::open(&*repo).unwrap().count().unwrap(), 3);
    }

    #[test]
    fn cache_finds_artifact_without_location() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "release commit");
        let cache = memory_cache();
        let mut releases = Releases::open(&*repo).unwrap().with_cache(cache.clone());

        let cid = test_cid(1);
        {
            let mut r = releases.create(oid, None, &alice.signer).unwrap();
            // Register the artifact but add no location.
            r.register_artifact(cid, "bin".into(), &alice.signer)
                .unwrap();
        }

        // The public cache-backed lookup matches the location-less artifact,
        // while locations_for returns nothing.
        assert_eq!(releases.find_by_cid(&cid).unwrap().len(), 1);
        assert!(releases.locations_for(&cid).unwrap().is_empty());

        // It is backed by a sentinel row: releases_for_cid matches by cid, but
        // the locations index filters the empty-url sentinel out.
        assert_eq!(cache.releases_for_cid(&repo.id, &cid).unwrap().len(), 1);
        assert!(cache.locations_for(&repo.id, &cid).unwrap().is_empty());
    }
}
