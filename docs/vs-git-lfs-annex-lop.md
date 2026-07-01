# radicle-artifact vs git-lfs and git-annex vs LOP

This document compares radicle-artifact to the two most common tools for distributing large files with Git, git-lfs and git-annex, highlighting where its content-addressed, signed, decentralized model differs from theirs.

## vs. git-lfs

- **No single-server dependency** — LFS content is SHA-256-addressed, but resolved through one out-of-band server endpoint; if that server moves or dies, the bytes are unreachable. radicle-artifact records multiple per-DID locations for the same BLAKE3 CID, so bytes can move without breaking identity or availability.
- **Multi-location** — any contributor can announce mirrors under their own DID. Locations are arbitrary URLs (HTTP, iroh, IPFS, magnet, `rasl://`, ...), though the CLI currently fetches only HTTP and iroh. LFS ties you to one server.
- **No central server required** — the bytes never touch the git host; only the signed metadata COB syncs through it. Binaries are served peer-to-peer over iroh-blobs, or from one or more independent HTTP endpoints, so no single server is a chokepoint.
- **Signed by design** — every action (register, attest, location add) is signed by the author's Ed25519 key. LFS has no signing model.

## vs. git-annex

- **Multi-party attestation** — delegates can independently build from the same commit and attest their CID matches, recording this in the COB. git-annex has no attestation concept.
- **Redaction** — artifacts can be formally flagged as compromised with a reason; this is signed and permanent. git-annex has no equivalent.
- **Radicle-native trust model** — trust follows the repo's delegate set, not AWS credentials or GPG keys. No external key management.
- **Structured metadata** — artifacts carry a versioned data model (name, attestations, redactions, locations, metadata map) as a first-class COB, not just an availability ledger.

## vs. Git large-object-promisors (LOP)

[LOP](https://git-scm.com/docs/large-object-promisors) is an in-progress, git-native alternative to LFS: large blobs stay regular git objects (same OID) but live on a separate promisor remote, advertised to clients via a protocol v2 `promisor-remote` capability and offloaded with `git repack --filter`.

- **Content stays out of Git** — LOP keeps blobs as git objects keyed by OID, tied to how they were packed. radicle-artifact keeps only signed metadata in the repo; bytes are addressed by a build-output BLAKE3 CID, so two delegates producing the same binary get the same identity.
- **Not platform-centric** — LOP trust still flows from the main remote and its advertised promisor (hub-and-spoke). radicle-artifact trust is multi-party, following the repo's delegate set, and any delegate can announce mirrors without server permission.
- **Release-scoped, not history-scoped** — LOP applies to every large blob in history; radicle-artifact attaches artifacts to a tag or commit.
- **Complementary** — LOP fixes Git's protocol ergonomics for working-tree assets; it doesn't address reproducible provenance or redaction. A project could use LOP for in-tree assets and radicle-artifact for release outputs.

See [radicle-artifact vs. Git Large-Object-Promisors](./git-large-object-promisors.md) for the full comparison.

## Shared gap both solve, but differently

Both git-lfs and git-annex require external infrastructure you either own or pay for. radicle-artifact's iroh-seeding mode lets any node that has the bytes serve them — seeding is participatory and doesn't require a cloud account.

## Tradeoffs

radicle-artifact requires Radicle and is pre-release with a mutable schema. git-annex is battle-hardened and works with any git host.

## How git-annex works (background)

git-annex replaces large files with symlinks pointing into `.git/annex/objects/` keyed by a content hash. Git tracks the symlink; git-annex tracks the bytes. Remotes (S3, rsync, WebDAV, local drives) store content by key. A distributed availability ledger (in git refs) tracks which remotes hold which keys, enabling partial checkouts.

The COB in radicle-artifact plays a similar role to git-annex's availability ledger, but is signed, structured, and synced over the radicle protocol rather than via `git annex sync`.
