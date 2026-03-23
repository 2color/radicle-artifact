# radicle-artifact

A Radicle [Collaborative Object][cob] (COB) for recording content-addressed
release artifacts and their discovery locations.

A **Release** is associated with a Git OID (annotated tag or commit) and an
author (the DID of the creating user). It contains one or more **Artifacts**,
each identified by a content identifier (CID). Each artifact tracks the DID that
originally added it (the artifact author), and only that DID can update the
artifact's name. Each user can announce multiple discovery URLs for any artifact,
enabling decentralized mirroring. Users can also **attest** to an artifact,
recording that they independently verified the CID matches a build from the same
commit. Users can also **redact** an artifact, signaling that it should not be
used (e.g. due to a supply chain compromise or build reproducibility failure).
Redaction is permanent: it supersedes any prior attestation from the same DID and
prevents that DID from attesting again.

Each user is identified by a DID derived which is currently mapped 1:1 to the Radicle NodeID, an ED25519 public key. This could change in the future — there are ongoing discussions to decouple DIDs from NodeIDs as part of a broader effort to support multiple devices and agents, but for now the two are practically equivalent.

> **Note:** this cob is still in early development and the API is subject to change. Feedback and contributions are very welcome!

This COB is **build-system agnostic**. It works with any toolchain or build
process that produces addressable artifacts. Ideally your builds are
deterministic (reproducible), which lets other delegates independently verify
artifacts and record attestations. However, deterministic builds are not a
requirement; you can use radicle-artifact purely for publishing and discovering
release artifacts without attestation.

## Workflow

1. **Tag** — Create a canonical reference with an annotated tag for the release
2. **Build** — Build the release artifacts and compute their content identifiers (CIDs)
3. **Publish** — Create the release COB and add artifacts using the CLI
4. **Host** — Upload artifacts to any server or IPFS and register discovery locations per DID
5. **Verify** — Other delegates check out the tagged version, build independently, and attest artifacts whose CIDs match. Attestation can be limited to the artifacts a delegate is able to reproduce locally
6. **Redact** — If an artifact is found to be compromised or fails reproducibility checks, any DID can redact it with a reason. Redactions are permanent: they supersede prior attestations from the same DID and block future attestations from that DID

## COB type

`org.radworks.artifact`

## Data model

```
Release
├── oid: Oid                          # git commit or annotated tag
├── author: Did                       # user that created this release
└── artifacts: Map<Cid, Artifact>
    └── Artifact
        ├── author: Did               # user that added this artifact
        ├── name: String              # human-readable description (only author can update)
        ├── locations: Map<Did, Set<Url>>
        ├── attestations: Set<Did>      # users that verified the CID
        └── redactions: Map<Did, String> # users that flagged the artifact, with reason
```

- **Cid** — a string newtype for any content-addressing scheme (CIDv1, sha256, etc.)
- **Locations** — plain URLs (`https://`, `ipfs://`, `iroh://`, etc.)
- Each user can contribute multiple URLs per artifact; duplicate URLs are deduplicated automatically

## Collaboration model

Any user can contribute to any release. There is no restriction to the original
author. This means any user can add artifacts, announce discovery locations, and
record attestations on releases created by others. The `author` field records who created the release but does not gate contributions.

## Actions

| Action           | Description                                                     |
| ---------------- | --------------------------------------------------------------- |
| `Create`         | Initialize a release for a git OID                              |
| `AddArtifact`    | Add an artifact (CID + name), or update name if author re-sends |
| `AddLocation`    | Announce a discovery URL for an artifact                        |
| `RemoveLocation` | Retract a previously announced URL                              |
| `Attest`         | Record independent verification of a CID                        |
| `Redact`         | Flag an artifact as compromised/withdrawn                       |

## CLI usage

```
rad-artifact create <OID>                        # create release
rad-artifact add <OID> <CID> <NAME>              # add artifact
rad-artifact locate <OID> <CID> <URL>            # add discovery URL
rad-artifact remove-location <OID> <CID> <URL>   # remove discovery URL
rad-artifact attest <OID> <CID>                  # attest to an artifact
rad-artifact redact <OID> <CID> <REASON>         # redact an artifact
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
