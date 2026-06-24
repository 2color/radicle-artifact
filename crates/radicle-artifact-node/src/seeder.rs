//! Seeder primitives: persistent iroh-blobs store, per-repo tag management,
//! import + size helpers.
//!
//! This module holds the bytes-and-tags layer of artifact seeding. It knows
//! nothing about COBs or signing — callers compose `seed()` with the
//! appropriate `add_location` write themselves.
//!
//! Tags are scoped per repository and release: `seeded/{rid}/{release}/{cid}`.
//! The same CID seeded under two releases (or two repos) produces distinct
//! tags pointing at one underlying blob; unseeding one release does not
//! drop bytes another release still needs. GC marks from every tag, so the
//! blob survives until the last release referencing it is unseeded.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::iroh::EndpointConfig;
use crate::Error;
use cid::Cid;
use iroh::protocol::Router;
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode as IrohImportMode};
use iroh_blobs::api::{Store, TempTag};
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::{options::Options as FsStoreOptions, FsStore};
use iroh_blobs::store::GcConfig;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, HashAndFormat};
use n0_future::StreamExt;
use radicle::git::Oid;
use radicle::identity::RepoId;
use radicle_artifact_core::cid::{self as cid_utils, ArtifactKind};

pub use radicle_artifact_core::protocol::ImportMode;

/// Map the wire-protocol import mode onto the iroh-blobs one. A free
/// function because both types are foreign here (orphan rule).
fn to_iroh_import_mode(m: ImportMode) -> IrohImportMode {
    match m {
        ImportMode::Copy => IrohImportMode::Copy,
        ImportMode::Reference => IrohImportMode::TryReference,
    }
}

/// Directory name (under `<home>`) that holds the seeder's state.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Subdirectory of [`ARTIFACTS_DIR`] holding the FsStore.
pub const STORE_DIR: &str = "store";

/// First byte of every `seeded` tag — distinguishes them from any
/// future tag class we add. Binary, not UTF-8: see [`seeded_tag`].
const SEEDED_TAG_V1: u8 = 0x01;

/// Best-effort bound on waiting for a relay connection during bootstrap.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the store's background GC sweep runs.
///
/// GC marks every blob reachable from a tag (or live temp tag) and
/// sweeps the rest. The interval bounds how long an unseeded blob's
/// bytes linger on disk after its `seeded/...` tag is removed.
const GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A running iroh-blobs seeder: an `FsStore` plus the iroh `Router` that
/// accepts blob fetches against it.
pub struct Seeder {
    /// The persistent blob store.
    pub blobs: FsStore,
    /// The iroh protocol router serving blobs to peers.
    pub router: Router,
}

/// Bootstrap the iroh seeder.
///
/// Opens the persistent `FsStore` under `<home>/artifacts/store/`, binds
/// an iroh `Endpoint` to the supplied `SecretKey`, and spawns the
/// `BlobsProtocol` router. Anything previously tagged in the store is
/// reachable the moment this returns.
///
/// The caller supplies the iroh `SecretKey` — typically derived from the
/// user's radicle keystore via [`radicle_artifact_core::keys::radicle_secret_to_iroh`]. This
/// module never reads the keystore directly.
pub async fn bootstrap(home: &Path, secret: iroh::SecretKey) -> Result<Seeder, Error> {
    let dir = home.join(ARTIFACTS_DIR);
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;

    // FsStore::load defaults to gc: None — blobs would linger on disk
    // forever after untag_seeded. Enable the periodic mark-and-sweep
    // GC; our `seeded/{rid}/{release}/{cid}` tags are the live roots.
    let store_dir = dir.join(STORE_DIR);
    let db_path = store_dir.join("blobs.db");
    let mut options = FsStoreOptions::new(&store_dir);
    options.gc = Some(GcConfig {
        interval: GC_INTERVAL,
        add_protected: None,
    });
    let blobs = FsStore::load_with_opts(db_path, options)
        .await
        .map_err(|e| Error::Iroh(format!("FsStore load: {e}")))?;

    let preset = EndpointConfig::from_env()?;
    let endpoint = iroh::Endpoint::builder(preset)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|e| Error::Iroh(format!("endpoint bind: {e}")))?;

    // bind() only guarantees a local socket; until a relay is picked,
    // peers resolving our endpoint id can't reach us. Wait (best-effort,
    // bounded) so we don't add locations the network can't route to
    // yet. online() can block indefinitely when offline, so cap it.
    if tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online())
        .await
        .is_err()
    {
        tracing::warn!("endpoint not relay-connected after {ONLINE_TIMEOUT:?}; continuing anyway");
    }

    let blobs_protocol = BlobsProtocol::new(&blobs, None);
    let router = Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, blobs_protocol)
        .spawn();

    Ok(Seeder { blobs, router })
}

/// Binary tag key for a `(rid, release, cid)` triple.
///
/// Layout:
/// `[SEEDED_TAG_V1][rid_len: u8][rid_bytes][rel_len: u8][rel_bytes][Cid binary form]`.
/// Both ids are length-prefixed Oids, keeping the format hash-agnostic; a
/// SHA-256 id (32 bytes) slots in without a new sentinel byte. The CID's
/// self-describing binary form runs to the end.
///
/// The release sits in the key so the same CID seeded under two releases
/// yields two distinct tags over one blob: unseeding one leaves the other's
/// bytes protected from GC.
fn seeded_tag(rid: &RepoId, release: &Oid, cid: &Cid) -> Vec<u8> {
    let mut out = seeded_release_prefix(rid, release);
    out.extend_from_slice(&cid.to_bytes());
    out
}

/// Binary prefix matching every seeded tag for `rid`, across all releases.
fn seeded_rid_prefix(rid: &RepoId) -> Vec<u8> {
    let rid_b = rid_bytes(rid);
    let mut out = Vec::with_capacity(2 + rid_b.len());
    out.push(SEEDED_TAG_V1);
    out.push(rid_b.len() as u8);
    out.extend_from_slice(rid_b);
    out
}

/// Binary prefix matching every seeded tag for `(rid, release)`.
fn seeded_release_prefix(rid: &RepoId, release: &Oid) -> Vec<u8> {
    let mut out = seeded_rid_prefix(rid);
    let rel_b = AsRef::<[u8]>::as_ref(release);
    out.push(rel_b.len() as u8);
    out.extend_from_slice(rel_b);
    out
}

/// Raw bytes of the [`Oid`] backing a `RepoId` — 20 for SHA-1, 32 for
/// SHA-256 once radicle moves over.
fn rid_bytes(rid: &RepoId) -> &[u8] {
    AsRef::<[u8]>::as_ref(&**rid)
}

/// Reconstruct an [`Oid`] from its byte form, dispatching on length so
/// SHA-256 RIDs can join SHA-1 RIDs in the same store.
fn oid_from_bytes(b: &[u8]) -> Option<Oid> {
    match b.len() {
        20 => Some(Oid::from_sha1(b.try_into().ok()?)),
        // TODO(sha256): when radicle exposes a 32-byte Oid, dispatch here:
        // 32 => Some(Oid::from_sha256(b.try_into().ok()?)),
        _ => None,
    }
}

/// Inverse of [`seeded_tag`]: decode a tag name into `(RepoId, Oid, Cid)`.
fn parse_seeded_tag(name: &[u8]) -> Option<(RepoId, Oid, Cid)> {
    let rest = name.strip_prefix(&[SEEDED_TAG_V1])?;
    let (rid_b, rest) = take_len_prefixed(rest)?;
    let (rel_b, cid_b) = take_len_prefixed(rest)?;
    let rid = RepoId::from(oid_from_bytes(rid_b)?);
    let release = oid_from_bytes(rel_b)?;
    let cid = Cid::try_from(cid_b).ok()?;
    Some((rid, release, cid))
}

/// Split a `[len: u8][bytes]` field off the front, returning `(bytes, rest)`.
fn take_len_prefixed(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&len, rest) = buf.split_first()?;
    let len = usize::from(len);
    if rest.len() < len {
        return None;
    }
    Some(rest.split_at(len))
}

/// Build `AddPathOptions` for an absolute path under the requested mode.
fn add_opts(path: std::path::PathBuf, mode: ImportMode) -> AddPathOptions {
    AddPathOptions {
        path,
        format: BlobFormat::Raw,
        mode: to_iroh_import_mode(mode),
    }
}

/// Import a single file into the store and verify it matches the expected CID.
///
/// `mode` selects copy-vs-reference semantics — see [`ImportMode`].
///
/// Returns the blob `Hash` together with the [`TempTag`] that protects it.
/// The caller must keep the temp tag alive until a persistent tag covers
/// the blob, otherwise GC may sweep it. Prefer [`seed_artifact`], which
/// holds the temp tag across [`tag_seeded`].
pub async fn import_blob(
    store: &Store,
    path: &Path,
    expected: &Cid,
    mode: ImportMode,
) -> Result<(Hash, TempTag), Error> {
    // iroh-blobs requires an absolute path for in-place reference imports.
    let abs = dunce::canonicalize(path).map_err(|e| Error::Iroh(format!("canonicalize: {e}")))?;
    let tt = store
        .add_path_with_opts(add_opts(abs, mode))
        .temp_tag()
        .await
        .map_err(|e| Error::Iroh(format!("import blob: {e}")))?;
    let hash = tt.hash();

    let actual = cid_utils::blake3_hash_to_cid(hash.into(), ArtifactKind::Blob);
    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok((hash, tt))
}

/// Import a directory as a [`Collection`] and verify it matches the expected CID.
///
/// Each file becomes a collection entry keyed by its relative path; files
/// are imported in canonical (sorted) order for determinism.
///
/// Returns the root `Hash` and the [`TempTag`] that protects the
/// collection. Holding the root temp tag covers child blobs too — GC's
/// mark phase expands the hash-seq from any live root. The caller must
/// keep the temp tag alive until a persistent tag covers the root;
/// prefer [`seed_artifact`], which does this.
pub async fn import_collection(
    store: &Store,
    dir: &Path,
    expected: &Cid,
    mode: ImportMode,
) -> Result<(Hash, TempTag), Error> {
    let entries = cid_utils::canonical_walk(dir).map_err(Error::Io)?;

    let mut pairs: Vec<(String, Hash)> = Vec::new();
    // Hold per-file temp tags until the root tag is created — once the
    // root exists, GC mark expansion via hash-seq covers the children.
    let mut file_tags = Vec::with_capacity(entries.len());
    for (name, abs) in entries {
        let tt = store
            .add_path_with_opts(add_opts(abs, mode))
            .temp_tag()
            .await
            .map_err(|e| Error::Iroh(format!("import file {name}: {e}")))?;
        pairs.push((name, tt.hash()));
        file_tags.push(tt);
    }

    let collection = Collection::from_iter(pairs);
    let root_tag = collection
        .store(store)
        .await
        .map_err(|e| Error::Iroh(format!("store collection: {e}")))?;
    // Root temp tag now protects the whole hash-seq; drop per-file tags.
    drop(file_tags);

    let hash = root_tag.hash();
    let actual = cid_utils::blake3_hash_to_cid(hash.into(), ArtifactKind::Collection);
    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok((hash, root_tag))
}

/// Mark a `(rid, release, cid)` triple as actively seeded.
///
/// Sets the `seeded/{rid}/{release}/{cid}` tag pointing at `hash` with the
/// format matching the CID's kind. Idempotent; re-tagging with the same
/// hash is a no-op at the iroh-blobs layer.
pub async fn tag_seeded(
    store: &Store,
    rid: &RepoId,
    release: &Oid,
    cid: &Cid,
    hash: Hash,
) -> Result<(), Error> {
    let kind = cid_utils::artifact_kind(cid)?;
    let value = match kind {
        ArtifactKind::Blob => HashAndFormat::raw(hash),
        ArtifactKind::Collection => HashAndFormat::hash_seq(hash),
    };
    store
        .tags()
        .set(seeded_tag(rid, release, cid), value)
        .await
        .map_err(|e| Error::Iroh(format!("set seeded tag: {e}")))?;
    Ok(())
}

/// Import an artifact and register it as seeded in a single step.
///
/// Holds the import temp tag alive across [`tag_seeded`] so the
/// freshly imported bytes are never momentarily unprotected — if GC's
/// mark phase landed between the temp tag dropping and the persistent
/// tag being set, the blob would be swept.
pub async fn seed_artifact(
    store: &Store,
    rid: &RepoId,
    release: &Oid,
    cid: &Cid,
    path: &Path,
    kind: ArtifactKind,
    mode: ImportMode,
) -> Result<Hash, Error> {
    let (hash, _tt) = match kind {
        ArtifactKind::Blob => import_blob(store, path, cid, mode).await?,
        ArtifactKind::Collection => import_collection(store, path, cid, mode).await?,
    };
    tag_seeded(store, rid, release, cid, hash).await?;
    // _tt drops here, after the persistent seeded tag protects the bytes.
    Ok(hash)
}

/// Remove the `seeded/{rid}/{release}/{cid}` tag for one release.
///
/// Returns whether a tag was actually present and removed; the delete itself
/// reports the count, so callers learn this without a separate read that a
/// concurrent unseed could invalidate. Idempotent: deleting a tag that
/// doesn't exist returns `Ok(false)`. The underlying blob bytes are not
/// removed by this call; iroh-blobs' GC reclaims them on its next sweep once
/// no tags reference them. Bytes another release still tags survive.
pub async fn untag_seeded(
    store: &Store,
    rid: &RepoId,
    release: &Oid,
    cid: &Cid,
) -> Result<bool, Error> {
    let removed = store
        .tags()
        .delete(seeded_tag(rid, release, cid))
        .await
        .map_err(|e| Error::Iroh(format!("delete seeded tag: {e}")))?;
    Ok(removed > 0)
}

/// Remove the seeded tag for `cid` under every release of `rid`.
///
/// Fully stops seeding the CID in this repo regardless of how many releases
/// reference it. Returns the number of release tags removed.
pub async fn untag_all(store: &Store, rid: &RepoId, cid: &Cid) -> Result<usize, Error> {
    let prefix = seeded_rid_prefix(rid);
    let mut stream = store
        .tags()
        .list_prefix(&prefix)
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    // Collect matching tag names first; deleting while streaming the same
    // listing would mutate what we're iterating.
    let mut names = Vec::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        if let Some((_, _, tag_cid)) = parse_seeded_tag(info.name.as_ref()) {
            if &tag_cid == cid {
                names.push(info.name);
            }
        }
    }

    let removed = names.len();
    for name in names {
        store
            .tags()
            .delete(name)
            .await
            .map_err(|e| Error::Iroh(format!("delete seeded tag: {e}")))?;
    }
    Ok(removed)
}

/// Whether `(rid, release, cid)` is currently tagged as seeded.
pub async fn is_seeded(
    store: &Store,
    rid: &RepoId,
    release: &Oid,
    cid: &Cid,
) -> Result<bool, Error> {
    let info = store
        .tags()
        .get(seeded_tag(rid, release, cid))
        .await
        .map_err(|e| Error::Iroh(format!("get seeded tag: {e}")))?;
    Ok(info.is_some())
}

/// Whether `cid` is tagged as seeded under any release of `rid`.
pub async fn is_seeded_any(store: &Store, rid: &RepoId, cid: &Cid) -> Result<bool, Error> {
    Ok(seeded_cids(store, rid).await?.contains_key(cid))
}

/// Return every CID currently seeded under `rid`, mapped to its blob hash.
///
/// Walks the `[SEEDED_TAG_V1][rid_bytes]` prefix and collapses the
/// per-release tags: a CID seeded under several releases appears once, since
/// all those tags point at the same blob. The hash rides along so callers
/// can size each artifact without a second lookup. Decoding failures
/// (corrupt tag names, unlikely since we write them ourselves) are skipped.
pub async fn seeded_cids(store: &Store, rid: &RepoId) -> Result<HashMap<Cid, Hash>, Error> {
    let prefix = seeded_rid_prefix(rid);
    let mut stream = store
        .tags()
        .list_prefix(&prefix)
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    let mut out = HashMap::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        if let Some((_, _, cid)) = parse_seeded_tag(info.name.as_ref()) {
            out.insert(cid, info.hash);
        }
    }
    Ok(out)
}

/// Walk every seeded tag in the store, regardless of repo or release.
///
/// Yields each `(rid, release, cid, hash)` currently tagged as seeded. The
/// hash comes straight from the tag listing so callers can size the artifact
/// without a second tag lookup (see [`artifact_size_for`]). A blob shared
/// across releases appears once per release tag; callers that sum bytes
/// should dedup by hash. Tag names that don't parse cleanly are skipped; we
/// own the writer, so this only fires on corrupt stores.
pub async fn all_seeded(store: &Store) -> Result<Vec<(RepoId, Oid, Cid, Hash)>, Error> {
    let mut stream = store
        .tags()
        .list_prefix([SEEDED_TAG_V1])
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        if let Some((rid, release, cid)) = parse_seeded_tag(info.name.as_ref()) {
            out.push((rid, release, cid, info.hash));
        }
    }
    Ok(out)
}

/// Stored byte total for an already-resolved `(cid, hash)`.
///
/// Blobs report their own size; collections walk their hash sequence and
/// sum the children. Errors resolve to `0` so a `Status` view can render
/// the row even if iroh is momentarily unhappy. Callers hold the hash
/// already (from [`seeded_cids`] or [`all_seeded`]), so no tag lookup runs.
pub async fn artifact_size_for(store: &Store, cid: &Cid, hash: Hash) -> u64 {
    let Ok(kind) = cid_utils::artifact_kind(cid) else {
        return 0;
    };
    match kind {
        ArtifactKind::Blob => blob_size(store, hash).await,
        ArtifactKind::Collection => match Collection::load(hash, store).await {
            Ok(collection) => {
                let mut total = 0u64;
                for (_, child) in collection.iter() {
                    total = total.saturating_add(blob_size(store, *child).await);
                }
                total
            }
            Err(_) => 0,
        },
    }
}

async fn blob_size(store: &Store, hash: Hash) -> u64 {
    use iroh_blobs::api::proto::BlobStatus;
    match store.blobs().status(hash).await {
        Ok(BlobStatus::Complete { size }) => size,
        Ok(BlobStatus::Partial { size }) => size.unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::str::FromStr;

    use super::*;

    /// Build a fake raw-codec CID over `data`. The `tag_seeded`
    /// path requires a valid codec to pick `HashAndFormat::raw` vs
    /// `hash_seq`; the actual digest value doesn't have to match the
    /// blob hash for tag-layer tests.
    fn blob_cid(data: &[u8]) -> Cid {
        use cid::multihash::Multihash;
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(cid_utils::HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(cid_utils::RAW_CODEC, mh)
    }

    /// Two distinct RepoIds we can refer to in tests.
    fn rid_pair() -> (RepoId, RepoId) {
        // Both share the `rad:` prefix and a 20-byte Git-style Oid base58 body.
        let a = RepoId::from_str("rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip").unwrap();
        let b = RepoId::from_str("rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5").unwrap();
        assert_ne!(a, b);
        (a, b)
    }

    /// A `Oid` from a 40-hex-char (20-byte SHA-1) Oid.
    fn release(n: u8) -> Oid {
        Oid::from_str(&format!("{n:040x}")).unwrap()
    }

    /// Per-repo tag scoping: seeding the same CID in two repos creates two
    /// independent tags backed by one blob; unseeding in one repo leaves
    /// the other untouched.
    #[test]
    fn per_repo_tags_isolate() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();

            let (rid_a, rid_b) = rid_pair();
            let rel = release(1);
            let cid = blob_cid(b"shared bytes");
            // Real (rather than fabricated) iroh hash so iroh-blobs accepts
            // the tag value when we set it.
            let hash = Hash::new(b"shared bytes");

            tag_seeded(&store, &rid_a, &rel, &cid, hash).await.unwrap();
            tag_seeded(&store, &rid_b, &rel, &cid, hash).await.unwrap();

            assert!(is_seeded(&store, &rid_a, &rel, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid_b, &rel, &cid).await.unwrap());

            let cids_a = seeded_cids(&store, &rid_a).await.unwrap();
            let cids_b = seeded_cids(&store, &rid_b).await.unwrap();
            assert_eq!(cids_a.len(), 1);
            assert_eq!(cids_b.len(), 1);
            assert!(cids_a.contains_key(&cid));
            assert!(cids_b.contains_key(&cid));

            untag_seeded(&store, &rid_a, &rel, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid_a, &rel, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid_b, &rel, &cid).await.unwrap());

            // Per-rid enumeration stays clean across the removal.
            let cids_a = seeded_cids(&store, &rid_a).await.unwrap();
            let cids_b = seeded_cids(&store, &rid_b).await.unwrap();
            assert!(cids_a.is_empty());
            assert_eq!(cids_b.len(), 1);
        });
    }

    /// Per-release tag scoping: the same CID seeded under two releases of one
    /// repo creates two tags over one blob. Unseeding one release leaves the
    /// other's tag, the GC root that keeps the shared blob alive.
    #[test]
    fn per_release_tags_isolate() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();

            let (rid, _) = rid_pair();
            let (rel_a, rel_b) = (release(1), release(2));
            let cid = blob_cid(b"shared across releases");
            let hash = Hash::new(b"shared across releases");

            tag_seeded(&store, &rid, &rel_a, &cid, hash).await.unwrap();
            tag_seeded(&store, &rid, &rel_b, &cid, hash).await.unwrap();

            // Two distinct release tags, but the CID collapses to one entry.
            assert_eq!(all_seeded(&store).await.unwrap().len(), 2);
            assert_eq!(seeded_cids(&store, &rid).await.unwrap().len(), 1);

            // Unseeding one release keeps the other's tag alive.
            untag_seeded(&store, &rid, &rel_a, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid, &rel_a, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid, &rel_b, &cid).await.unwrap());
            assert!(is_seeded_any(&store, &rid, &cid).await.unwrap());

            // `untag_all` sweeps whatever release tags remain for the CID.
            tag_seeded(&store, &rid, &rel_a, &cid, hash).await.unwrap();
            assert_eq!(untag_all(&store, &rid, &cid).await.unwrap(), 2);
            assert!(!is_seeded_any(&store, &rid, &cid).await.unwrap());
        });
    }

    /// `untag_seeded` on an untagged triple is a no-op (idempotent).
    #[test]
    fn unregister_unknown_is_noop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let (rid_a, _) = rid_pair();
            let rel = release(1);
            let cid = blob_cid(b"never seeded");

            // No tag set; deleting it must still succeed.
            untag_seeded(&store, &rid_a, &rel, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid_a, &rel, &cid).await.unwrap());
        });
    }

    /// `all_seeded` returns every `(rid, release, cid)` tagged across all
    /// repos, exercising the `parse_seeded_tag` decode path that
    /// `seeded_cids` alone doesn't cover.
    #[test]
    fn all_seeded_round_trip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let (rid_a, rid_b) = rid_pair();
            let (rel_1, rel_2) = (release(1), release(2));
            let cid_x = blob_cid(b"x");
            let cid_y = blob_cid(b"y");
            let cid_z = blob_cid(b"z");
            let hash = Hash::new(b"value");

            let triples = [
                (rid_a, rel_1, cid_x),
                (rid_a, rel_2, cid_y),
                (rid_b, rel_1, cid_x),
                (rid_b, rel_1, cid_z),
            ];
            for (rid, rel, cid) in &triples {
                tag_seeded(&store, rid, rel, cid, hash).await.unwrap();
            }

            // The hash rides along from the tag listing, so callers can size
            // each artifact without a second lookup.
            let got: HashSet<(RepoId, Oid, Cid, Hash)> =
                all_seeded(&store).await.unwrap().into_iter().collect();
            let want: HashSet<(RepoId, Oid, Cid, Hash)> = triples
                .into_iter()
                .map(|(rid, rel, cid)| (rid, rel, cid, hash))
                .collect();
            assert_eq!(got, want);
        });
    }

    /// Lock the binary tag-name layout: sentinel byte, RID length+bytes,
    /// release length+bytes, then the CID's canonical binary form. Also
    /// exercise the encode/decode round-trip via `parse_seeded_tag`.
    #[test]
    fn seeded_tag_layout() {
        // Today's ids are SHA-1; the format itself doesn't bake that in,
        // which is the point of the length prefixes.
        const SHA1_LEN: usize = 20;

        let (rid, _) = rid_pair();
        let rel = release(7);
        let cid = blob_cid(b"layout");
        let tag = seeded_tag(&rid, &rel, &cid);

        let rid_off = 2;
        let rel_len_off = rid_off + SHA1_LEN;
        let rel_off = rel_len_off + 1;
        let cid_off = rel_off + SHA1_LEN;

        assert_eq!(tag.len(), cid_off + cid.to_bytes().len());
        assert_eq!(tag[0], SEEDED_TAG_V1);
        assert_eq!(usize::from(tag[1]), SHA1_LEN);
        assert_eq!(&tag[rid_off..rel_len_off], AsRef::<[u8]>::as_ref(&*rid));
        assert_eq!(usize::from(tag[rel_len_off]), SHA1_LEN);
        assert_eq!(&tag[rel_off..cid_off], AsRef::<[u8]>::as_ref(&rel));
        assert_eq!(Cid::try_from(&tag[cid_off..]).unwrap(), cid);

        let (rid_back, rel_back, cid_back) = parse_seeded_tag(&tag).expect("decodes");
        assert_eq!(rid_back, rid);
        assert_eq!(rel_back, rel);
        assert_eq!(cid_back, cid);
    }
}
