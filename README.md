# radicle-artifact

A Radicle [Collaborative Object][cob] (COB) for recording content-addressed
release artifacts and their discovery locations.

A **Release** is associated with a Git OID (annotated tag or commit) and an
author (the NodeId of the creating node). It contains one or more **Artifacts**,
each identified by a content identifier (CID). Each node can announce a single
discovery URL for any artifact, enabling decentralized mirroring. Nodes can also
**attest** to an artifact, recording that they independently verified the CID
matches a build from the same commit.

> **Note:** this cob is still in early development and the API is subject to change. Feedback and contributions are very welcome!

## Workflow

1. **Tag** — Create a canonical reference with an annotated tag for the release
2. **Build** — Build the release artifacts and compute their content identifiers (CIDs)
3. **Publish** — Create the release COB and add artifacts using the CLI
4. **Host** — Upload artifacts to any server or IPFS and register discovery locations per node
5. **Verify** — Other delegates check out the tagged version, build independently, and attest artifacts whose CIDs match. Attestation can be limited to the artifacts a delegate is able to reproduce locally

## COB type

`org.radworks.artifact`

## Data model

```
Release
├── oid: Oid                          # git commit or annotated tag
├── author: NodeId                    # node that created this release
└── artifacts: Map<Cid, Artifact>
    └── Artifact
        ├── name: String              # human-readable description
        ├── locations: Map<NodeId, Url>
        └── attestations: Set<NodeId>   # nodes that verified the CID
```

- **Cid** — a string newtype for any content-addressing scheme (CIDv1, sha256, etc.)
- **Locations** — plain URLs (`https://`, `ipfs://`, `iroh://`, etc.)
- Each node contributes a single URL per artifact; announcing a new URL replaces the previous one

## Collaboration model

Any node can contribute to any release. There is no restriction to the original
author. This means any node can add artifacts, announce discovery locations, and
record attestations on releases created by others. The `author` field records who created the release but does not gate contributions.

## Actions

| Action           | Description                              |
| ---------------- | ---------------------------------------- |
| `Create`         | Initialize a release for a git OID       |
| `AddArtifact`    | Add an artifact (CID + name) to release  |
| `AddLocation`    | Announce a discovery URL for an artifact |
| `RemoveLocation` | Retract a previously announced URL       |
| `Attest`         | Record independent verification of a CID |

## CLI usage

```
rad-artifact create <OID>                        # create release
rad-artifact add <OID> <CID> <NAME>              # add artifact
rad-artifact locate <OID> <CID> <URL>            # add discovery URL
rad-artifact remove-location <OID> <CID> <URL>   # remove discovery URL
rad-artifact attest <OID> <CID>                  # attest to an artifact
rad-artifact show <OID> [--pretty]               # show release
rad-artifact list [--pretty] [--verbose]          # list all releases
```

Use `--repository <RID>` to target a specific repo (defaults to cwd).
Use `--no-sync` to skip network announcement after writes.

## Project structure

```
src/
├── lib.rs              # core types, COB traits, store layer
├── error.rs            # Build and Apply error types
├── display.rs          # JSON and pretty-print display forms
└── bin/
    └── rad-artifact.rs # CLI binary
```

## Cutting a release

Requires [cargo-release](https://github.com/crate-ci/cargo-release) and
[git-cliff](https://git-cliff.org/).

```
cargo release minor --execute
```

You can use `minor`, `major`, or `patch` and cargo-release will automatically
calculate the next version number. You can also pass an explicit version like
`cargo release 0.3.0 --execute` if needed.

This will bump the version in `Cargo.toml`, generate the changelog via git-cliff,
commit, tag, and publish to crates.io.

By default, `cargo release` runs in dry-run mode — omit `--execute` to preview
what will happen. To preview just the changelog: `git cliff --tag 0.3.0`

## License

MIT OR Apache-2.0

[cob]: https://radicle.xyz/guides/protocol#collaborative-objects
