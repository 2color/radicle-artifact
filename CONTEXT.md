# rad-artifact

Distributes release artifacts over iroh, peer-to-peer, with discovery
recorded in a Radicle collaborative object. A node seeds bytes; the COB
records where peers can fetch them.

## Language

**Release**:
A COB entry, keyed by a commit, holding a set of Artifacts for a repository.

**Artifact**:
A named, content-addressed file or collection of files within a Release, identified by its CID.

**CID**:
The BLAKE3 content identifier of an Artifact's bytes.

**Location**:
A URL under a contributor's DID asserting where an Artifact can be fetched (typically `iroh://{endpoint}`).
_Avoid_: source, mirror, provider.

**Seeded Tag**:
A `seeded/{rid}/{cid}` marker in the node's blob store asserting the node is actively serving that Artifact's bytes.
_Avoid_: pin.

**Dangling Tag**:
A Seeded Tag whose CID no Release references — so no Location can anchor to it.
_Avoid_: orphan tag (the design doc overloads "orphan" for unrelated cases).

**Orphaned Location** (a.k.a. orphaned-self):
A Location under our own DID, pinned to our current endpoint, for a CID the node is no longer seeding — it points peers at us for bytes we don't have. The mirror image of a Dangling Tag, and what `--retract-orphaned[-self]` removes.
_Avoid_: stale location (a Stale Endpoint is the distinct case where the URL is pinned to a _previous_ or undecodable endpoint).

## Relationships

- A **Release** contains one or more **Artifacts**
- An **Artifact** has zero or more **Locations**, grouped by contributor **DID**
- A **Seeded Tag** should correspond to an **Artifact** in some **Release**; when it doesn't, it is a **Dangling Tag**
