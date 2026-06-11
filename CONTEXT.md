# rad-artifact

Two distinct layers:

- Radicle collaborative object ([COB]) in the git storage, synced over the radicle protocol:
  - **create** a Release tied to a tag/commit
  - **register** Artifacts against it
  - **add** download Locations (discovery metadata, never bytes)
- rad-artifact seeding node:
  - **Seeding** is a node holding an Artifact's bytes and serving them to peers over [iroh].

The COB says where bytes can be fetched, a seeding node holds and seeds the bytes.

## Language

**Create** (verb):
Open a new signed Release in the COB associated with a commit OID and optionally a git tag. This git tag is unrelated to the blob-store Tags defined below
_Avoid_: register (you register into a Release, not the Release itself).

**Register** (verb):
Record an Artifact against a Release in the COB.
_Avoid_: add (the CLI command was renamed from `add` to `register`), publish.

**Add** (verb):
Record a Location in the COB under your DID, asserting that an artifact's bytes are retrievable at that URL. Applies to any URL scheme. For `radiroh://` Locations specifically, Adding is the COB-side complement to Seeding: a node that adds a its EndpointID Location without seeding creates an Orphaned Location; a node that seeds without adding a Location creates a Dangling Tag. Adding is a COB write like any other; the change reaches peers via Announce, not as part of Adding itself.
_Avoid_: announce (that's the network push, below); publish.

**Announce** (verb):
Announcing is the first step of the radicle *Sync* process below where the local git refs that were updated are broadcast to interested radicle nodes. Note that a successful Sync requires those peers to also fetch and echo the refs back. Applies to every COB write (register, attest, add, …). Triggered automatically by `rad-artifact` after COB operations; deferred with `--no-announce` and sent later with `rad sync -a`.
_Avoid_: broadcast, sync (earlier names for this flag); don't conflate Announcing with the full Sync it kicks off, nor with Adding a Location.

**Sync** (radicle concept):
The round-trip by which a peer confirms it has replicated your refs. A node Announces its sigrefs commit; a remote node learns of it, fetches that sigrefs commit and all references (including every COB), and — because its own refs changed — makes a new ref announcement that now includes your sigrefs commit. When the local node sees the remote echo back the same sigrefs commit it announced, that remote is in sync: one replica. `rad sync` waits for enough replicas (3 by default, or all your preferred seeds) before reporting success.
_Avoid_: announce (that's only the first step); broadcast.

**Seed** (verb):
Hold an Artifact's bytes on a node and serve them to peers over iroh, tracked locally by a Seeded Tag. Distinct from Adding its Location: the `seed` command composes both, but the two acts stay separate, and their drift is a Dangling Tag or an Orphaned Location. _Avoid_: serve/serving, host, mirror (use "seed"/"seeding"); don't widen "seed" to cover adding the Location.

**Fetch** (verb):
Pull an Artifact's bytes into the local node's store, resolving Locations
(iroh or HTTP) and verifying against the CID. Does not write to disk.
Pair with `--seed` to keep serving the bytes afterwards.
_Avoid_: download (that writes a file too); export (that re-emits bytes
already in the store).

**Download** (verb):
Fetch an Artifact into the store, then export the bytes to a file on disk.
_Avoid_: fetch (that's store-only).

**Seeder**:
A node that seeds an Artifact's bytes. Distinct from a Radicle seed node,
which holds the repo's git/COB; one machine can be both, but "Seeder" here
always means the iroh bytes role.
_Avoid_: host, mirror; "provider" is tolerated only as iroh-blobs' internal
term (its download-side name for a Seeder's endpointId).

**Release**:
A COB entry, keyed by a commit, holding a set of Artifacts for a repository.

**Artifact**:
A named, content-addressed file or collection of files within a Release, identified by its CID.

**CID**:
The [BLAKE3] content identifier of an Artifact's bytes.

**EndpointId**:
The iroh network identity of a Seeder, derived from the radicle node's Ed25519 secret and encoded as lowercase base32 (the "endpoint id") in a `radiroh://{endpoint_id}`. See [docs/uri]
Location points peers at it for peer-to-peer fetch.
_Avoid_: address, host; "provider endpoint" is iroh-blobs' internal phrasing.

**Location**:
A URL under a contributor's DID asserting where an Artifact can be fetched. A
`radiroh://{endpoint}` URL names a Seeder's iroh Endpoint for peer-to-peer
fetch; other schemes (e.g. `https://`) point at plain HTTP. _Added_ and
_removed_ (`location add`/`remove`, `add_location`/`remove_location`).
_Avoid_: source, mirror, provider; register (that's for Artifacts).

**Tag**:
A tag is a `(tag_name, hash)` tuple can give to data in iroh-blobs filesystem store to prevent the data with the hash (which is in the CID) from being garbage collected. A *seeded tag* is for content you are seeding. The same CID can have more than one seeded tag, example if you are a seeder for the same artifact in two different repos or releases. During in-flight retrieval a *temporary tag* protects the data from GC until the retrieval is finished and it gets the seeded tag. Not to be confused with a Release's git tag, which lives in radicle git storage and never appears in the blob store.
_Avoid_: pin.

**Dangling Tag**:
A Seeded Tag whose CID no Release references — so no Location can anchor to it.
_Avoid_: orphan tag (the design doc overloads "orphan" for unrelated cases).

**Orphaned Location** (a.k.a. orphaned-self):
A Location under our own DID, pinned to our current endpoint, for a CID the node is no longer seeding — it points peers at us for bytes we don't have. The mirror image of a Dangling Tag, and what `--remove-orphaned[-self]` removes.
_Avoid_: stale location (a Stale Endpoint is the distinct case where the URL is pinned to a _previous_ or undecodable endpoint).

## Relationships

- A **Release** contains one or more **Artifacts**
- An **Artifact** has zero or more **Locations**, grouped by contributor **DID**
- A **Seeded Tag** should correspond to an **Artifact** in some **Release**; when it doesn't, it is a **Dangling Tag**
- **Seeding** and **Adding** a Location are the two halves of making an artifact available over iroh: a node seeds the bytes and adds the `radiroh://` **Location** so peers can discover it; the two drift apart as **Dangling Tags** (seeded, no Location) and **Orphaned Locations** (Location added, no longer seeded)

[COB]: https://radicle.dev/guides/protocol#collaborative-objects
[canonical reference]: https://radicle.dev/2025/08/12/canonical-references
[iroh]: https://docs.iroh.computer/protocols/blobs
[BLAKE3]: https://github.com/BLAKE3-team/BLAKE3
[uri-scheme]: ./docs/uri-scheme.md