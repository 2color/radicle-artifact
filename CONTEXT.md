# rad-artifact

Two distinct layers:

- Radicle collaborative object ([COB]) in the git storage, synced over the radicle protocol:
  - **Create** a Release tied to a tag/commit
  - **Register** Artifacts against it
  - **Add** download Locations (discovery metadata, never bytes)
- rad-artifact seeding node:
  - **Seeding** is a node holding an Artifact's bytes and serving them to peers over [iroh].

The COB says where bytes can be fetched, a seeding node holds and seeds the bytes.

The two layers ship as separate crates and binaries: `radicle-artifact` is the COB library plus the `rad-artifact` CLI (no iroh/tokio in its tree), and `radicle-artifact-node` is the seeding daemon (`rad-artifact-node`, spawned by `rad-artifact node start`). They share `radicle-artifact-core` (wire protocol, CID helpers, endpoint identity) and talk over the control socket via `radicle-artifact-client` (sync transport for the CLI; async behind its `tokio` feature for embedders). COB-only consumers depend on the lean crates and are never exposed to the iroh dependency tree.

## Language

**Create** (verb):
Open a signed Release in the COB associated with a commit OID and optionally a git tag. Idempotent for a single author: if you already created a Release for the same commit (and tag), Create reuses it rather than opening a duplicate; Releases authored by others are never reused. This git tag is unrelated to the blob-store Tags defined below
_Avoid_: register (you register into a Release, not the Release itself); upsert, ensure (Create is the canonical verb even though it reuses your own Release).

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
Hold an Artifact's bytes on a node and serve them to peers with `radicle-artifact-node`, tracked locally by a Seeded Tag. Distinct from Adding its Location: the `seed` command composes both, but the two acts stay separate, and their drift is a Dangling Tag or an Orphaned Location. _Avoid_: serve/serving, host, mirror (use "seed"/"seeding"); don't widen "seed" to cover adding the Location.

**Fetch** (verb):
Pull an Artifact's bytes into the local node's store, resolving Locations
(iroh or HTTP) and verifying against the CID. Does not write to disk.
Pair with `--seed` to keep serving the bytes afterwards.
_Avoid_: download (that writes a file too); export (that re-emits bytes
already in the store).

**Download** (verb):
Fetch an Artifact into the store, then export the bytes to a file on disk.
_Avoid_: fetch (that's store-only).

**Verify** (verb):
Check that a local file's CID matches an Artifact registered in a Release, applying the same delegate and redaction rules as `list`/`show`. Answers "did a delegate publish exactly these bytes?" Local and read-only: it needs the repository in storage, but no seeding node, no network, and no signer. Where `show` hides a redacted Artifact, Verify fails on one — hiding suits browsing, not verification.
_Avoid_: check, validate; don't conflate with Attest (a claim recorded for others, which rehashes nothing) or with the CID check inside Fetch (transport integrity, not trust).

**Watch** (verb):
Run a process that Seeds every trusted Artifact as peers publish it —
the only place in this project where network discovery happens, which is
why Locate must not borrow the word. Scoped to the repositories the
radicle node seeds, and to Artifacts that pass Verify's trust rules. It
composes Fetch and Add: `rad-artifact watch`.
_Avoid_: mirror, follow, sync (Sync is the radicle round-trip below);
don't use "watch" for a one-shot read such as `list`.

**Locate** (verb):
Query, across every repository in local storage, where an Artifact can be found — the node-wide read over Locations, keyed by CID. Returns the discovery Locations by default, or the Releases that contain the CID with `--releases`. Read-only and local: it reads COBs already in storage and never touches the network.
_Avoid_: find (reserved for the per-repo lookup — one repository, not node-wide); discover (implies network/peer discovery, which Locate never does); search.

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
- **Locate** answers, for a **CID**, every **Location** (and every **Release** that holds it) across all repositories in local storage
- A **Seeded Tag** should correspond to an **Artifact** in some **Release**; when it doesn't, it is a **Dangling Tag**
- **Watching** repeats **Fetch** + **Seed** + **Add** for every trusted **Artifact** a peer publishes, so a node mirrors a repository without being told each **CID**
- **Seeding** and **Adding** a Location are the two halves of making an artifact available over iroh: a node seeds the bytes and adds the `radiroh://` **Location** so peers can discover it; the two drift apart as **Dangling Tags** (seeded, no Location) and **Orphaned Locations** (Location added, no longer seeded)

[COB]: https://radicle.dev/guides/protocol#collaborative-objects
[canonical reference]: https://radicle.dev/2025/08/12/canonical-references
[iroh]: https://docs.iroh.computer/protocols/blobs
[BLAKE3]: https://github.com/BLAKE3-team/BLAKE3
[uri-scheme]: ./docs/uri-scheme.md