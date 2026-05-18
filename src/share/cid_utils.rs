//! CID (Content Identifier) utilities for artifact content addressing.
//!
//! Provides conversions between iroh-blobs hashes and CIDs, deterministic
//! content ID computation for directories, and CID verification.

use std::io;
use std::path::{Path, PathBuf};

use cid::multihash::Multihash;
use cid::Cid;

use super::Error;

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

/// Create a CID from an iroh-blobs hash and artifact kind.
///
/// This is the inverse of [`cid_to_blake3_hash`]: given a BLAKE3 hash and the
/// appropriate codec, it produces a CIDv1.
pub fn blake3_hash_to_cid(hash: iroh_blobs::Hash, kind: ArtifactKind) -> Cid {
    let codec = match kind {
        ArtifactKind::Blob => RAW_CODEC,
        ArtifactKind::Collection => BLAKE3_HASHSEQ_CODEC,
    };
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, hash.as_bytes())
        .expect("BLAKE3 digest is always 32 bytes");
    Cid::new_v1(codec, mh)
}

/// Extract the BLAKE3 digest from a CID's multihash as an iroh-blobs hash.
///
/// Works with any CID codec as long as the multihash uses BLAKE3 (0x1e).
pub fn cid_to_blake3_hash(cid: &Cid) -> Result<iroh_blobs::Hash, Error> {
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
    Ok(iroh_blobs::Hash::from_bytes(digest))
}

/// Verify that `data` matches the expected CID (blake3, raw codec 0x55).
pub fn verify_cid(data: &[u8], expected: &Cid) -> Result<(), Error> {
    let digest = blake3::hash(data);
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes())
        .map_err(|e| Error::Cid(format!("multihash wrap: {e}")))?;
    let actual = Cid::new_v1(RAW_CODEC, mh);

    if actual != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// Compute the CID of a file on disk.
///
/// Streams the file through a BLAKE3 hasher to avoid loading it into memory.
pub fn compute_blob_cid(path: &std::path::Path) -> Result<Cid, Error> {
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher).map_err(Error::Io)?;
    let hash: iroh_blobs::Hash = hasher.finalize().into();
    Ok(blake3_hash_to_cid(hash, ArtifactKind::Blob))
}

/// Verify that a file on disk matches the expected CID.
///
/// Streams the file through a BLAKE3 hasher to avoid loading it into memory.
pub fn verify_cid_file(path: &std::path::Path, expected: &Cid) -> Result<(), Error> {
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher).map_err(Error::Io)?;
    let digest = hasher.finalize();
    let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes())
        .map_err(|e| Error::Cid(format!("multihash wrap: {e}")))?;
    let actual = Cid::new_v1(RAW_CODEC, mh);

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
/// [`compute_content_id`] and [`super::serve::add_collection`].
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

/// Compute a content ID for a directory by building an iroh-blobs Collection.
///
/// Each file is individually BLAKE3-hashed, then a Collection is constructed
/// from (relative_path, hash) pairs in sorted order. The collection's root
/// blob hash serves as the deterministic content ID.
///
/// The resulting CID uses the `blake3-hashseq` codec (0x80).
pub fn compute_content_id(dir: &Path) -> Result<Cid, io::Error> {
    let entries: Vec<(String, iroh_blobs::Hash)> = canonical_walk(dir)?
        .into_iter()
        .map(|(name, path)| {
            let file = std::fs::File::open(&path)?;
            let mut reader = io::BufReader::new(file);
            let mut hasher = blake3::Hasher::new();
            io::copy(&mut reader, &mut hasher)?;
            Ok((name, hasher.finalize().into()))
        })
        .collect::<Result<_, io::Error>>()?;

    let collection = iroh_blobs::format::collection::Collection::from_iter(entries);

    // to_blobs() returns [meta_bytes, hashseq_bytes]. The collection's
    // content ID is the BLAKE3 hash of the HashSeq blob.
    let root_blob = collection.to_blobs().last().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "collection produced no blobs")
    })?;

    let hash = blake3::hash(&root_blob);

    let mh =
        multihash::Multihash::wrap(HASH_CODE_BLAKE3, hash.as_bytes()).map_err(io::Error::other)?;
    let cid = Cid::new_v1(BLAKE3_HASHSEQ_CODEC, mh);

    Ok(cid)
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

    // -- CID conversion tests --

    fn blob_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(RAW_CODEC, mh)
    }

    fn collection_cid(data: &[u8]) -> Cid {
        let digest = blake3::hash(data);
        let mh = Multihash::<64>::wrap(HASH_CODE_BLAKE3, digest.as_bytes()).unwrap();
        Cid::new_v1(BLAKE3_HASHSEQ_CODEC, mh)
    }

    #[test]
    fn verify_cid_matches() {
        let data = b"hello world";
        let cid = blob_cid(data);
        assert!(verify_cid(data, &cid).is_ok());
    }

    #[test]
    fn verify_cid_mismatch() {
        let data = b"hello world";
        let wrong_cid = blob_cid(b"wrong");
        assert!(matches!(
            verify_cid(data, &wrong_cid),
            Err(Error::CidMismatch { .. })
        ));
    }

    #[test]
    fn cid_to_blake3_hash_roundtrip() {
        let data = b"test data";
        let expected_hash = iroh_blobs::Hash::new(data);
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
        let expected_hash = iroh_blobs::Hash::new(data);
        let cid = collection_cid(data);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, expected_hash);
    }

    #[test]
    fn blake3_hash_to_cid_blob_roundtrip() {
        let data = b"test data";
        let hash = iroh_blobs::Hash::new(data);
        let cid = blake3_hash_to_cid(hash, ArtifactKind::Blob);
        assert_eq!(artifact_kind(&cid).unwrap(), ArtifactKind::Blob);
        let extracted = cid_to_blake3_hash(&cid).unwrap();
        assert_eq!(extracted, hash);
    }

    #[test]
    fn blake3_hash_to_cid_collection_roundtrip() {
        let data = b"test data";
        let hash = iroh_blobs::Hash::new(data);
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
