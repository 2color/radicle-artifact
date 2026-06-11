//! Seeder primitives: persistent iroh-blobs store, per-repo tag management,
//! import + size helpers.
//!
//! This module holds the bytes-and-tags layer of artifact seeding. It knows
//! nothing about COBs or signing — callers compose `seed()` with the
//! appropriate `add_location` write themselves.
//!
//! Tags are scoped per repository: `seeded/{rid}/{cid}`. The same CID
//! seeded in two repos produces two distinct tags pointing at one
//! underlying blob — unseeding in one repo does not affect the other.

use std::collections::HashSet;
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
    // GC; our `seeded/{rid}/{cid}` tags are the live roots.
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

/// Binary tag key for a `(rid, cid)` pair.
///
/// Layout: `[SEEDED_TAG_V1][rid_len: u8][rid_bytes][Cid binary form]`.
/// The length prefix keeps the format hash-agnostic — a SHA-256 RID
/// (32 bytes) would slot in without a new sentinel byte.
fn seeded_tag(rid: &RepoId, cid: &Cid) -> Vec<u8> {
    let rid_b = rid_bytes(rid);
    let cid_bytes = cid.to_bytes();
    let mut out = Vec::with_capacity(2 + rid_b.len() + cid_bytes.len());
    out.push(SEEDED_TAG_V1);
    out.push(rid_b.len() as u8);
    out.extend_from_slice(rid_b);
    out.extend_from_slice(&cid_bytes);
    out
}

/// Binary prefix matching every seeded tag for `rid`.
fn seeded_rid_prefix(rid: &RepoId) -> Vec<u8> {
    let rid_b = rid_bytes(rid);
    let mut out = Vec::with_capacity(2 + rid_b.len());
    out.push(SEEDED_TAG_V1);
    out.push(rid_b.len() as u8);
    out.extend_from_slice(rid_b);
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

/// Inverse of [`seeded_tag`]: decode a tag name back into `(RepoId, Cid)`.
fn parse_seeded_tag(name: &[u8]) -> Option<(RepoId, Cid)> {
    let rest = name.strip_prefix(&[SEEDED_TAG_V1])?;
    let (&len_byte, rest) = rest.split_first()?;
    let rid_len = usize::from(len_byte);
    if rest.len() < rid_len {
        return None;
    }
    let (rid_b, cid_b) = rest.split_at(rid_len);
    let rid = RepoId::from(oid_from_bytes(rid_b)?);
    let cid = Cid::try_from(cid_b).ok()?;
    Some((rid, cid))
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

/// Mark a `(rid, cid)` pair as actively seeded.
///
/// Sets the `seeded/{rid}/{cid}` tag pointing at `hash` with the format
/// matching the CID's kind. Idempotent — re-tagging with the same hash
/// is a no-op at the iroh-blobs layer.
pub async fn tag_seeded(store: &Store, rid: &RepoId, cid: &Cid, hash: Hash) -> Result<(), Error> {
    let kind = cid_utils::artifact_kind(cid)?;
    let value = match kind {
        ArtifactKind::Blob => HashAndFormat::raw(hash),
        ArtifactKind::Collection => HashAndFormat::hash_seq(hash),
    };
    store
        .tags()
        .set(seeded_tag(rid, cid), value)
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
    cid: &Cid,
    path: &Path,
    kind: ArtifactKind,
    mode: ImportMode,
) -> Result<Hash, Error> {
    let (hash, _tt) = match kind {
        ArtifactKind::Blob => import_blob(store, path, cid, mode).await?,
        ArtifactKind::Collection => import_collection(store, path, cid, mode).await?,
    };
    tag_seeded(store, rid, cid, hash).await?;
    // _tt drops here, after the persistent seeded tag protects the bytes.
    Ok(hash)
}

/// Remove the `seeded/{rid}/{cid}` tag.
///
/// Idempotent: deleting a tag that doesn't exist returns `Ok(())`. The
/// underlying blob bytes are not removed by this call — iroh-blobs' GC
/// reclaims them on its next sweep once no tags reference them.
pub async fn untag_seeded(store: &Store, rid: &RepoId, cid: &Cid) -> Result<(), Error> {
    store
        .tags()
        .delete(seeded_tag(rid, cid))
        .await
        .map_err(|e| Error::Iroh(format!("delete seeded tag: {e}")))?;
    Ok(())
}

/// Whether `(rid, cid)` is currently tagged as seeded.
pub async fn is_seeded(store: &Store, rid: &RepoId, cid: &Cid) -> Result<bool, Error> {
    let info = store
        .tags()
        .get(seeded_tag(rid, cid))
        .await
        .map_err(|e| Error::Iroh(format!("get seeded tag: {e}")))?;
    Ok(info.is_some())
}

/// Return every CID currently seeded under `rid`.
///
/// Walks the `[SEEDED_TAG_V1][rid_bytes]` prefix. Decoding failures
/// (corrupt tag names, unlikely since we write them ourselves) are
/// skipped.
pub async fn seeded_cids(store: &Store, rid: &RepoId) -> Result<HashSet<Cid>, Error> {
    let prefix = seeded_rid_prefix(rid);
    let mut stream = store
        .tags()
        .list_prefix(&prefix)
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    let mut out = HashSet::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        let Some(suffix) = info.name.as_ref().strip_prefix(prefix.as_slice()) else {
            continue;
        };
        if let Ok(cid) = Cid::try_from(suffix) {
            out.insert(cid);
        }
    }
    Ok(out)
}

/// Walk every seeded tag in the store, regardless of repo.
///
/// Yields each `(rid, cid, hash)` currently tagged as seeded. The hash
/// comes straight from the tag listing so callers can size the artifact
/// without a second tag lookup (see [`artifact_size_for`]). Tag names that
/// don't parse cleanly are skipped — we own the writer, so this only fires
/// on corrupt stores.
pub async fn all_seeded(store: &Store) -> Result<Vec<(RepoId, Cid, Hash)>, Error> {
    let mut stream = store
        .tags()
        .list_prefix([SEEDED_TAG_V1])
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        if let Some((rid, cid)) = parse_seeded_tag(info.name.as_ref()) {
            out.push((rid, cid, info.hash));
        }
    }
    Ok(out)
}

/// Sum of stored bytes for a single seeded `(rid, cid)` pair.
///
/// Blobs report their own size; collections walk their hash sequence and
/// sum the children. Errors resolve to `0` so a `Status` view can render
/// the row even if iroh is momentarily unhappy.
pub async fn artifact_size(store: &Store, rid: &RepoId, cid: &Cid) -> u64 {
    let Ok(Some(tag)) = store.tags().get(seeded_tag(rid, cid)).await else {
        return 0;
    };
    artifact_size_for(store, cid, tag.hash).await
}

/// Stored byte total for an already-resolved `(cid, hash)`, skipping the
/// seeded-tag lookup. Use when the hash is already in hand (e.g. from
/// [`all_seeded`]) to avoid a redundant `tags().get()`.
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
            let cid = blob_cid(b"shared bytes");
            // Real (rather than fabricated) iroh hash so iroh-blobs accepts
            // the tag value when we set it.
            let hash = Hash::new(b"shared bytes");

            tag_seeded(&store, &rid_a, &cid, hash).await.unwrap();
            tag_seeded(&store, &rid_b, &cid, hash).await.unwrap();

            assert!(is_seeded(&store, &rid_a, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid_b, &cid).await.unwrap());

            let cids_a = seeded_cids(&store, &rid_a).await.unwrap();
            let cids_b = seeded_cids(&store, &rid_b).await.unwrap();
            assert_eq!(cids_a.len(), 1);
            assert_eq!(cids_b.len(), 1);
            assert!(cids_a.contains(&cid));
            assert!(cids_b.contains(&cid));

            untag_seeded(&store, &rid_a, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid_a, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid_b, &cid).await.unwrap());

            // Per-rid enumeration stays clean across the removal.
            let cids_a = seeded_cids(&store, &rid_a).await.unwrap();
            let cids_b = seeded_cids(&store, &rid_b).await.unwrap();
            assert!(cids_a.is_empty());
            assert_eq!(cids_b.len(), 1);
        });
    }

    /// `untag_seeded` on an untagged pair is a no-op (idempotent).
    #[test]
    fn unregister_unknown_is_noop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let (rid_a, _) = rid_pair();
            let cid = blob_cid(b"never seeded");

            // No tag set; deleting it must still succeed.
            untag_seeded(&store, &rid_a, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid_a, &cid).await.unwrap());
        });
    }

    /// `all_seeded` returns every `(rid, cid)` tagged across all repos,
    /// exercising the `parse_seeded_tag` decode path that `seeded_cids`
    /// alone doesn't cover.
    #[test]
    fn all_seeded_round_trip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let (rid_a, rid_b) = rid_pair();
            let cid_x = blob_cid(b"x");
            let cid_y = blob_cid(b"y");
            let cid_z = blob_cid(b"z");
            let hash = Hash::new(b"value");

            let pairs = [
                (rid_a, cid_x),
                (rid_a, cid_y),
                (rid_b, cid_x),
                (rid_b, cid_z),
            ];
            for (rid, cid) in &pairs {
                tag_seeded(&store, rid, cid, hash).await.unwrap();
            }

            // The hash rides along from the tag listing, so callers can size
            // each artifact without a second lookup.
            let got: HashSet<(RepoId, Cid, Hash)> =
                all_seeded(&store).await.unwrap().into_iter().collect();
            let want: HashSet<(RepoId, Cid, Hash)> = pairs
                .into_iter()
                .map(|(rid, cid)| (rid, cid, hash))
                .collect();
            assert_eq!(got, want);
        });
    }

    /// Lock the binary tag-name layout: sentinel byte, RID length, RID
    /// bytes, then the CID's canonical binary form. Also exercise the
    /// encode/decode round-trip via `parse_seeded_tag`.
    #[test]
    fn seeded_tag_layout() {
        // Today's RIDs are SHA-1; the format itself doesn't bake that in,
        // which is the point of the length prefix.
        const SHA1_LEN: usize = 20;

        let (rid, _) = rid_pair();
        let cid = blob_cid(b"layout");
        let tag = seeded_tag(&rid, &cid);

        assert_eq!(tag.len(), 2 + SHA1_LEN + cid.to_bytes().len());
        assert_eq!(tag[0], SEEDED_TAG_V1);
        assert_eq!(usize::from(tag[1]), SHA1_LEN);
        assert_eq!(&tag[2..2 + SHA1_LEN], AsRef::<[u8]>::as_ref(&*rid));
        assert_eq!(Cid::try_from(&tag[2 + SHA1_LEN..]).unwrap(), cid);

        let (rid_back, cid_back) = parse_seeded_tag(&tag).expect("decodes");
        assert_eq!(rid_back, rid);
        assert_eq!(cid_back, cid);
    }
}
