# rad-artifact

Two distinct layers:

- Radicle collaborative object ([COB]) in the git storage, synced over the radicle protocol:
  - **Create** a Release tied to a tag/commit
  - **Register** Artifacts against it
  - **Add** download Locations (discovery metadata, never bytes)
- rad-artifact seeding node:
  - **Seed** an Artifact: hold its bytes on a node and serve them to peers over [iroh].

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
Record a Location in the COB under your DID, asserting that an artifact's bytes are retrievable at that URL. Applies to any URL scheme. For `radiroh://` Locations specifically, Adding is the COB-side complement to Seeding: a node that adds a Location for its EndpointId without seeding creates an Orphaned Location; a node that seeds without adding a Location leaves a missing Location, which Reconcile adds. Adding is a COB write like any other; the change reaches peers via Announce, not as part of Adding itself.
_Avoid_: announce (that's the network push, below); publish.

**Announce** (verb):
Announcing is the first step of the radicle *Sync* process below where the local git refs that were updated are broadcast to interested radicle nodes. Note that a successful Sync requires those peers to also fetch and echo the refs back. Applies to every COB write (register, attest, add, …). Triggered automatically by `rad-artifact` after COB operations; deferred with `--no-announce` and sent later with `rad sync -a`.
_Avoid_: broadcast, sync (earlier names for this flag); don't conflate Announcing with the full Sync it kicks off, nor with Adding a Location.

**Sync** (radicle concept):
The round-trip by which a peer confirms it has replicated your refs. A node Announces its sigrefs commit; a remote node learns of it, fetches that sigrefs commit and all references (including every COB), and — because its own refs changed — makes a new ref announcement that now includes your sigrefs commit. When the local node sees the remote echo back the same sigrefs commit it announced, that remote is in sync: one replica. `rad sync` waits for enough replicas (3 by default, or all your preferred seeds) before reporting success.
_Avoid_: announce (that's only the first step); broadcast.

**Seed** (verb):
Hold an Artifact's bytes on a node and serve them to peers with `radicle-artifact-node`, tracked locally by a Seeded Tag. Distinct from Adding its Location: the `seed` command composes both, but the two acts stay separate, and their drift is a missing Location or an Orphaned Location.
_Avoid_: serve/serving, host, mirror (use "seed"/"seeding"); don't widen "seed" to cover adding the Location.

**Unseed** (verb):
Remove the Seeded Tag for an Artifact in one Release, so the node stops seeding its bytes there. The bytes are garbage-collected only after the last Seeded Tag for that CID is removed, so the node keeps seeding the same CID in other Releases. The `unseed` command also removes your `radiroh://` Locations for the Artifact. This mirrors how `seed` composes Seed and Add, but the two acts stay separate. If you Unseed and keep the Location, you get an Orphaned Location.
_Avoid_: unpin, stop serving, delete (that's for Releases); don't widen "unseed" to cover removing the Location.

**Fetch** (verb):
Pull an Artifact's bytes into the local node's store, resolving Locations (iroh or HTTP) and verifying against the CID. Does not write to disk. Pair with `--seed` to keep seeding the bytes afterwards.
_Avoid_: download (that writes a file too); export (that re-emits bytes already in the store).

**Download** (verb):
Fetch an Artifact into the store, then export the bytes to a file on disk.
_Avoid_: fetch (that's store-only).

**Verify** (verb):
Check that a local file's CID matches an Artifact registered in a Release, applying the same Trust and redaction rules as `list`/`show`. Answers "did a trusted party register exactly these bytes?" Local and read-only: it needs the repository in storage, but no seeding node, no network, and no signer. Where `show` hides a redacted Artifact, Verify fails on one — hiding suits browsing, not verification.
_Avoid_: check, validate; don't conflate with Attest (a claim recorded for others, which rehashes nothing) or with the CID check inside Fetch (transport integrity, not trust).

**Attest** (verb):
Record a signed claim that you independently built or checked an Artifact and got the same CID. Rehashes nothing; run Verify first. The Author's own attestation is a no-op, because Registering already implies it, and the CLI refuses it.
_Avoid_: verify (that's the local hash check), sign, approve.

**Redact** (verb):
Record, with a reason, that an Artifact should not be used. Anyone can redact, but only a redaction by a Trusted Party withdraws the Artifact; others carry no authority. Permanent: it supersedes your earlier attestation and stops you attesting again. Never touches the bytes wherever they are seeded or hosted.
_Avoid_: hide, delete (`delete` removes your ref to a whole Release), revoke.

**Reconcile** (verb):
Compare the node's Seeded Tags with the `radiroh://` Locations under your DID, add missing Locations, and report Orphaned Locations, Stale Endpoints and Dangling Tags. Removes a Location only when asked (`--remove-orphaned`, `--remove-orphaned-self`).
_Avoid_: sync, repair.

**Watch** (verb):
Run a process that Seeds every trusted Artifact as peers register it — the only place in this project where network discovery happens, which is why Locate must not be called "discover". Scoped to the repositories the radicle node seeds, and to Artifacts that pass Verify's trust rules. It composes Fetch, Seed and Add: `rad-artifact watch`.
_Avoid_: mirror, follow, sync (Sync is the radicle round-trip above); don't use "watch" for a one-shot read such as `list`.

**Locate** (verb):
Query, across every repository in local storage, where an Artifact can be found — the node-wide read over Locations, keyed by CID. Returns the discovery Locations by default, or the Releases that contain the CID with `--releases`. Read-only and local: it reads COBs already in storage and never touches the network.
_Avoid_: find (reserved for the per-repo lookup — one repository, not node-wide); discover (implies network/peer discovery, which Locate never does); search.

**Delegate**:
A maintainer named in the Radicle repository identity. The root of Trust.

**Creator**:
The DID that Created a Release.
_Avoid_: author (that's per Artifact).

**Author**:
The DID that first Registered an Artifact. Only the Author can rename it.
_Avoid_: creator (that's per Release), owner.

**Trust**:
The set of DIDs whose Releases and Artifacts count by default: the Delegates and the local user. A *Scope* picks which side of that line to show: `Trusted` (default), `Untrusted` (`--untrusted`) or `All` (`--all-authors`). A Scope never widens who can Redact or write metadata.

**Trusted Party**:
For one Artifact, its Author or a Delegate. Only their redactions and metadata writes take effect.
_Avoid_: trusted user (that's Trust, which includes the local user but not the Author).

**Seeder**:
A node that seeds an Artifact's bytes. Distinct from a Radicle seed node, which holds the repo's git/COB; one machine can be both, but "Seeder" here always means the iroh bytes role.
_Avoid_: host, mirror; "provider" is tolerated only as iroh-blobs' internal term (its download-side name for a Seeder's EndpointId).

**Release**:
A COB entry, keyed by a commit, holding a set of Artifacts for a repository. An empty Release (no Artifacts) is a normal Release. A *fully redacted* Release is one where a Trusted Party redacted every Artifact.

**Artifact**:
A named, content-addressed file (a *blob*) or folder (a *collection*) within a Release, identified by its CID.

**CID**:
The [BLAKE3] content identifier of an Artifact's bytes.

**EndpointId**:
The iroh network identity of a Seeder, derived from the radicle node's Ed25519 secret and encoded as lowercase base32. A `radiroh://{endpoint_id}` Location points peers at it for peer-to-peer fetch. See [docs/uri-scheme.md][uri-scheme].
_Avoid_: address, host; "provider endpoint" is iroh-blobs' internal phrasing.

**Location**:
A URL under a contributor's DID asserting where an Artifact can be fetched. A `radiroh://{endpoint_id}` URL names a Seeder's EndpointId for peer-to-peer fetch; other schemes (e.g. `https://`) point at plain HTTP. _Added_ and _removed_ (`location add`/`remove`, `add_location`/`remove_location`).
_Avoid_: source, mirror, provider; register (that's for Artifacts).

**Tag**:
A `(tag_name, hash)` tuple in the iroh-blobs filesystem store that keeps the data with that hash (which is in the CID) from being garbage collected. A *Seeded Tag* (`seeded/{rid}/{release}/{cid}`) marks content you are seeding. The same CID can have more than one Seeded Tag, for example if you seed the same Artifact in two repos or Releases. During an in-flight retrieval a *temporary tag* protects the data from GC until the retrieval finishes and it gets the Seeded Tag. Not to be confused with a Release's git tag, which lives in radicle git storage and never appears in the blob store.
_Avoid_: pin.

**Dangling Tag**:
A Seeded Tag for a CID that is not an Artifact in any Release. The node seeds the bytes, but peers cannot find them, because a Location can only be added to an Artifact. Reconcile reports it; `unseed` removes it.
_Avoid_: orphan tag (the design doc overloads "orphan" for unrelated cases).

**Orphaned Location** (a.k.a. orphaned-self):
A Location under our own DID, naming our current EndpointId, for a CID the node is no longer seeding — it points peers at us for bytes we don't have. The mirror image of a missing Location, and what `--remove-orphaned[-self]` removes.
_Avoid_: stale location (that's a Stale Endpoint).

**Stale Endpoint**:
A `radiroh://` Location under our own DID that names a previous or undecodable EndpointId. Reconcile reports it.

## Relationships

- A **Release** has one **Creator** and contains zero or more **Artifacts**
- An **Artifact** has one **Author** and zero or more **Locations**, grouped by contributor **DID**
- **Trust** decides which **Releases** and **Artifacts** count; **Trusted Parties** decide which redactions and metadata count
- **Locate** answers, for a **CID**, every **Location** (and every **Release** that holds it) across all repositories in local storage
- A **Seeded Tag** should correspond to an **Artifact** in some **Release**; when it doesn't, it is a **Dangling Tag**
- **Watching** repeats **Fetch** + **Seed** + **Add** for every trusted **Artifact** a peer registers, so a node seeds a repository's Artifacts without being told each **CID**
- **Seeding** and **Adding** a Location are the two halves of making an artifact available over iroh: a node seeds the bytes and adds the `radiroh://` **Location** so peers can discover it; the two drift apart as missing Locations (seeded, no Location) and **Orphaned Locations** (Location added, no longer seeded), which **Reconcile** finds

[COB]: https://radicle.dev/guides/protocol#collaborative-objects
[iroh]: https://docs.iroh.computer/protocols/blobs
[BLAKE3]: https://github.com/BLAKE3-team/BLAKE3
[uri-scheme]: ./docs/uri-scheme.md
