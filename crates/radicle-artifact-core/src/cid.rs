//! CID (Content Identifier) utilities for artifact content addressing.
//!
//! Provides conversions between BLAKE3 hashes and CIDs, deterministic
//! content ID computation for directories, and CID verification.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use cid::multihash::Multihash;
use cid::Cid as InnerCid;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// Content identifier for an artifact: the project-wide CID type.
///
/// A newtype around the `cid` crate's [`cid::Cid`], mirroring [`EndpointId`]:
/// it is the type used across the codebase (COB actions, wire protocol,
/// public APIs, maps), and the inner `cid::Cid` is reached only at the
/// iroh-blobs hash boundary via [`Cid::as_inner`] / [`Cid::into_inner`].
///
/// The wrapper exists for serde. The `cid` crate's derived [`Serialize`]
/// encodes a CID as raw bytes, which renders as an unreadable JSON byte
/// array; a per-field `#[serde(with = …)]` adapter could fix that but is
/// opt-in and fails silently when a field is forgotten. Owning the
/// [`Serialize`]/[`Deserialize`] impls here makes the canonical multibase
/// string (`"bafy…"`) the only encoding, enforced by the type rather than
/// the author. `Display` and `FromStr` already match that string form.
///
/// [`EndpointId`]: crate::keys::EndpointId
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cid(InnerCid);

impl Cid {
    /// Borrow the underlying `cid::Cid` (for the iroh-blobs hash boundary).
    pub fn as_inner(&self) -> &InnerCid {
        &self.0
    }

    /// Consume into the underlying `cid::Cid`.
    pub fn into_inner(self) -> InnerCid {
        self.0
    }
}

impl From<InnerCid> for Cid {
    fn from(cid: InnerCid) -> Self {
        Self(cid)
    }
}

impl From<Cid> for InnerCid {
    fn from(cid: Cid) -> Self {
        cid.0
    }
}

impl fmt::Display for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Cid {
    type Err = <InnerCid as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        InnerCid::from_str(s).map(Self)
    }
}

impl Serialize for Cid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Cid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        InnerCid::from_str(&s).map(Self).map_err(de::Error::custom)
    }
}

/// BLAKE3 multihash code.
///
/// Source: <https://github.com/multiformats/multicodec/blob/master/table.csv#L51>
pub const HASH_CODE_BLAKE3: u64 = 0x1e;

/// `blake3-hashseq` codec for iroh collections (a sequence of BLAKE3 hashes).
pub const BLAKE3_HASHSEQ_CODEC: u64 = 0x80;

/// Raw binary codec for single blobs.
pub const RAW_CODEC: u64 = 0x55;

/// Whether the CID represents a single blob or a collection of named blobs.
pub fn artifact_kind(cid: &Cid) -> Result<ArtifactKind, Error> {
    match cid.0.codec() {
        RAW_CODEC => Ok(ArtifactKind::Blob),
        BLAKE3_HASHSEQ_CODEC => Ok(ArtifactKind::Collection),
        other => Err(Error::Cid(format!("unsupported CID codec: 0x{other:x}"))),
    }
}

/// The kind of artifact a CID points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// A single blob (raw codec 0x55).
    Blob,
    /// A named collection of blobs (blake3-hashseq codec 0x80).
    Collection,
}

/// Create a CID from a BLAKE3 hash and artifact kind.
///
/// This is the inverse of [`cid_to_blake3_hash`]: given a BLAKE3 hash and the
/// appropriate codec, it produces a CIDv1.
pub fn blake3_hash_to_cid(hash: blake3::Hash, kind: ArtifactKind) -> Cid {
    let codec = match kind {
        ArtifactKind::Blob => RAW_CODEC,
        ArtifactKind::Collection => BLAKE3_HASHSEQ_CODEC,
    };
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, hash.as_bytes())
        .expect("BLAKE3 digest is always 32 bytes");
    Cid(InnerCid::new_v1(codec, mh))
}

/// Extract the BLAKE3 digest from a CID's multihash.
///
/// Works with any CID codec as long as the multihash uses BLAKE3 (0x1e).
pub fn cid_to_blake3_hash(cid: &Cid) -> Result<blake3::Hash, Error> {
    let mh = cid.0.hash();
    if mh.code() != HASH_CODE_BLAKE3 {
        return Err(Error::Cid(format!(
            "expected BLAKE3 multihash (0x1e), got 0x{:x}",
            mh.code()
        )));
    }
    let digest: [u8; 32] = mh.digest().try_into().map_err(|_| {
        Error::Cid(format!(
            "expected 32-byte BLAKE3 digest, got {} bytes",
            mh.digest().len()
        ))
    })?;
    Ok(blake3::Hash::from_bytes(digest))
}

/// Compute the CID of a file on disk.
///
/// Streams the file through a BLAKE3 hasher to avoid loading it into memory.
pub fn compute_blob_cid(path: &std::path::Path) -> Result<Cid, Error> {
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher).map_err(Error::Io)?;
    Ok(blake3_hash_to_cid(hasher.finalize(), ArtifactKind::Blob))
}

/// Verify that a file on disk matches the expected CID.
///
/// Streams the file through a BLAKE3 hasher to avoid loading it into memory.
pub fn verify_cid_file(path: &std::path::Path, expected: &Cid) -> Result<(), Error> {
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher).map_err(Error::Io)?;
    let actual = blake3_hash_to_cid(hasher.finalize(), ArtifactKind::Blob);

    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// Walk a directory and return sorted (relative_name, absolute_path) pairs.
///
/// Skips symlinks, normalizes path separators to `/`, and sorts by name
/// for deterministic ordering. This is the canonical walk used by
/// [`compute_content_id`].
pub fn canonical_walk(dir: &Path) -> Result<Vec<(String, PathBuf)>, io::Error> {
    let root_dir = dunce::canonicalize(dir)?;
    let mut entries = Vec::new();

    for entry in walkdir::WalkDir::new(&root_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let abs = dunce::canonicalize(entry.path())?;
        let rel = abs.strip_prefix(&root_dir).map_err(io::Error::other)?;

        // Normalize path separators to forward slashes for cross-platform consistency
        let name = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");

        entries.push((name, abs));
    }

    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(entries)
}

/// Wire form of the iroh-blobs `CollectionMeta` blob, reproduced here so
/// directory CIDs can be computed without the iroh-blobs dependency. The
/// node crate carries a cross-check test against the real
/// `iroh_blobs::format::collection::Collection` encoding, so any upstream
/// format drift fails CI rather than silently forking the CID space.
#[derive(Serialize)]
struct CollectionMeta {
    header: [u8; 13],
    names: Vec<String>,
}

/// Header of the collection metadata blob (iroh-blobs `CollectionV0`).
const COLLECTION_HEADER: &[u8; 13] = b"CollectionV0.";

/// Build the collection root blob (the HashSeq) for sorted
/// `(name, file_hash)` entries: the postcard-encoded meta blob's hash
/// followed by each file hash, concatenated.
fn collection_root_blob(entries: &[(String, blake3::Hash)]) -> Vec<u8> {
    let meta = CollectionMeta {
        header: *COLLECTION_HEADER,
        names: entries.iter().map(|(name, _)| name.clone()).collect(),
    };
    let meta_bytes = postcard::to_stdvec(&meta).expect("collection meta always encodes");
    let meta_hash = blake3::hash(&meta_bytes);

    let mut root = Vec::with_capacity(32 * (entries.len() + 1));
    root.extend_from_slice(meta_hash.as_bytes());
    for (_, hash) in entries {
        root.extend_from_slice(hash.as_bytes());
    }
    root
}

/// Compute a content ID for a directory using the iroh-blobs Collection format.
///
/// Each file is individually BLAKE3-hashed, then the collection root blob
/// (HashSeq) is constructed from (relative_path, hash) pairs in sorted
/// order. The root blob's hash serves as the deterministic content ID.
///
/// The resulting CID uses the `blake3-hashseq` codec (0x80).
pub fn compute_content_id(dir: &Path) -> Result<Cid, io::Error> {
    let entries: Vec<(String, blake3::Hash)> = canonical_walk(dir)?
        .into_iter()
        .map(|(name, path)| {
            let file = std::fs::File::open(&path)?;
            let mut reader = io::BufReader::new(file);
            let mut hasher = blake3::Hasher::new();
            io::copy(&mut reader, &mut hasher)?;
            Ok((name, hasher.finalize()))
        })
        .collect::<Result<_, io::Error>>()?;

    let hash = blake3::hash(&collection_root_blob(&entries));
    Ok(blake3_hash_to_cid(hash, ArtifactKind::Collection))
}

/// Total logical size of the artifact at `path`: file length for a blob,
/// sum of member file lengths for a directory. Walks the same canonical set
/// as [`compute_content_id`], so it matches the seeder's logical size (the
/// sum of child blob sizes, without hashseq overhead).
pub fn compute_size_from_path(path: &Path) -> Result<u64, io::Error> {
    if path.is_dir() {
        canonical_walk(path)?
            .into_iter()
            .try_fold(0u64, |sum, (_, p)| {
                Ok(sum.saturating_add(std::fs::metadata(&p)?.len()))
            })
    } else {
        Ok(std::fs::metadata(path)?.len())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn create_test_dir(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for (path, contents) in files {
            let file_path = dir.path().join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&file_path, contents).unwrap();
        }
        dir
    }

    // -- Cid encoding tests --

    #[test]
    fn cid_serializes_as_string() {
        let cid = blob_cid(b"cid-encoding");

        // Encodes as the multibase string, not a JSON byte array.
        let value = serde_json::to_value(cid).unwrap();
        assert_eq!(value, serde_json::Value::String(cid.to_string()));

        // And round-trips back to the same CID.
        let back: Cid = serde_json::from_value(value).unwrap();
        assert_eq!(back, cid);
    }

    // -- CID conversion tests --

    fn blob_cid(data: &[u8]) -> Cid {
        blake3_hash_to_cid(blake3::hash(data), ArtifactKind::Blob)
    }

    fn collection_cid(data: &[u8]) -> Cid {
        blake3_hash_to_cid(blake3::hash(data), ArtifactKind::Collection)
    }

    #[test]
    fn cid_to_blake3_hash_roundtrip() {
        let data = b"test data";
        let expected_hash = blake3::hash(data);
        let cid = blob_cid(data);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, expected_hash);
    }

    #[test]
    fn cid_to_blake3_hash_rejects_sha256() {
        let digest = [0u8; 32];
        let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
        let cid = Cid(InnerCid::new_v1(RAW_CODEC, mh));
        assert!(matches!(cid_to_blake3_hash(&cid), Err(Error::Cid(_))));
    }

    #[test]
    fn artifact_kind_blob() {
        let cid = blob_cid(b"test");
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Blob);
    }

    #[test]
    fn artifact_kind_collection() {
        let cid = collection_cid(b"test");
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Collection);
    }

    #[test]
    fn artifact_kind_unknown_codec() {
        let digest = blake3::hash(b"test");
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        let cid = Cid(InnerCid::new_v1(0x99, mh));
        assert!(matches!(artifact_kind(&cid), Err(Error::Cid(_))));
    }

    #[test]
    fn cid_to_blake3_works_with_hashseq_codec() {
        let data = b"test data";
        let expected_hash = blake3::hash(data);
        let cid = collection_cid(data);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, expected_hash);
    }

    #[test]
    fn blake3_hash_to_cid_blob_roundtrip() {
        let data = b"test data";
        let hash = blake3::hash(data);
        let cid = blake3_hash_to_cid(hash, ArtifactKind::Blob);
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Blob);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, hash);
    }

    #[test]
    fn blake3_hash_to_cid_collection_roundtrip() {
        let data = b"test data";
        let hash = blake3::hash(data);
        let cid = blake3_hash_to_cid(hash, ArtifactKind::Collection);
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Collection);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, hash);
    }

    // -- canonical_walk tests --

    #[test]
    fn canonical_walk_returns_sorted_entries() {
        let dir = create_test_dir(&[("c.txt", b"c"), ("a.txt", b"a"), ("b.txt", b"b")]);
        let entries = canonical_walk(dir.path()).unwrap();

        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn canonical_walk_normalizes_separators() {
        let dir = create_test_dir(&[("sub/deep/file.txt", b"data")]);
        let entries = canonical_walk(dir.path()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "sub/deep/file.txt");
    }

    #[test]
    fn canonical_walk_skips_directories() {
        let dir = create_test_dir(&[("a/file.txt", b"data")]);
        let entries = canonical_walk(dir.path()).unwrap();

        assert_eq!(entries.len(), 1);
        assert!(entries[0].1.is_file());
    }

    #[test]
    fn canonical_walk_returns_absolute_paths() {
        let dir = create_test_dir(&[("file.txt", b"data")]);
        let entries = canonical_walk(dir.path()).unwrap();

        assert!(entries[0].1.is_absolute());
    }

    // -- compute_content_id tests --

    #[test]
    fn determinism() {
        let dir1 = create_test_dir(&[("a.txt", b"alpha"), ("b.txt", b"beta"), ("c.txt", b"gamma")]);
        let dir2 = create_test_dir(&[("c.txt", b"gamma"), ("a.txt", b"alpha"), ("b.txt", b"beta")]);

        let hash1 = compute_content_id(dir1.path()).unwrap();
        let hash2 = compute_content_id(dir2.path()).unwrap();
        assert_eq!(
            hash1, hash2,
            "same files in different creation order should produce the same hash"
        );
    }

    /// Golden value: computed once and hardcoded to catch regressions in the
    /// hashing algorithm, Collection format, or serialization.
    #[test]
    fn golden_hash() {
        let expected = "bagaachraxw5bpahcjbvb23lan2bmgueuuidupwxy6zhmibu7g4672o7snypa";

        let dir = create_test_dir(&[
            ("hello.txt", b"hello"),
            ("sub/world.txt", b"world"),
            ("file with spaces.txt", b"spaces matter"),
            (".hidden", b"hidden file"),
            ("empty.txt", b""),
            ("archive.tar.gz", b"multi-extension"),
            ("a.txt", b"a-root"),
            ("a/b.txt", b"a-subdir"),
            ("sub2/other.txt", b"sibling dir"),
            ("deep/nested/path/file.txt", b"deeply nested"),
        ]);
        let actual = compute_content_id(dir.path()).unwrap();
        assert_eq!(actual.to_string(), expected);
    }

    #[test]
    fn symlink_is_skipped() {
        let dir = create_test_dir(&[("real.txt", b"data")]);

        let link_path = dir.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("real.txt"), &link_path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(dir.path().join("real.txt"), &link_path).unwrap();

        let hash_with_symlink = compute_content_id(dir.path()).unwrap();

        let dir2 = create_test_dir(&[("real.txt", b"data")]);
        let hash_without = compute_content_id(dir2.path()).unwrap();
        assert_eq!(
            hash_with_symlink, hash_without,
            "symlinks should be skipped, not hashed"
        );
    }

    // -- compute_size_from_path tests --

    #[test]
    fn size_of_file_is_its_length() {
        let dir = create_test_dir(&[("a.bin", b"hello world")]);
        let size = compute_size_from_path(&dir.path().join("a.bin")).unwrap();
        assert_eq!(size, 11);
    }

    #[test]
    fn size_of_dir_sums_member_files() {
        // Sum of member file lengths, ignoring directory structure.
        let dir = create_test_dir(&[("a.txt", b"abc"), ("sub/b.txt", b"de"), ("sub/c.txt", b"")]);
        let size = compute_size_from_path(dir.path()).unwrap();
        assert_eq!(size, 5);
    }
}
