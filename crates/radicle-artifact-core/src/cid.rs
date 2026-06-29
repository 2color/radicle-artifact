//! CID (Content Identifier) utilities for artifact content addressing.
//!
//! Provides conversions between BLAKE3 hashes and CIDs, deterministic
//! content ID computation for directories, and CID verification.

use std::io;
use std::path::{Path, PathBuf};

use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use cid::multihash::Multihash;
use cid::Cid;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// A [`Cid`] that always serializes as its canonical multibase string.
///
/// The `cid` crate's derived [`Serialize`] encodes a CID as a newtype-struct
/// of raw bytes, which renders as an unreadable JSON byte array. Persisted
/// COB actions and the wire protocol need the `"bafy…"` string form instead.
///
/// A per-field `#[serde(with = …)]` annotation could supply that, but it is
/// opt-in and fails silently: a single forgotten field writes bytes and
/// forks the on-disk encoding. Wrapping the CID moves the guarantee into the
/// type, so every field of this type encodes as a string and the compiler,
/// not the author, enforces it.
///
/// Derefs to the inner [`Cid`], so read-only CID methods work unchanged;
/// convert with [`From`] in either direction at construction boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactCid(pub Cid);

impl From<Cid> for ArtifactCid {
    fn from(cid: Cid) -> Self {
        Self(cid)
    }
}

impl From<ArtifactCid> for Cid {
    fn from(cid: ArtifactCid) -> Self {
        cid.0
    }
}

impl Deref for ArtifactCid {
    type Target = Cid;

    fn deref(&self) -> &Cid {
        &self.0
    }
}

impl fmt::Display for ArtifactCid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ArtifactCid {
    type Err = <Cid as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cid::from_str(s).map(Self)
    }
}

impl Serialize for ArtifactCid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ArtifactCid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Cid::from_str(&s).map(Self).map_err(de::Error::custom)
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
    match cid.codec() {
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
    Cid::new_v1(codec, mh)
}

/// Extract the BLAKE3 digest from a CID's multihash.
///
/// Works with any CID codec as long as the multihash uses BLAKE3 (0x1e).
pub fn cid_to_blake3_hash(cid: &Cid) -> Result<blake3::Hash, Error> {
    let mh = cid.hash();
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

    // -- ArtifactCid tests --

    #[test]
    fn artifact_cid_serializes_as_string() {
        let cid = blob_cid(b"artifact-cid");
        let wrapped = ArtifactCid(cid);

        // Encodes as the multibase string, not a JSON byte array.
        let value = serde_json::to_value(wrapped).unwrap();
        assert_eq!(value, serde_json::Value::String(cid.to_string()));

        // And round-trips back to the same CID.
        let back: ArtifactCid = serde_json::from_value(value).unwrap();
        assert_eq!(back, wrapped);
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
        let cid = Cid::new_v1(RAW_CODEC, mh);
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
        let cid = Cid::new_v1(0x99, mh);
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
}
