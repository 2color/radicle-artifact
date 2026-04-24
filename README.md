# radicle-artifact

Secure artifact distribution for [radicle](https://radicle.xyz/).

Git was never built to distribute large files and binaries. Existing solutions like Git LFS encode a URL in the repository tree, which breaks Git's content-addressed nature and leaves every artifact prone to link rot.

## In plain English

Delegates create a canonical tag for a release, build artifacts, and anyone can verify they're exactly what you built — no matter where they're downloaded from. Other team members can independently rebuild the release and cryptographically sign that they got the same result. If an artifact turns out to be compromised or broken, any delegate can redact it with a reason so others know not to trust it.

Artifacts can be hosted by an HTTP server, directly from the cli (peer-to-peer serving with [iroh](https://www.iroh.computer/)), BitTorrent, or IPFS. They persist as long as at least one location URL is reachable. Hosting is participatory — any radicle user can announce a location to increase redundancy.

## Why radicle-artifact

| Feature              | Git LFS          | GitHub Releases  | radicle-artifact              |
| -------------------- | ---------------- | ---------------- | ----------------------------- |
| Content verification | URL-based        | URL-based        | Cryptographic CID             |
| Hosting              | Central server   | Central server   | Any HTTP, IPFS, P2P           |
| Trust model          | Single publisher | Single publisher | Multi-party attestation       |
| Bound to commit      | No               | Loose (tag)      | Yes (signed, per-OID release) |

More specifically, `radicle-artifact` makes artifact distribution:

- **Verifiable and signed** — every artifact is content addressed with a [CID](https://dasl.ing/cid.html) and bound to the exact commit it was built from. Every [action](#actions) (`Add`, `Attest`, `AddLocation`) is signed by its author's Ed25519 key.
- **Decentralized** — anyone can help serve artifacts, or independently rebuild and verify, increasing resilience, and making serving participatory.
- **Transport-agnostic** — artifacts can have multiple _locations_ and be shared over HTTP, iroh, IPFS, magnet links, [`rasl://`](https://dasl.ing/rasl.html), or any URL scheme. The CLI comes with [iroh-blobs](https://docs.iroh.computer/protocols/blobs) support for reliable peer-to-peer serving and fetching of artifacts with incremental verification.

Trust is multi-party and follows from the repository **delegates**, the trusted maintainers that establish canonical branches and tags. Attestations allow delegates to independently rebuild and **attest** that their build matches, and they can also **redact** artifacts if compromised or broken.

radicle-artifact is useful for distributing any data related to code: binaries, static sites, model weights, and scientific datasets.

> **Note:** this collaborative object (COB) is still in early development and the API is subject to change. Feedback and contributions are very welcome!

## How it works

A **Release** is associated with a Git OID (annotated tag or commit). A release contains one or more **Artifacts**, each identified by a content identifier (CID) and a name string. Each artifact tracks the DID (decentralized identifier) that originally added it, and only that DID can update the artifact's name.

Users can **announce locations** for any artifact — URLs where the bytes can be fetched — enabling decentralized mirroring. They can also **attest** to an artifact, recording that they independently verified the CID matches a build from the same commit. Finally, they can **redact** an artifact, signaling that it should not be used (e.g. due to a supply chain compromise or build reproducibility failure). Redaction is permanent: it supersedes any prior attestation from the same DID and prevents that DID from attesting again.

### Trust model

The trust model follows from radicle: multiple delegates independently build the same commit and attest that the CIDs match.

This COB is **build-system agnostic**. It works with any toolchain or build process that produces addressable artifacts. Ideally your builds are deterministic (reproducible), which lets other delegates independently verify artifacts and record attestations. However, deterministic builds are not a requirement; you can use radicle-artifact purely for publishing and discovering release artifacts without attestation.

### Implementation notes

Each user is identified by a DID that is currently mapped 1:1 to the Radicle NodeID, an Ed25519 public key. This could change in the future — there are ongoing discussions to decouple DIDs from NodeIDs as part of a broader effort to support multiple devices and agents, but for now the two are practically equivalent.

## Workflow

1. **Tag** — Create a [canonical reference](https://radicle.xyz/2025/08/12/canonical-references) from an annotated tag or commit.
2. **Build** — Build the release artifacts.
3. **Compute CID** — Compute the CID of each artifact by hashing the contents. Folders are hashed as [iroh-blob collections](https://docs.iroh.computer/protocols/blobs#collections).
4. **Add** — Add artifacts to a release using the `rad-artifact add` command, which creates the release if it doesn't exist and records the artifact CID.
5. **Serve** — Upload artifacts to an HTTP server, or serve directly from the CLI using `rad-artifact serve` and register discovery locations per DID.
6. **Fetch** — Fetch artifacts using the `rad-artifact fetch` command.
7. **Attest** — Other delegates check out the release version, build the artifacts independently and attest the CIDs match.
8. **Redact** — If an artifact is found to be compromised or fails reproducibility checks, redact it with a reason.

## Requirements

- A working [Radicle](https://radicle.xyz/) installation (the `rad` CLI and a node identity)
- The `rad-artifact` CLI (this crate)
- Optional: a deterministic build toolchain if you want other delegates to attest

No central server or hosted service is required.

## COB type

`org.radworks.artifact`

## Data model

```
Release
├── oid: Oid                          # git commit or annotated tag (one release per OID)
└── artifacts: Map<Cid, Artifact>
    └── Artifact
        ├── author: Did               # user that added this artifact
        ├── name: String              # human-readable description (only author can update)
        ├── locations: Map<Did, Set<Url>>
        ├── attestations: Set<Did>      # users that verified the CID
        └── redactions: Map<Did, String> # users that flagged the artifact, with reason
```

- **Cid** — a string newtype for any content-addressing scheme (CIDv1, sha256, etc.)
- **Locations** — plain URLs (`https://`, `ipfs://`, `magnet://`, [`rasl://`](https://dasl.ing/rasl.html), `iroh://`, etc.)
- Each user can contribute multiple URLs per artifact; duplicate URLs are deduplicated automatically

## Collaboration model

A release is identified by its commit OID, not by the person who created it. Any user can add artifacts, announce discovery locations, or attest and redact on any release. The first contributor's `add` creates the release COB; subsequent contributors reuse it. Artifact-level attribution is still recorded — only the DID that added an artifact can update its name, and attestations/redactions are attributed to their signer.

Trust weighting happens at the artifact level: redactions and attestations from repository **delegates** are highlighted, and `list --delegates-only` filters to releases that contain at least one artifact from a delegate.

Two unsynced nodes can independently create a release COB for the same commit. After they sync, retrieval (`fetch`, `serve`) unions artifacts and locations across every release COB for the given commit or CID, so duplicate COBs don't break discovery. A fully deterministic per-OID COB ID — so that both nodes produce the same COB identity before syncing — would require upstream changes to the `radicle-cob` crate and is not implemented here.

## Actions

| Action           | Description                                                                  |
| ---------------- | ---------------------------------------------------------------------------- |
| `Create`         | Initialize a release for a git OID (internal, auto-created by `AddArtifact`) |
| `AddArtifact`    | Add an artifact (CID + name), or update name if author re-sends              |
| `AddLocation`    | Announce a discovery URL for an artifact                                     |
| `RemoveLocation` | Retract a previously announced URL                                           |
| `Attest`         | Record independent verification of a CID                                     |
| `Redact`         | Flag an artifact as compromised/withdrawn                                    |

## CLI usage

`<COMMIT>` accepts a full OID, abbreviated hash, or tag name of a **commit or annotated tag**.

```
rad-artifact add <COMMIT> --cid <CID> -n <NAME>              # add artifact (creates release if needed)
rad-artifact location add <COMMIT> --cid <CID> <URL>         # add discovery URL
rad-artifact location remove <COMMIT> --cid <CID> <URL>      # remove discovery URL
rad-artifact attest <COMMIT> --cid <CID>                     # attest to an artifact
rad-artifact redact <COMMIT> --cid <CID> -m <REASON>         # redact an artifact
rad-artifact show <COMMIT> [--pretty]                        # show release
rad-artifact list [--pretty] [--delegates-only]              # list releases (--delegates-only: with ≥1 delegate-authored artifact)
rad-artifact cid <PATH>                                      # compute BLAKE3 CID
rad-artifact fetch [<COMMIT> --cid <CID>]                    # fetch artifact (interactive without args)
rad-artifact serve <PATH>                                    # serve artifact via iroh-blobs (runs in foreground)
```

Use `--repository <RID>` to target a specific repo (defaults to cwd).
Use `--no-sync` to skip network announcement after writes.
Use `--no-input` to disable interactive prompts (for scripts and CI).

### Typical session

Publishing a release after a build:

```bash
# Compute the CID of the built artifact
$ rad-artifact cid ./target/release/myapp
bafkr4ih...

# Add it to the release for tag v1.0.0 (creates the release COB on first add)
$ rad-artifact add v1.0.0 --cid bafkr4ih... -n "linux-amd64-binary"

# Announce where others can fetch it
$ rad-artifact location add v1.0.0 --cid bafkr4ih... https://mycdn.example/myapp-v1.0.0

# Or serve it directly over iroh-blobs (blocks the terminal)
$ rad-artifact serve ./target/release/myapp
```

Another delegate then independently verifies it:

```bash
git checkout v1.0.0 && cargo build --release
rad-artifact cid ./target/release/myapp            # should match bafkr4ih...
rad-artifact attest v1.0.0 --cid bafkr4ih...
```

### Example output

```
$ rad-artifact show v1.0.0 --pretty
Release v1.0.0 (commit a1b2c3d…)
└── bafkr4ih…  linux-amd64-binary
    author:        did:key:z6Mk…alice
    attestations:  did:key:z6Mk…bob (delegate)
    locations:     https://mycdn.example/myapp-v1.0.0
                   iroh://…
```

### CI example

Publishing on tag push, e.g. from GitHub Actions:

```yaml
- name: Publish artifact
  run: |
    cargo build --release
    CID=$(rad-artifact cid ./target/release/myapp)
    rad-artifact add "$GITHUB_REF_NAME" --cid "$CID" -n "linux-amd64-binary" --no-input
    rad-artifact location add "$GITHUB_REF_NAME" --cid "$CID" \
      "https://github.com/${GITHUB_REPOSITORY}/releases/download/${GITHUB_REF_NAME}/myapp" --no-input
```

## Glossary

| Term         | Meaning                                                                                                |
| ------------ | ------------------------------------------------------------------------------------------------------ |
| **COB**      | Collaborative Object — Radicle's signed, replayable state machine stored in git                        |
| **CID**      | Content Identifier — a hash that uniquely addresses a blob's bytes ([spec](https://dasl.ing/cid.html)) |
| **DID**      | Decentralized Identifier — who signed an action; currently 1:1 with a Radicle NodeID                   |
| **OID**      | Git Object ID — the SHA-1 of a commit or annotated tag                                                 |
| **RID**      | Radicle Repository ID                                                                                  |
| **Delegate** | A maintainer whose key is authorized to establish canonical branches and tags for a repo               |
| **Attest**   | Sign a statement that you independently produced the same CID from the same commit                     |
| **Redact**   | Sign a statement that an artifact should not be used (compromise, build failure, etc.)                 |

## Project structure

```
src/
├── lib.rs              # core types, COB traits, store layer
├── error.rs            # Build and Apply error types
├── display.rs          # JSON and pretty-print display forms
└── bin/
    └── rad-artifact.rs # CLI binary
```

## How the COB is implemented

The COB is implemented using the [`radicle`](https://crates.io/crates/radicle) crate's COB framework.

The `Release` type implements three traits that plug into the framework:

- **`CobWithType`** — registers the type name `org.radworks.artifact`
- **`Cob`** — defines how to build initial state from the first operation (`from_root`) and how to apply subsequent operations (`op`)
- **`Evaluate`** — deserializes git entries into typed operations and feeds them through the state machine

Each mutation (create, add artifact, attest, redact, etc.) is an `Action` that implements `CobAction`. Actions are written to git as signed entries via `Transaction`, and state is reconstructed on read by replaying the operation DAG.

| What the COB does                              | Module                | Key types                                             |
| ---------------------------------------------- | --------------------- | ----------------------------------------------------- |
| Define and evaluate the state machine          | `radicle::cob`        | `Evaluate`, `Op`, `Entry`                             |
| Persist and transact operations as git objects | `radicle::cob::store` | `Store`, `Transaction`, `Cob`, `CobAction`            |
| Read from and write to the git repository      | `radicle::storage`    | `ReadRepository`, `WriteRepository`, `SignRepository` |
| Sign entries with the user's Ed25519 key       | `radicle::crypto`     | `Signer`, `Signature`, `Device`                       |
| Announce changes to the network                | `radicle::node`       | `Node`, `Announcer`                                   |

## Cutting a release

Requires [cargo-release](https://github.com/crate-ci/cargo-release) and
[git-cliff](https://git-cliff.org/).

```
cargo release minor --execute
```

You can use `minor`, `major`, or `patch` and cargo-release will automatically calculate the next version number. You can also pass an explicit version like `cargo release 0.3.0 --execute` if needed. This will bump the version in `Cargo.toml`, generate the changelog via git-cliff,
commit, tag, and publish to crates.io.

By default, `cargo release` runs in dry-run mode — omit `--execute` to preview what will happen. To preview just the changelog: `git cliff --tag 0.3.0`

## License

MIT OR Apache-2.0

[cob]: https://radicle.xyz/guides/protocol#collaborative-objects
