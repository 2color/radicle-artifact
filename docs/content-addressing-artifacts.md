# Content addressing in Radicle Artifact

This document describes how Radicle Artifact derives a Content Identifier (CID) for an artifact, whether it is a single file or a directory of files. It also explains why we chose iroh collections for directories. The goal is one CID that identifies either a single file or a collection of files (a directory) as one artifact. In both cases, BLAKE3 verifies the contents of every file. For a collection, the CID also verifies every **file name and path**..

The implementation lives in [`radicle-artifact-core/src/cid.rs`](../crates/radicle-artifact-core/src/cid.rs).

## The Problem

To register an artifact, we need a fingerprint: a single identifier that says "exactly this content."

For a single file, the problem is already solved. A BLAKE3 hash of the file's bytes is deterministic, has no overhead, and there is only one way to compute it. Verifying the hash verifies the file.

A directory is harder. There is no single byte stream to hash, so we have to define one. The set of files, their names, their relative paths, and their order all have to be part of the identifier, or two directories with different layouts could collide. But nothing else may leak in. A naive approach, tar the directory and hash the tarball, is fragile because tar archives encode metadata (timestamps, permissions, ownership, iteration order) that varies between machines, producing different hashes for identical file contents.

We need a content-addressed representation of a directory that is:

- **Deterministic**: the same file names and contents always produce the same hash
- **Lightweight**: minimal overhead beyond the file data itself
- **Canonical**: one correct representation, no ambiguity

The two cases should also share one identifier format, so that a consumer can tell from the identifier alone whether it points at a file or a directory, and verify either the same way.

## How iroh Collections Work

An iroh `Collection` is a flat list of `(String, blake3::Hash)` pairs. Filenames are mapped to 32-byte BLAKE3 content hashes. There is no notion of directories as separate entries — directory structure is encoded in the name strings as relative paths (e.g. `"assets/style.css"`, `"js/app.js"`). This keeps the format flat while still representing arbitrary directory trees.

On the wire, a collection splits into two blobs:

### 1. The Metadata Blob (`CollectionMeta`)

Serialized with [postcard](https://docs.rs/postcard) (a compact, no_std-friendly binary format):

```
┌─────────────────────────┐
│ header: "CollectionV0." │  13 bytes, magic/version tag
├─────────────────────────┤
│ names: Vec<String>      │  varint-prefixed length, then
│   "index.html"          │  each string is varint-length
│   "assets/style.css"    │  prefixed + raw UTF-8 bytes
│   "js/app.js"           │
└─────────────────────────┘
```

No delimiters between strings — postcard uses length-prefixed encoding throughout (similar to protobuf, but without field tags, making it more compact).

### 2. The Root Blob (`HashSeq`)

A sequence of 32-byte BLAKE3 hashes:

```
┌──────────────────────────┐
│ hash(metadata blob)      │  32 bytes
│ hash("index.html")       │  32 bytes
│ hash("assets/style.css") │  32 bytes
│ hash("js/app.js")        │  32 bytes
└──────────────────────────┘
```

The first entry is the hash of the metadata blob. The remaining entries correspond 1:1 with the names in the metadata.

### The Collection Hash

The **BLAKE3 hash of the root blob** is the single hash that identifies the entire collection. Verifying this one hash verifies every **file name** and every **file's contents**.

```
Collection Hash = blake3(root blob)
                = blake3(hash(meta) ‖ hash(file₁) ‖ hash(file₂) ‖ …)
```

### The Canonical Walk

The collection format fixes the encoding, but not which files go in or in what order. `canonical_walk` fixes that part so two machines build the same list from the same directory:

- Only regular files are included. Directories are implied by the file paths, and symlinks are skipped, not followed.
- Names are paths relative to the artifact root, with `/` as the separator on every platform.
- Entries are sorted by name (byte order), so the filesystem's iteration order does not leak into the hash.

`compute_content_id` runs this walk, hashes each file with a streaming BLAKE3 hasher, builds the root blob from the sorted `(name, hash)` pairs, and hashes that.

### Independent from iroh-blobs

The core crate does not depend on iroh-blobs. It reproduces the `CollectionV0` metadata encoding and the HashSeq layout directly, so a CLI can compute a directory's CID without pulling in the networking stack. The node crate, which does depend on iroh-blobs, has a cross-check test (`tests/collection_format.rs`) that computes the same directory's CID with the real `iroh_blobs::format::collection::Collection` and asserts equality. If the upstream format ever drifts, CI fails instead of silently forking the CID space.

## Why This Approach

**No metadata pollution.** Unlike tar/zip, there are no timestamps, permissions, or ownership fields. Two directories with identical file names and contents always produce the same collection hash, regardless of when or where they were built.

**Flat representation of trees.** Directory structure lives in the name strings as relative paths (`"components/Header.tsx"`), not as separate directory entries with their own metadata. This means there's exactly one entry per file and no ambiguity about how empty directories or nested paths are represented.

**Positional, tag-free encoding.** Postcard serializes fields in declaration order with no field numbers or type tags. This eliminates the ambiguity that self-describing formats introduce (field reordering, unknown field handling). The `"CollectionV0."` magic header handles versioning instead.

**Compact.** The overhead per file is just a varint-prefixed filename string in the metadata blob and a 32-byte hash in the root blob. No redundant structure.

**Streaming verification.** Because the root blob is a hash sequence, a verifier can check individual files incrementally as they arrive.

**Ready-made distribution.** Because we're already using the iroh collection format, build artifacts can be distributed directly over iroh's peer-to-peer network without any conversion step. A publisher produces a collection, signs it, and shares it — peers fetch and verify using the same structures. The hashing format and the transfer protocol speak the same language.

**BLAKE3.** The hash function is fast (parallelizable, SIMD-accelerated) and produces 256-bit digests. It is also adopted by the [BDASL](https://dasl.ing/bdasl.html) spec.

## In the Radicle Artifact

iroh itself identifies content by a raw BLAKE3 hash. We wrap that hash in a **CIDv1** so the identifier is self-describing: it carries the hash function and says whether it points at a single blob or a collection.

```
CIDv1 = <version 1> <codec> <multihash: 0x1e (BLAKE3), 32-byte digest>
```

The codec distinguishes the two kinds of artifact:

| Artifact kind | Codec                    | Digest                          | Produced by           |
| ------------- | ------------------------ | ------------------------------- | --------------------- |
| Blob          | `raw` (0x55)             | `blake3(file contents)`         | `compute_blob_cid`    |
| Collection    | `blake3-hashseq` (0x80)  | `blake3(root blob)` (see above) | `compute_content_id`  |

`artifact_kind` reads the codec back out of a CID, and `cid_to_blake3_hash` recovers the digest for the iroh-blobs boundary, where fetching and seeding work with plain BLAKE3 hashes. It accepts either codec but rejects any multihash that is not BLAKE3.

The `Cid` type is a newtype around the `cid` crate's `Cid`. It is the only CID type in the codebase; the inner value is exposed only at the iroh-blobs hash boundary. The wrapper exists so that serde uses the multibase string form (base32, `b…`) as the one encoding, in COB actions, on the wire, and in JSON. The `cid` crate's derived `Serialize` would emit a byte array instead. `Display` and `FromStr` use the same string form, so an identifier looks the same in a CLI, a log line, and a JSON payload:

Two helpers complete the picture. `verify_cid_file` streams a file through BLAKE3 and compares the resulting blob CID against an expected one, returning a `CidMismatch` error with both values on failure. `compute_size_from_path` reports the logical size of an artifact: the file length for a blob, or the sum of member file lengths for a directory, using the same canonical walk so it matches what the seeder reports.
