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
//! let mut release = releases.create(oid, &alice.signer).unwrap();
//!
//! let cid: Cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi".parse().unwrap();
//! let url = Url::parse("https://example.com/artifacts/linux-amd64.tar.gz").unwrap();
//! release.add_artifact(cid, "linux-amd64 binary".into(), &alice.signer).unwrap();
//! release.add_location(cid, url, &alice.signer).unwrap();
//! ```

#![deny(missing_docs)]

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

// Re-export cid::Cid as the content identifier type.
// A Cid has both a binary representation (the struct itself) and a string
// representation (multibase-encoded, used for Display/FromStr/JSON serde).
pub use cid::Cid;

pub mod display;
pub mod error;

/// Type name of an artifact release.
pub static TYPENAME: LazyLock<TypeName> =
    LazyLock::new(|| FromStr::from_str("org.radworks.artifact").expect("type name is valid"));

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

/// A `Release` groups content-addressed artifacts under a single Git OID
/// (annotated tag or commit).
///
/// Multiple artifacts can exist per release, and multiple users can announce
/// discovery locations for each artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// The DID of the user that created this release.
    author: Did,
    oid: Oid,
    artifacts: IndexMap<Cid, Artifact>,
}

/// A single artifact identified by its [`Cid`].
///
/// Each artifact has a human-readable `name` describing what it is, and a set
/// of discovery locations contributed by various users identified by their DIDs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    name: String,
    locations: BTreeMap<Did, BTreeSet<Url>>,
    /// Nodes that have independently verified this artifact's CID.
    #[serde(default)]
    attestations: BTreeSet<Did>,
    /// Structured metadata contributed by users, keyed by DID then by
    /// reverse-DNS namespace key (e.g. `xyz.example.build-env`).
    /// Each user manages their own entries independently.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<Did, BTreeMap<String, serde_json::Value>>,
}

impl Artifact {
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

    /// Get all structured metadata, keyed by contributing DID.
    pub fn metadata(&self) -> &BTreeMap<Did, BTreeMap<String, serde_json::Value>> {
        &self.metadata
    }

    /// Get metadata contributed by a specific DID.
    pub fn metadata_of(&self, user: &Did) -> Option<&BTreeMap<String, serde_json::Value>> {
        self.metadata.get(user)
    }

    /// Collect values for a specific key across all contributing DIDs.
    pub fn all_metadata_for_key(&self, key: &str) -> Vec<(&Did, &serde_json::Value)> {
        self.metadata
            .iter()
            .filter_map(|(did, map)| map.get(key).map(|v| (did, v)))
            .collect()
    }
}

/// The collaborative object actions for artifact releases.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    /// Create a [`Release`] for the given [`Oid`].
    ///
    /// Must be the first action. Subsequent `Create` actions are ignored.
    Create {
        /// The commit or annotated tag OID this release corresponds to.
        oid: Oid,
    },
    /// Add an artifact to the release.
    ///
    /// Idempotent — ignored if the CID already exists.
    AddArtifact {
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
    /// Silent no-op if the CID does not exist in the release.
    Attest {
        /// The content identifier of the artifact to attest.
        cid: Cid,
    },
    /// Set a structured metadata entry on an artifact.
    ///
    /// Each user manages their own metadata independently. Setting the same
    /// key again overwrites the previous value for that user. Keys follow
    /// reverse-DNS convention (e.g. `xyz.example.build-env`).
    /// Silent no-op if the CID does not exist in the release.
    SetMetadata {
        /// The content identifier of the artifact.
        cid: Cid,
        /// Metadata key (reverse-DNS convention recommended).
        key: String,
        /// Arbitrary JSON value.
        value: serde_json::Value,
    },
    /// Remove a metadata entry previously set by this user.
    ///
    /// No-op if the key, user, or CID is not found.
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
            Action::Create { oid } => vec![*oid],
            _ => Vec::new(),
        }
    }
}

impl Release {
    /// Construct a new [`Release`].
    fn new(oid: Oid, author: Did) -> Self {
        Self {
            author,
            oid,
            artifacts: IndexMap::new(),
        }
    }

    /// Get the [`Did`] of the user that created this release.
    pub fn author(&self) -> &Did {
        &self.author
    }

    /// Get the [`Oid`] this release is associated with.
    pub fn oid(&self) -> &Oid {
        &self.oid
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
            Action::AddArtifact { cid, name } => {
                // Idempotent: only insert if CID is new.
                self.artifacts.entry(cid).or_insert_with(|| Artifact {
                    name,
                    locations: BTreeMap::new(),
                    attestations: BTreeSet::new(),
                    metadata: BTreeMap::new(),
                });
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
                    artifact.attestations.insert(user);
                }
            }
            Action::SetMetadata { cid, key, value } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    artifact.metadata.entry(user).or_default().insert(key, value);
                }
            }
            Action::RemoveMetadata { cid, key } => {
                if let Some(artifact) = self.artifacts.get_mut(&cid) {
                    if let Entry::Occupied(mut e) = artifact.metadata.entry(user) {
                        e.get_mut().remove(&key);
                        if e.get().is_empty() {
                            e.remove();
                        }
                    }
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
        let Some(Action::Create { oid }) = actions.next() else {
            return Err(error::Build::Initial);
        };
        repo.commit(oid)
            .map_err(|err| error::Build::MissingCommit { oid, err })?;
        let author = Did::from(op.author);
        let mut release = Self::new(oid, author);
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
    raw: store::Store<'a, Release, R>,
}

impl<'a, R> Deref for Releases<'a, R> {
    type Target = store::Store<'a, Release, R>;

    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

impl<'a, R> Releases<'a, R>
where
    R: ReadRepository + cob::Store<Namespace = NodeId>,
{
    /// Open a releases store.
    pub fn open(repository: &'a R) -> Result<Self, RepositoryError> {
        let identity = repository.identity_head()?;
        let raw = store::Store::open(repository)?.identity(identity);

        Ok(Self { raw })
    }

    /// Return the number of [`Release`]s in the store.
    ///
    /// Note: this deserializes every COB, so it is O(n).
    pub fn count(&self) -> Result<usize, store::Error> {
        Ok(self.all()?.count())
    }

    /// Get a [`Release`], given its [`ReleaseId`] identifier.
    pub fn get(&self, id: &ReleaseId) -> Result<Option<Release>, store::Error> {
        self.raw.get(id.as_object_id())
    }

    /// Find the [`Release`]s that are associated with the `wanted` commit.
    pub fn find_by_oid(&self, wanted: Oid) -> Result<FindByOid<'a>, store::Error> {
        FindByOid::new(self, wanted)
    }
}

/// [`Iterator`] for finding each [`Release`] where the [`Release::oid`] matches
/// the wanted commit. See [`Releases::find_by_oid`].
pub struct FindByOid<'a> {
    releases: Box<dyn Iterator<Item = Result<(ObjectId, Release), cob::store::Error>> + 'a>,
    needle: Oid,
}

impl<'a> FindByOid<'a> {
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

impl Iterator for FindByOid<'_> {
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
            .raw
            .get(id.as_object_id())?
            .ok_or_else(move || store::Error::NotFound(TYPENAME.clone(), (*id).into()))?;

        Ok(ReleaseMut {
            id: *id,
            release,
            store: self,
        })
    }

    /// Create a new [`Release`] in the repository.
    pub fn create<'g, G>(
        &'g mut self,
        oid: Oid,
        signer: &Device<G>,
    ) -> Result<ReleaseMut<'a, 'g, R>, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        let (id, release) = store::Transaction::initial::<_, _, Transaction<R>>(
            "Create release",
            &mut self.raw,
            signer,
            |tx, _| {
                tx.create(oid)?;
                Ok(())
            },
        )?;

        Ok(ReleaseMut {
            id: id.into(),
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

    /// Add an artifact to the release.
    pub fn add_artifact<G>(
        &mut self,
        cid: Cid,
        name: String,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Add artifact", signer, |tx| tx.add_artifact(cid, name))
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

    /// Set a structured metadata entry on an artifact.
    pub fn set_metadata<G>(
        &mut self,
        cid: Cid,
        key: String,
        value: serde_json::Value,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Set metadata", signer, |tx| {
            tx.set_metadata(cid, key, value)
        })
    }

    /// Remove a metadata entry previously set by this user.
    pub fn remove_metadata<G>(
        &mut self,
        cid: Cid,
        key: String,
        signer: &Device<G>,
    ) -> Result<EntryId, store::Error>
    where
        G: Signer<crypto::Signature>,
    {
        self.transaction("Remove metadata", signer, |tx| {
            tx.remove_metadata(cid, key)
        })
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

        let (release, commit) =
            tx.0.commit(message, self.id.into(), &mut self.store.raw, signer)?;
        self.release = release;

        Ok(commit)
    }
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
    fn create(&mut self, oid: Oid) -> Result<(), store::Error> {
        self.0.push(Action::Create { oid })
    }

    /// Add an artifact to the transaction.
    fn add_artifact(&mut self, cid: Cid, name: String) -> Result<(), store::Error> {
        self.0.push(Action::AddArtifact { cid, name })
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

    /// Set a metadata entry on an artifact.
    fn set_metadata(
        &mut self,
        cid: Cid,
        key: String,
        value: serde_json::Value,
    ) -> Result<(), store::Error> {
        self.0.push(Action::SetMetadata { cid, key, value })
    }

    /// Remove a metadata entry from an artifact.
    fn remove_metadata(&mut self, cid: Cid, key: String) -> Result<(), store::Error> {
        self.0.push(Action::RemoveMetadata { cid, key })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod test {
    use radicle::git::{raw::Repository, Oid};
    use radicle::identity::Did;
    use radicle::test;
    use url::Url;

    use crate::{Cid, Releases};

    /// Create a valid CIDv1 (raw codec, sha2-256) from a distinguishing byte.
    fn test_cid(n: u8) -> Cid {
        use cid::multihash::Multihash;
        let mut digest = [0u8; 32];
        digest[0] = n;
        // 0x12 = sha2-256 hash code, 0x55 = raw codec
        let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
        Cid::new_v1(0x55, mh)
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

    #[test]
    fn e2e() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();

        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        // The release author should be Alice.
        assert_eq!(release.author(), &Did::from(alice.signer.public_key()));

        // Alice adds an artifact.
        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
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
        let release = releases.create(oid, &alice.signer);
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
            let r1 = releases.create(oid, &alice.signer).unwrap();
            r1.id
        };
        let r2 = {
            let r2 = releases.create(oid, &alice.signer).unwrap();
            r2.id
        };

        // COB store deduplicates: same OID + same signer = same release.
        assert_eq!(r1, r2);
        assert_eq!(releases.get(&r1).unwrap(), releases.get(&r2).unwrap());
    }

    #[test]
    fn idempotent_add_artifact() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "first name".into(), &alice.signer)
            .unwrap();
        // Second add with different name is ignored — first name wins.
        release
            .add_artifact(cid, "second name".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.name(), "first name");
        assert_eq!(release.artifacts().len(), 1);
    }

    #[test]
    fn add_location_for_missing_cid_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(99);
        let url = Url::parse("https://example.com/file.tar.gz").unwrap();
        // Should succeed but have no effect since the CID doesn't exist.
        release.add_location(cid, url, &alice.signer).unwrap();

        assert!(release.artifact(&cid).is_none());
    }

    #[test]
    fn find_by_oid_returns_matching_releases() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid1 = commit(&repo.backend, "Commit A");
        let oid2 = commit(&repo.backend, "Commit B");
        let mut releases = Releases::open(&*repo).unwrap();

        let id1 = releases.create(oid1, &alice.signer).unwrap().id;
        let _id2 = releases.create(oid2, &alice.signer).unwrap().id;

        // find_by_oid should return only the release matching oid1.
        let results: Vec<_> = releases
            .find_by_oid(oid1)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id1);
        assert_eq!(results[0].1.oid(), &oid1);
    }

    #[test]
    fn find_by_oid_returns_empty_for_no_match() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Commit A");
        let other_oid = commit(&repo.backend, "Commit B");
        let mut releases = Releases::open(&*repo).unwrap();

        releases.create(oid, &alice.signer).unwrap();

        let results: Vec<_> = releases
            .find_by_oid(other_oid)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
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
        release
            .remove_location(cid, url2, &alice.signer)
            .unwrap();
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // All three delegates attest to the artifact.
        release.attest(cid, &alice.signer).unwrap();
        release.attest(cid, &bob.signer).unwrap();
        release.attest(cid, &carol.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.attestations().len(), 3);
        assert!(artifact.is_attested_by(&Did::from(alice.signer.public_key())));
        assert!(artifact.is_attested_by(&Did::from(bob.signer.public_key())));
        assert!(artifact.is_attested_by(&Did::from(carol.signer.public_key())));
    }

    #[test]
    fn idempotent_attestation() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // Attesting twice from the same node should be a no-op.
        release.attest(cid, &alice.signer).unwrap();
        release.attest(cid, &alice.signer).unwrap();

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
        let mut release = releases.create(oid, &alice.signer).unwrap();

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
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();
        release.attest(cid, &alice.signer).unwrap();

        // Reload and verify attestation is still present.
        release.reload().unwrap();
        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact.is_attested_by(&Did::from(alice.signer.public_key())));
        assert_eq!(artifact.attestations().len(), 1);
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
    fn set_metadata_basic() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        let value = serde_json::json!({"flake": true, "output": "packages.x86_64-linux.node"});
        release
            .set_metadata(
                cid,
                "xyz.example.nix-build".into(),
                value.clone(),
                &alice.signer,
            )
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_meta = artifact
            .metadata_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert_eq!(alice_meta.get("xyz.example.nix-build").unwrap(), &value);
    }

    #[test]
    fn set_metadata_overwrites() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        release
            .set_metadata(
                cid,
                "xyz.example.key".into(),
                serde_json::json!("first"),
                &alice.signer,
            )
            .unwrap();
        release
            .set_metadata(
                cid,
                "xyz.example.key".into(),
                serde_json::json!("second"),
                &alice.signer,
            )
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_meta = artifact
            .metadata_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert_eq!(
            alice_meta.get("xyz.example.key").unwrap(),
            &serde_json::json!("second")
        );
        assert_eq!(alice_meta.len(), 1);
    }

    #[test]
    fn remove_metadata() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // Set two keys, remove one.
        release
            .set_metadata(
                cid,
                "xyz.example.a".into(),
                serde_json::json!(1),
                &alice.signer,
            )
            .unwrap();
        release
            .set_metadata(
                cid,
                "xyz.example.b".into(),
                serde_json::json!(2),
                &alice.signer,
            )
            .unwrap();
        release
            .remove_metadata(cid, "xyz.example.a".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_meta = artifact
            .metadata_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert!(alice_meta.get("xyz.example.a").is_none());
        assert_eq!(alice_meta.get("xyz.example.b").unwrap(), &serde_json::json!(2));
    }

    #[test]
    fn remove_metadata_cleans_up_empty_did() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        release
            .set_metadata(
                cid,
                "xyz.example.key".into(),
                serde_json::json!(true),
                &alice.signer,
            )
            .unwrap();
        release
            .remove_metadata(cid, "xyz.example.key".into(), &alice.signer)
            .unwrap();

        // The DID entry itself should be cleaned up when the last key is removed.
        let artifact = release.artifact(&cid).unwrap();
        assert!(artifact
            .metadata_of(&Did::from(alice.signer.public_key()))
            .is_none());
        assert!(artifact.metadata().is_empty());
    }

    #[test]
    fn set_metadata_for_missing_cid_is_noop() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(99);
        release
            .set_metadata(
                cid,
                "xyz.example.key".into(),
                serde_json::json!(true),
                &alice.signer,
            )
            .unwrap();

        assert!(release.artifact(&cid).is_none());
    }

    #[test]
    fn metadata_persists_through_reload() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        let value = serde_json::json!({"reproducible": true});
        release
            .set_metadata(cid, "xyz.example.build".into(), value.clone(), &alice.signer)
            .unwrap();

        release.reload().unwrap();
        let artifact = release.artifact(&cid).unwrap();
        let alice_meta = artifact
            .metadata_of(&Did::from(alice.signer.public_key()))
            .unwrap();
        assert_eq!(alice_meta.get("xyz.example.build").unwrap(), &value);
    }

    #[test]
    fn multi_user_metadata() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
            .unwrap();

        // Both users set the same key with different values.
        release
            .set_metadata(
                cid,
                "xyz.example.verified".into(),
                serde_json::json!(true),
                &alice.signer,
            )
            .unwrap();
        release
            .set_metadata(
                cid,
                "xyz.example.verified".into(),
                serde_json::json!(false),
                &bob.signer,
            )
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();

        // Each user's value is independent.
        let alice_did = Did::from(alice.signer.public_key());
        let bob_did = Did::from(bob.signer.public_key());
        assert_eq!(
            artifact.metadata_of(&alice_did).unwrap().get("xyz.example.verified").unwrap(),
            &serde_json::json!(true)
        );
        assert_eq!(
            artifact.metadata_of(&bob_did).unwrap().get("xyz.example.verified").unwrap(),
            &serde_json::json!(false)
        );

        // all_metadata_for_key returns both entries.
        let entries = artifact.all_metadata_for_key("xyz.example.verified");
        assert_eq!(entries.len(), 2);
    }
}
