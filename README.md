# radicle-artifact

Secure artifact distribution for [radicle](https://radicle.dev/).

Git was never built to distribute large file and binaries. Existing solutions like Git LFS encode a URL in the repository tree, which breaks Git's content-addressed nature and leaves every artifact prone to link rot.

`radicle-artifact` makes artifact distribution:

- **Verifiable and signed** — every artifact is hashed with [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) and content addressed with a [CID](https://dasl.ing/cid.html) and bound to the exact commit it was built from. Every [action](#actions) (`Add`, `Attest`, `AddLocation`) is signed by its author's Ed25519 key.
- **Decentralized** — anyone can help serve artifacts, or independently rebuild and verify, increasing resilience, and making serving participatory.
- **Transport-agnostic** — artifacts can have multiple *locations* and shared over HTTP, iroh, IPFS, magnet links, [`rasl://`](https://dasl.ing/rasl.html), or any URL scheme. The cli comes with [iroh-blobs](https://docs.iroh.computer/protocols/blobs) support for reliable peer-to-peer serving and fetching of artifacts with incremental verification.

Trust is multi-party and follows from the repository **delegates**, the trusted maintianers that establish canonical branches and tags. Attestations allow delegates to independently rebuild and **attest** that their  matches, and can also **redact** artifacts if compromised or broken.

radicle-artifact is useful for distributing any data related to code: binaries, static sites, model weights, and scientific datasets.

## How it works

A **Release** is associated with a Git OID (annotated tag or commit). A release contains one or more **Artifacts**, each identified by a content identifier (CID) and a name string. Each artifact tracks the DID that originally added it (the artifact author), and only that DID can update the artifact's name. Users can help serve content by announcing location URLs for any artifact, enabling decentralized mirroring. 

Users can also **attest** to an artifact, recording that they independently verified the CID matches a build from the same commit. Users can also **redact** an artifact, signaling that it should not be used (e.g. due to a supply chain compromise or build reproducibility failure). Redaction is permanent: it supersedes any prior attestation from the same DID and prevents that DID from attesting again.

Each user is identified by a DID that is currently mapped 1:1 to the Radicle NodeID, an Ed25519 public key. This could change in the future — there are ongoing discussions to decouple DIDs from NodeIDs as part of a broader effort to support multiple devices and agents, but for now the two are practically equivalent.

> **Note:** this cob is still in early development and the API is subject to change. Feedback and contributions are very welcome!

This COB is **build-system agnostic**. It works with any toolchain or build process that produces addressable artifacts. Ideally your builds are deterministic (reproducible), which lets other delegates independently verify artifacts and record attestations. However, deterministic builds are not a requirement; you can use radicle-artifact purely for publishing and discovering release artifacts without attestation.

## Workflow

1. **Tag** — Create a [canonical reference](https://radicle.dev/2025/08/12/canonical-references) from an annotated tag or commit.
2. **Build** — Build the release artifacts and derive their content identifiers (CIDs).
3. **Compute CID** — Compute the CID of each artifact by hashing the contents. Folders are hashed as [iroh-blob collections](https://docs.iroh.computer/protocols/blobs#collections).
4. **Add** — Add artifacts to a release using the `rad-artifact add` command, which creates the release if it doesn't exist and records the artifact CID.
5. **Serve** — Upload artifacts to an HTTP server, or serve directly from the CLI using `rad-artifact serve` and register discovery locations per DID.
6. **Fetch** — Fetch artifacts using the `rad-artifact fetch` command.
7. **Attest** — Other delegates check out the release version, build the artifacts independently and attest the CIDs match.
8. **Redact** — If an artifact is found to be compromised or fails reproducibility checks, redact it with a reason.

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
rad-artifact serve <PATH>                                    # serve artifact via iroh-blobs
```

Use `--repository <RID>` to target a specific repo (defaults to cwd).
Use `--no-sync` to skip network announcement after writes.
Use `--no-input` to disable interactive prompts (for scripts and CI).

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

[cob]: https://radicle.dev/guides/protocol#collaborative-objects
