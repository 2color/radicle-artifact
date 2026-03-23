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

/// Maximum byte length for a redaction reason string.
pub const MAX_REDACT_REASON_LEN: usize = 2048;

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// The collaborative object actions for artifact releases.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// If the CID already exists, the name is updated.
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
                    if !artifact.redactions.contains_key(&user) {
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

    /// Redact an artifact with a reason.
    fn redact(&mut self, cid: Cid, reason: String) -> Result<(), store::Error> {
        self.0.push(Action::Redact { cid, reason })
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
        // Second add with different name updates it.
        release
            .add_artifact(cid, "second name".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.name(), "second name");
        assert_eq!(release.artifacts().len(), 1);
    }

    #[test]
    fn add_artifact_records_author() {
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

        let artifact = release.artifact(&cid).unwrap();
        assert_eq!(artifact.author(), &Did::from(alice.signer.public_key()));
    }

    #[test]
    fn non_author_cannot_rename_artifact() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "original name".into(), &alice.signer)
            .unwrap();

        // Bob tries to rename — should be ignored.
        release
            .add_artifact(cid, "bobs name".into(), &bob.signer)
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
    fn redact_artifact() {
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "test artifact".into(), &alice.signer)
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
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Attest then redact — redaction should supersede the attestation.
        release.attest(cid, &alice.signer).unwrap();
        release
            .redact(cid, "source was compromised".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_did = Did::from(alice.signer.public_key());
        assert!(!artifact.is_attested_by(&alice_did));
        assert!(artifact.is_redacted_by(&alice_did));
    }

    #[test]
    fn redact_then_attest_is_blocked() {
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

        // Redact first, then attempt to attest — the attestation should be
        // silently ignored because redactions are permanent and supersede.
        release
            .redact(cid, "suspected issue".into(), &alice.signer)
            .unwrap();
        release.attest(cid, &alice.signer).unwrap();

        let artifact = release.artifact(&cid).unwrap();
        let alice_did = Did::from(alice.signer.public_key());
        assert!(!artifact.is_attested_by(&alice_did));
        assert!(artifact.is_redacted_by(&alice_did));
    }

    #[test]
    fn redact_only_removes_own_attestation() {
        let test::setup::NodeWithRepo {
            node: alice, repo, ..
        } = test::setup::NodeWithRepo::default();
        let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();
        let oid = commit(&repo.backend, "Test Commit");
        let mut releases = Releases::open(&*repo).unwrap();
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
            .unwrap();

        // Both attest, then Alice redacts — only Alice's attestation is removed.
        release.attest(cid, &alice.signer).unwrap();
        release.attest(cid, &bob.signer).unwrap();
        release
            .redact(cid, "compromised".into(), &alice.signer)
            .unwrap();

        let artifact = release.artifact(&cid).unwrap();
        assert!(!artifact.is_attested_by(&Did::from(alice.signer.public_key())));
        assert!(artifact.is_attested_by(&Did::from(bob.signer.public_key())));
    }

    #[test]
    fn redact_empty_reason() {
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
        release
            .redact(cid, "".into(), &alice.signer)
            .unwrap();

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
        let mut release = releases.create(oid, &alice.signer).unwrap();

        let cid = test_cid(1);
        release
            .add_artifact(cid, "linux-amd64 binary".into(), &alice.signer)
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
        let mut release = releases.create(oid, &alice.signer).unwrap();

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
}
