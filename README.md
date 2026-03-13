# radicle-artifact

A Radicle [Collaborative Object][cob] (COB) for recording content-addressed
release artifacts and their discovery locations.

A **Release** is associated with a Git OID (annotated tag or commit) and
contains one or more **Artifacts**, each identified by a content identifier
(CID). Multiple nodes can announce discovery URLs for any artifact, enabling
decentralized mirroring.

[cob]: https://radicle.xyz/guides/protocol#collaborative-objects

## COB type

`org.radworks.artifact`

## Data model

```
Release
├── oid: Oid                          # git commit or annotated tag
└── artifacts: Map<Cid, Artifact>
    └── Artifact
        ├── name: String              # human-readable description
        └── locations: Map<NodeId, Vec<Url>>
```

- **Cid** — a string newtype for any content-addressing scheme (CIDv1, sha256, etc.)
- **Locations** — plain URLs (`https://`, `ipfs://`, `iroh://`, etc.)
- Multiple nodes can contribute locations for the same artifact

## Actions

| Action           | Description                              |
| ---------------- | ---------------------------------------- |
| `Create`         | Initialize a release for a git OID       |
| `AddArtifact`    | Add an artifact (CID + name) to release  |
| `AddLocation`    | Announce a discovery URL for an artifact |
| `RemoveLocation` | Retract a previously announced URL       |

## CLI usage

```
rad-artifact create <OID>                        # create release
rad-artifact add <OID> <CID> <NAME>              # add artifact
rad-artifact locate <OID> <CID> <URL>            # add discovery URL
rad-artifact remove-location <OID> <CID> <URL>   # remove discovery URL
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

## License

MIT OR Apache-2.0
