# rad-artifact

Two distinct layers. You **create** a Release tied to a tag/commit,
**register** Artifacts against it, and **add** download Locations, all in a
Radicle collaborative object (COB) in the git storage, synced over the radicle
protocol; discovery metadata, never bytes. **Seeding** is a node holding an Artifact's bytes
and serving them to peers over iroh. The COB says where bytes can be
fetched; a seeding node is what actually answers.

## Language

**Create** (verb):
Open a new signed Release in the COB associated with a commit OID and optionally a tag.
_Avoid_: register (you register into a Release, not the Release itself).

**Register** (verb):
Record an Artifact against a Release in the COB.
_Avoid_: add (the CLI command was renamed from `add` to `register`), publish.

**Seed** (verb):
Hold an Artifact's bytes on a node and serve them to peers over iroh.
Tracked locally by a Seeded Tag; advertised to peers by a `radiroh://`
Location.
_Avoid_: serve/serving, host, mirror (use "seed"/"seeding").

**Seeder**:
A node that seeds an Artifact's bytes. Distinct from a Radicle seed node,
which holds the repo's git/COB; one machine can be both, but "Seeder" here
always means the iroh bytes role.
_Avoid_: host, mirror; "provider" is tolerated only as iroh-blobs' internal
term (its download-side name for a Seeder's endpoint).

**Release**:
A COB entry, keyed by a commit, holding a set of Artifacts for a repository.

**Artifact**:
A named, content-addressed file or collection of files within a Release, identified by its CID.

**CID**:
The BLAKE3 content identifier of an Artifact's bytes.

**Location**:
A URL under a contributor's DID asserting where an Artifact can be fetched; a URL like `radiroh://{endpoint}` iroh endpoint for peer-to-peer fetch. _Added_ and _removed_ (`location add`/`remove`, `add_location`/`remove_location`).
_Avoid_: source, mirror, provider; register (that's for Artifacts).

**Seeded Tag**:
A `seeded/{rid}/{cid}` marker in the node's blob store asserting the node is actively seeding that Artifact's bytes.
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
- **Seeding** by a running a node and adding `radiroh://` **Location** to registered artifact CIDs; the two drift apart as **Dangling Tags** (seeded, no Location added) and **Orphaned Locations** (Location added, no longer seeded)
