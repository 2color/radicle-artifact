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
use std::str::FromStr;
use std::time::Duration;

use cid::Cid;
use iroh::protocol::Router;
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode as IrohImportMode};
use iroh_blobs::api::Store;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, HashAndFormat};
use n0_future::StreamExt;
use radicle::identity::RepoId;
use serde::{Deserialize, Serialize};

use crate::share::cid_utils::{self, ArtifactKind};
use crate::share::iroh::EndpointConfig;
use crate::share::Error;

/// How imported bytes are placed in the store.
///
/// Wraps [`iroh_blobs::api::blobs::ImportMode`] with a serde-friendly,
/// project-stable representation suitable for the wire protocol.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportMode {
    /// Copy bytes into the store. The source file can be moved or
    /// deleted afterwards without breaking seeding. Default for the
    /// node.
    Copy,
    /// Reference the source file in place. No bytes are copied. The
    /// caller is responsible for keeping the source path stable for as
    /// long as they want to seed the artifact; if the file is moved or
    /// deleted, fetches will fail.
    Reference,
}

impl From<ImportMode> for IrohImportMode {
    fn from(m: ImportMode) -> Self {
        match m {
            ImportMode::Copy => IrohImportMode::Copy,
            ImportMode::Reference => IrohImportMode::TryReference,
        }
    }
}

/// Directory name (under `<home>`) that holds the seeder's state.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Subdirectory of [`ARTIFACTS_DIR`] holding the FsStore.
pub const STORE_DIR: &str = "store";

/// Tag prefix marking a CID/repo pair as actively seeded.
const SEEDED_PREFIX: &str = "seeded/";

/// Best-effort bound on waiting for a relay connection during bootstrap.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

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
/// user's radicle keystore via [`crate::share::keys::radicle_secret_to_iroh`]. This
/// module never reads the keystore directly.
pub async fn bootstrap(home: &Path, secret: iroh::SecretKey) -> Result<Seeder, Error> {
    let dir = home.join(ARTIFACTS_DIR);
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;

    let blobs = FsStore::load(dir.join(STORE_DIR))
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
    // bounded) so we don't announce locations the network can't route to
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

/// Tag key for a `(rid, cid)` pair: `seeded/{rid}/{cid}`.
fn seeded_tag(rid: &RepoId, cid: &Cid) -> String {
    format!("{SEEDED_PREFIX}{rid}/{cid}")
}

/// Build `AddPathOptions` for an absolute path under the requested mode.
fn add_opts(path: std::path::PathBuf, mode: ImportMode) -> AddPathOptions {
    AddPathOptions {
        path,
        format: BlobFormat::Raw,
        mode: mode.into(),
    }
}

/// Import a single file into the store and verify it matches the expected CID.
///
/// `mode` selects copy-vs-reference semantics — see [`ImportMode`].
pub async fn import_blob(
    store: &Store,
    path: &Path,
    expected: &Cid,
    mode: ImportMode,
) -> Result<Hash, Error> {
    // iroh-blobs requires an absolute path for in-place reference imports.
    let abs = dunce::canonicalize(path).map_err(|e| Error::Iroh(format!("canonicalize: {e}")))?;
    let tag = store
        .add_path_with_opts(add_opts(abs, mode))
        .with_tag()
        .await
        .map_err(|e| Error::Iroh(format!("import blob: {e}")))?;

    let actual = cid_utils::blake3_hash_to_cid(tag.hash, ArtifactKind::Blob);
    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(tag.hash)
}

/// Import a directory as a [`Collection`] and verify it matches the expected CID.
///
/// Each file becomes a collection entry keyed by its relative path; files
/// are imported in canonical (sorted) order for determinism.
pub async fn import_collection(
    store: &Store,
    dir: &Path,
    expected: &Cid,
    mode: ImportMode,
) -> Result<Hash, Error> {
    let entries = cid_utils::canonical_walk(dir).map_err(Error::Io)?;

    let mut pairs: Vec<(String, Hash)> = Vec::new();
    for (name, abs) in entries {
        let tag = store
            .add_path_with_opts(add_opts(abs, mode))
            .with_tag()
            .await
            .map_err(|e| Error::Iroh(format!("import file {name}: {e}")))?;
        pairs.push((name, tag.hash));
    }

    let collection = Collection::from_iter(pairs);
    let root_tag = collection
        .store(store)
        .await
        .map_err(|e| Error::Iroh(format!("store collection: {e}")))?;

    let actual = cid_utils::blake3_hash_to_cid(root_tag.hash(), ArtifactKind::Collection);
    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(root_tag.hash())
}

/// Mark a `(rid, cid)` pair as actively seeded.
///
/// Sets the `seeded/{rid}/{cid}` tag pointing at `hash` with the format
/// matching the CID's kind. Idempotent — re-tagging with the same hash
/// is a no-op at the iroh-blobs layer.
pub async fn register_seeded(
    store: &Store,
    rid: &RepoId,
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
        .set(seeded_tag(rid, cid).as_bytes(), value)
        .await
        .map_err(|e| Error::Iroh(format!("set seeded tag: {e}")))?;
    Ok(())
}

/// Remove the `seeded/{rid}/{cid}` tag.
///
/// Idempotent: deleting a tag that doesn't exist returns `Ok(())`. The
/// underlying blob bytes are not removed by this call — iroh-blobs' GC
/// reclaims them once no tags reference them.
pub async fn unregister_seeded(store: &Store, rid: &RepoId, cid: &Cid) -> Result<(), Error> {
    store
        .tags()
        .delete(seeded_tag(rid, cid).as_bytes())
        .await
        .map_err(|e| Error::Iroh(format!("delete seeded tag: {e}")))?;
    Ok(())
}

/// Whether `(rid, cid)` is currently tagged as seeded.
pub async fn is_seeded(store: &Store, rid: &RepoId, cid: &Cid) -> Result<bool, Error> {
    let info = store
        .tags()
        .get(seeded_tag(rid, cid).as_bytes())
        .await
        .map_err(|e| Error::Iroh(format!("get seeded tag: {e}")))?;
    Ok(info.is_some())
}

/// Return every CID currently seeded under `rid`.
///
/// Walks the `seeded/{rid}/` tag prefix. Decoding failures (corrupt tag
/// names, unlikely since we write them ourselves) are skipped.
pub async fn seeded_cids(store: &Store, rid: &RepoId) -> Result<HashSet<Cid>, Error> {
    let prefix = format!("{SEEDED_PREFIX}{rid}/");
    let mut stream = store
        .tags()
        .list_prefix(prefix.as_bytes())
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    let mut out = HashSet::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        let name = String::from_utf8_lossy(info.name.as_ref());
        if let Some(suffix) = name.strip_prefix(&prefix) {
            if let Ok(cid) = Cid::from_str(suffix) {
                out.insert(cid);
            }
        }
    }
    Ok(out)
}

/// Walk every `seeded/...` tag in the store, regardless of repo.
///
/// Yields each `(rid, cid)` pair currently tagged as seeded. Tag names
/// that don't parse cleanly are skipped — we own the writer, so this
/// only fires on corrupt stores.
pub async fn all_seeded(store: &Store) -> Result<Vec<(RepoId, Cid)>, Error> {
    let mut stream = store
        .tags()
        .list_prefix(SEEDED_PREFIX.as_bytes())
        .await
        .map_err(|e| Error::Iroh(format!("list seeded tags: {e}")))?;

    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| Error::Iroh(format!("seeded tag stream: {e}")))?;
        let name = String::from_utf8_lossy(info.name.as_ref());
        let Some(rest) = name.strip_prefix(SEEDED_PREFIX) else {
            continue;
        };
        let Some((rid_s, cid_s)) = rest.split_once('/') else {
            continue;
        };
        let (Ok(rid), Ok(cid)) = (RepoId::from_str(rid_s), Cid::from_str(cid_s)) else {
            continue;
        };
        out.push((rid, cid));
    }
    Ok(out)
}

/// Sum of stored bytes for a single seeded `(rid, cid)` pair.
///
/// Blobs report their own size; collections walk their hash sequence and
/// sum the children. Errors resolve to `0` so a `Status` view can render
/// the row even if iroh is momentarily unhappy.
pub async fn artifact_size(store: &Store, rid: &RepoId, cid: &Cid) -> u64 {
    let Ok(kind) = cid_utils::artifact_kind(cid) else {
        return 0;
    };
    let Ok(Some(tag)) = store.tags().get(seeded_tag(rid, cid).as_bytes()).await else {
        return 0;
    };
    match kind {
        ArtifactKind::Blob => blob_size(store, tag.hash).await,
        ArtifactKind::Collection => match Collection::load(tag.hash, store).await {
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

    /// Build a fake raw-codec CID over `data`. The `register_seeded`
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

            register_seeded(&store, &rid_a, &cid, hash).await.unwrap();
            register_seeded(&store, &rid_b, &cid, hash).await.unwrap();

            assert!(is_seeded(&store, &rid_a, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid_b, &cid).await.unwrap());

            let cids_a = seeded_cids(&store, &rid_a).await.unwrap();
            let cids_b = seeded_cids(&store, &rid_b).await.unwrap();
            assert_eq!(cids_a.len(), 1);
            assert_eq!(cids_b.len(), 1);
            assert!(cids_a.contains(&cid));
            assert!(cids_b.contains(&cid));

            unregister_seeded(&store, &rid_a, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid_a, &cid).await.unwrap());
            assert!(is_seeded(&store, &rid_b, &cid).await.unwrap());

            // Per-rid enumeration stays clean across the removal.
            let cids_a = seeded_cids(&store, &rid_a).await.unwrap();
            let cids_b = seeded_cids(&store, &rid_b).await.unwrap();
            assert!(cids_a.is_empty());
            assert_eq!(cids_b.len(), 1);
        });
    }

    /// `unregister_seeded` on an untagged pair is a no-op (idempotent).
    #[test]
    fn unregister_unknown_is_noop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = FsStore::load(tmp.path()).await.unwrap();
            let (rid_a, _) = rid_pair();
            let cid = blob_cid(b"never seeded");

            // No tag set; deleting it must still succeed.
            unregister_seeded(&store, &rid_a, &cid).await.unwrap();
            assert!(!is_seeded(&store, &rid_a, &cid).await.unwrap());
        });
    }
}
