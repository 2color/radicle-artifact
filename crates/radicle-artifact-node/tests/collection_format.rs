//! Cross-check that `radicle_artifact_core::cid::compute_content_id` —
//! which reimplements the iroh-blobs `CollectionV0`/HashSeq encoding to
//! stay iroh-free — matches the real `iroh_blobs` Collection encoding.
//! If iroh-blobs ever changes the format, this fails CI instead of
//! letting the two implementations fork the CID space.

use std::fs;
use std::io;
use std::path::Path;

use cid::Cid;
use iroh_blobs::format::collection::Collection;
use radicle_artifact_core::cid::{
    blake3_hash_to_cid, canonical_walk, compute_content_id, ArtifactKind,
};

/// The pre-split implementation: build the Collection with iroh-blobs
/// and hash its root (HashSeq) blob.
fn compute_content_id_iroh_blobs(dir: &Path) -> Result<Cid, io::Error> {
    let entries: Vec<(String, iroh_blobs::Hash)> = canonical_walk(dir)?
        .into_iter()
        .map(|(name, path)| {
            let bytes = fs::read(&path)?;
            Ok((name, iroh_blobs::Hash::new(&bytes)))
        })
        .collect::<Result<_, io::Error>>()?;

    let collection = Collection::from_iter(entries);
    let root_blob = collection.to_blobs().last().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "collection produced no blobs")
    })?;

    Ok(blake3_hash_to_cid(
        blake3::hash(&root_blob),
        ArtifactKind::Collection,
    ))
}

#[test]
fn content_id_matches_iroh_blobs_collection_encoding() {
    let dir = tempfile::TempDir::new().unwrap();
    let files: &[(&str, &[u8])] = &[
        ("hello.txt", b"hello"),
        ("sub/world.txt", b"world"),
        ("file with spaces.txt", b"spaces matter"),
        (".hidden", b"hidden file"),
        ("empty.txt", b""),
        ("unicode-\u{30c6}\u{30b9}\u{30c8}.bin", b"\x00\x01\xff"),
        ("deep/nested/path/file.txt", b"deeply nested"),
    ];
    for (path, contents) in files {
        let file_path = dir.path().join(path);
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, contents).unwrap();
    }

    let ours = compute_content_id(dir.path()).unwrap();
    let theirs = compute_content_id_iroh_blobs(dir.path()).unwrap();
    assert_eq!(
        ours, theirs,
        "core's CollectionV0/HashSeq reimplementation drifted from iroh-blobs"
    );
}

#[test]
fn empty_dir_matches() {
    let dir = tempfile::TempDir::new().unwrap();
    let ours = compute_content_id(dir.path()).unwrap();
    let theirs = compute_content_id_iroh_blobs(dir.path()).unwrap();
    assert_eq!(ours, theirs);
}
