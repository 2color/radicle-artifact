# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.17.0] - 2026-07-09

### ⭐️ Highlights

#### Faster (50x) reads with a cache

Reading releases (both with `rad-artifact list` and the library) no longer re-materializes them from every COB action in git on each call. A new SQLite cache stores each materialized release alongside a normalized index for CID lookups, so state reads become simple queries after a cheap freshness check against the COB's git tips.

Repository-wide operations that previously took hundreds of milliseconds now finish in single-digit milliseconds! The cache is a pure optimization: it validates freshness on every read, re-materializes only the objects whose git tips changed. See [docs/cache-benchmarks.md](docs/cache-benchmarks.md) for the measured speedups.

Counting releases is now cache-free: `Releases::count` walks the COB refs instead of materializing every release just to return a number, reducing what was took ~290 ms on a cold cache down to ~5 ms.

#### Cross-repo artifact lookup with `locate <cid>`

Building on the new cache, the new `rad-artifact locate <cid>` command returns every location for a given CID across every repository in local storage (or, with `--releases`, the releases that contain it) as JSON. It reads only local storage and never touches the network, refreshing one shared cache across all repositories before the lookup so results always reflect the current COB state.

### Added

* `43d66a8` add SQLite cache for Release COB *<daniel@norman.life>*
* `62231e6` add cross-repo CID lookup *<daniel@norman.life>*

### Changed

* `434a058` count releases via ref walk, not materialization *<daniel@norman.life>*
* `c3530bf` add cache read benchmark and results doc *<daniel@norman.life>*
* `9d09ef1` store release timestamp in the blob *<daniel@norman.life>*

### Docs

* `4218d4e` publish workspace README for all crates *<daniel@norman.life>*
* `5136ce3` update CHANGELOG *<daniel@norman.life>*

## [0.16.0] - 2026-07-06

### ⚠️ Breaking changes

JSON output when passing the `--json` flag to the `rad-artifact` cli now follows a single **camelCase** convention, for example `release_id` becomes `releaseId`.

- The conventional metadata key used to store a size hint for artifact `size-bytes` is renamed `sizeBytes`.

- The `metadata` object is always present in `--json` output, empty when unset, so consumers get a stable shape.

The signed replicated COB storage format is deliberately left unchanged.

Artifacts registered before this keep the legacy `size-bytes` key and render their size as a raw integer, with no automatic fallback. See [`docs/adr/0001-json-casing.md`](./docs/adr/0001-json-casing.md) for the rationale.

### Added

* `a27a6fc` use camelCase for JSON and wire output [**breaking**] *<daniel@norman.life>*

### Docs

* `fbdd806` refine readme *<daniel@norman.life>*

## [0.15.1] - 2026-07-01

This is a small release with cosmetic changes to the output when registering artifacts.

### Changed

* `5050c27` **cli:** reduce noise in register output *<daniel@norman.life>*
* `2bf34d9` check host machine before mac compilation *<daniel@norman.life>*
* `541d76a` include chore & docs commits in changelog *<daniel@norman.life>*

### Docs

* `0f680d5` use register command in release flow *<daniel@norman.life>*
* `d824bd0` refine the README *<daniel@norman.life>*
* `d9f1615` add comparison to lfs, annex, and LOP *<daniel@norman.life>*

## [0.15.0] - 2026-06-29

### ⚠️ Breaking changes

This release ships with three breaking changes:

- **`iroh://` → `radiroh://` scheme**: legacy URLs no longer parse and fetch ignores them; run `rad-artifact reconcile --remove-orphaned-self` to migrate your locations in one pass.
- **COB type `org.radworks.artifact` → `dev.radicle.artifact`**: COBs under the old name are no longer found. Recreate releases with the latest version of the CLI.
- **CIDs serialize as base32 in storage and on the wire**: operations written with the old byte-array encoding no longer deserialize. Library consumers: use public `radicle_artifact::Cid` rather than `cid::Cid`.

See the highlights section below for more detail.

### ⭐️ Highlights

#### Workspace split: lean COB crate, separate seeding daemon

The single crate is now a four-crate workspace, so COB-only consumers are
never exposed to the iroh dependency tree:

- `radicle-artifact` — COB types/operations + the `rad-artifact` CLI; no
  iroh, no tokio. The `share` cargo feature is gone.
- `radicle-artifact-node` — the new `rad-artifact-node` daemon binary
  (iroh-blobs store + blob serving). `rad-artifact node start` spawns it
  from next to the CLI binary or `$PATH`; install both binaries.
- `radicle-artifact-core` — shared wire protocol, CID helpers, and
  endpoint identity (over `iroh-base` only).
- `radicle-artifact-client` — control-socket client; sync by default,
  async via its `tokio` feature for embedders.

#### A long-running node for reliable seeding

Peer-to-peer seeding used to mean keeping a one-shot `serve` command running in a terminal, but this was mired with many limitation: you couldn't seed more than one artifact at a time. Moreover, seeding worked only as long as the process.

This release replaces that with a real background node:

```sh
$ rad-artifact node start
Node started (socket: /Users/you/.radicle/artifacts/control.sock)

$ rad-artifact seed ./dist/linux-amd64.tar.gz
Seeded bafkr4id5wrvsbpcw5hbdpcosgyzxt75hosqzj544v6mavnaxmjkypatgme (12.4 MiB, new tagged)
Added radiroh location to release abc1234
```

The node starts once, detaches from your terminal, and keeps serving across shell exits and terminal closes. It reuses your radicle key for the iroh endpoint, so starting it may prompt for your passphrase if the key is encrypted. It holds a persistent iroh-blobs store on disk, so restarts don't re-import or re-hash anything you're already seeding. Check on it with `rad-artifact node status` (endpoint id, seeded count, disk, traffic, and relay status), `rad-artifact node list`, and `rad-artifact node logs --follow`; stop it cleanly with `rad-artifact node stop`, which lets in-flight transfers drain before exiting.

Together this means your published artifacts stay reachable peer-to-peer without you babysitting a foreground process.

#### ⚠️ Rename the location URL scheme to `radiroh://`

The peer-to-peer location scheme is renamed from the invented, unowned `iroh://` to the Radicle-namespaced `radiroh://` (rad issue b93d542). Radicle owns this namespace, so we can specify what the URL means — both peer discovery and the iroh-blobs transfer protocol — without colliding with the iroh project. See [docs/uri-scheme.md](docs/uri-scheme.md) for the grammar.

The host encoding is unchanged: the iroh endpoint id as lowercase base32, no padding (RFC 4648). A bare `radiroh://` still derives the endpoint id from the location author's DID.

This is a **breaking change** on read: legacy `iroh://` URLs are no longer parsed, and fetch ignores them. There is no automatic dual-read; instead, `rad-artifact reconcile --remove-orphaned-self` migrates your locations in a single run. It retracts the legacy URLs and re-adds fresh `radiroh://` URLs.

#### ⚠️ Rename the COB type to `dev.radicle.artifact`

The collaborative object type name is renamed from `org.radworks.artifact` to the Radicle-namespaced `dev.radicle.artifact`, dropping the org name in favour of the project namespace.

The type name is embedded in the signed COB manifest and forms part of the `refs/cobs/<typename>/<id>` ref path, so this is a **breaking change**: COBs created under the old name are no longer found. No migration is provided, so recreate any local releases under the new type.

#### ⚠️ CIDs encoded with base32 in storage

CIDs in COB operations (stored as JSON in git storage) and on the control-socket wire now serialize as their base32 string encoding (`bafk...`) instead of the raw byte array, resulting in more efficiency and consistency across the stack.

This was because the `cid` crate's derived `Serialize` which encodes a `Cid` as a serde byte sequence, and `serde_json` faithfully renders any byte sequence as a JSON array of numbers rather than a string.

This is a **breaking change** for stored COBs: operations written with the old byte-array encoding no longer deserialize. For library consumers it is also a **breaking API change**: the public `radicle_artifact::Cid` type is the newtype rather than `cid::Cid`.

#### Create a release up front with `create`

`rad-artifact create [<revision>]` opens a release for a commit and prints
its id, so a script can create once and register many artifacts into it:

```
id=$(rad-artifact create v1.0)
rad-artifact register ./bin-a --release "$id" -n bin-a
rad-artifact register ./bin-b --release "$id" -n bin-b
```

It is idempotent per author: re-running for the same commit and tag reuses
your existing release instead of minting a duplicate; releases authored by
others are never reused.

#### Seed without saving: `fetch` keeps bytes in the store, `download` writes to disk

Previously the only way to get an artifact was to write it to disk. Now `fetch <CID>` pulls the bytes into the node's store and verifies the CID without touching the filesystem. Useful when you want to seed an artifact for others without cluttering your working directory. `download --cid <CID>` does the same and then saves the file, replacing the old `fetch` disk-export behavior.

```sh
$ rad-artifact fetch v1.0 --cid baf.. --seed
$ rad-artifact download v1.0 --cid baf.. -o ./dist/app.tar.gz
```

#### 🌱 `seed` / `unseed`, with automatic cleanup

`serve` has been renamed to `seed`, with a matching `unseed`:

`rad artifact seed <PATH>` computes the CID for an artifact, hands the bytes to the node, and announces a `radiroh://` location on the release in one step.

`unseed --cid <CID>` stops seeding and removes your node's `radiroh://` locations. Both announce the COB change to the network when they write one (like the other mutating commands) so peers discover. The node also runs periodic blob garbage collection, so space from unseeded artifacts is reclaimed automatically rather than growing without bound.

#### 🌱 Seeding is scoped per release

The same file can ship in more than one release (a release candidate promoted to a final release, say), and those releases reference the same CID. The node now tracks what it seeds per `(repo, release, CID)` rather than per `(repo, CID)`, so the bytes stay protected as long as any release still references them.

In practice this means `unseed` is release-aware: with `--release <id>` it stops seeding just that release's copy and leaves the others serving. Without `--release`, a CID that lives in more than one release prompts you to pick one (or "All releases") at a terminal, and falls back to sweeping every release in scripts (`--no-input`) or when the CID is in only one release. The shared blob on disk is only garbage-collected once the last release referencing it is unseeded. `seed` and `download --seed`/`fetch --seed` likewise tag under the release they announce the location to, keeping the seeded tag and the COB location in sync.

#### 🌱 See which artifacts you've announced at a glance

`list` and `show` now mark artifacts you've announced as a seeder with 🌱, derived from the COB and your DID, no node RPC required, so it reflects what the network sees even when the node is offline.

#### 🤝 Keep your locations honest with `reconcile`

`rad-artifact reconcile` brings the artifact COB back in line with what your node is actually seeding. It auto-adds missing `radiroh://` locations for artifacts you're serving, flags drift in the other direction (locations you left behind, stale endpoint ids) without deleting anything until you ask, and reports **dangling tags** — CIDs the node is seeding that no release references. Pass `--remove-orphaned <CID>` or `--remove-orphaned-self` to prune explicitly, and `--all-repos` to sweep everything at once. This is also the supported one-run migration off the legacy `iroh://` scheme (see below).

#### Fetching now goes through the node - groundwork for desktop

`fetch` no longer spins up a throwaway iroh endpoint of its own; it routes through the running node over a typed control-socket protocol, reusing the node's persistent store and connections. The same protocol exposes `has`, `fetch`, and `export` operations with streaming progress. Beyond making fetches faster and more reliable, this establishes the node as the single long-lived process that future clients — including the planned Radicle desktop integration — can talk to over a stable local interface, rather than each shelling out to the CLI.

#### Rename `add` to `register`

The CLI command `add` is renamed to `register`, drawing a clear line between **Registering** artifacts and download location synced over the radicle protocol (discovery metadata, never bytes) and **Seeding**, the node holding the artifact's bytes and seeding them to peers over iroh.

`add` stays as a hidden alias, so existing scripts and pipelines keep working.

`register <PATH> --seed` registers and seeds in a single step: it reuses the CID computed during registration to hand the bytes to the node and announce a `radiroh://` location, so the artifact is hashed once instead of twice and the common publish flow drops from two commands to one. Requires a running node and a local path (it conflicts with `--cid`).

`register --json` emits `{cid, release_id, revision}` on stdout instead of the human-readable summary, so a script can capture the release id and CID — for example to drive a later `--release <id>` call — without re-deriving the CID or scraping stderr.

For library consumers this is a **breaking API change**: `Release::add_artifact` is now `register_artifact`, and the COB action `Action::AddArtifact` is now `Action::RegisterArtifact`. The on-the-wire format is unchanged — the action still serializes as `AddArtifact` via `#[serde(rename)]`, so existing COBs deserialize as before and no migration is needed.


#### Multiple iroh relays via `IROH_RELAY_HOSTS`

The ability of nodes to successfully fetch artifacts in a peer-to-peer fashion depends on iroh's ability to establish either a direct connection or a relayed. This process is facilitated by a "dumb" third *relay* server that helps the node with [QUIC address discovery](https://www.iroh.computer/blog/qad) and relaying (the equivalent of STUN and TURN in WebRTC parlance).

The **`IROH_RELAY_HOSTS`** environment variable (previously `IROH_RELAY_URL`) now accepts a comma-separated list of relay hosts, so deployments can point the node at more than one relay for redundancy. Each host is served over `https://`, so the scheme is no longer repeated per entry. The default is now `eu-1.relay.iroh.radicle.garden`.

> *Note:* an endpoint may be connected to multiple relay servers, but it will advertise its home relay endpoint as the one best used to hole-punch or relay packets through. For more information, see the [iroh relay docs](https://github.com/n0-computer/iroh/blob/main/iroh/docs/relays.md).

#### Encrypted peer discovery over pkarr

`radiroh://` iroh endpoints are now resolved over HTTPS with the pkarr server instead of unencrypted DNS over UDP. The node previously resolved peers via DNS TXT queries, so endpoint discovery had no encrypted path; only publishing used HTTPS.

The `IROH_DNS_ENDPOINT_ORIGIN` environment variable which would configure the DNS server for used resolution has been removed. This means that reolving the relays and pkarr publishing address relies on the system DNS configuration.

#### Record a `size-bytes` hint on register

Registering an artifact from a local `<PATH>` now also records a `size-bytes` metadata entry, so peers can get a hint about an artifact's size before fetching.

Pass `--no-size` to skip it. Registering by `--cid` records no size since there are no local bytes to measure.

### Added

* `9731329` introduce seeder module, base32 endpoint ids *<daniel@norman.life>*
* `ec2f7c3` add seeder primitives with per-repo tag scoping *<daniel@norman.life>*
* `8af7a58` add control-socket protocol types *<daniel@norman.life>*
* `eeba35c` add control-socket client *<daniel@norman.life>*
* `b306c26` **seeder:** add all_seeded helper *<daniel@norman.life>*
* `460dff4` add foreground node daemon *<daniel@norman.life>*
* `e3f0bdd` **node:** add parent-side lifecycle helpers *<daniel@norman.life>*
* `3a05898` **node:** log via the log crate facade *<daniel@norman.life>*
* `ef4fd2c` wire rad-artifact node CLI surface *<daniel@norman.life>*
* `70be783` rename serve to seed and add top-level unseed *<daniel@norman.life>*
* `456b81c` add rad-artifact reconcile *<daniel@norman.life>*
* `3800a6b` **node:** match pretty status to --json output *<daniel@norman.life>*
* `b95b7b3` **reconcile:** broaden retraction & ouput *<daniel@norman.life>*
* `754a407` **node:** derive CID from path in `node seed` *<daniel@norman.life>*
* `a6e4755` **fetch:** show https/iroh breakdown in trying summary *<daniel@norman.life>*
* `1e37268` **share:** make iroh configurable via env *<daniel@norman.life>*
* `88216b4` **reconcile:** report dangling seeded tags *<daniel@norman.life>*
* `dcaf2b6` **seeder:** enable periodic blob GC *<daniel@norman.life>*
* `acae5ba` **protocol:** add InvalidRequest code for malformed wire input *<daniel@norman.life>*
* `d3dd21f` **reconcile:** sweep legacy iroh:// URLs under our DID *<daniel@norman.life>*
* `64c2a60` **protocol:** add fetch/export/has wire types *<daniel@norman.life>*
* `95f331c` **node:** add NodeCtx and Has/Export handlers *<daniel@norman.life>*
* `8926b2b` **node:** implement the streaming Fetch handler *<daniel@norman.life>*
* `6127a5f` **client:** add has and streaming fetch/export methods *<daniel@norman.life>*
* `b2d82f0` **cli:** route fetch through the node *<daniel@norman.life>*
* `27a2462` **node:** wire iroh connection and traffic stats *<daniel@norman.life>*
* `89aefb9` **node:** add cheap Alive command for liveness *<daniel@norman.life>*
* `ae49531` **cli:** add `register --seed` to register and seed in one step *<daniel@norman.life>*
* `817e3f9` **cli:** add `--json` to register *<daniel@norman.life>*
* `fea2dc0` support multiple relay URLs via IROH_RELAY_URLS *<daniel@norman.life>*
* `f8e2211` **node:** surface relay health in status *<daniel@norman.life>*
* `8a545e7` **cli:** warm up node command output *<daniel@norman.life>*
* `c8afba4` **display:** mark seeded artifacts with a seedling *<daniel@norman.life>*
* `da62177` split Fetch and Download *<daniel@norman.life>*
* `f0e9ad9` allow fetch/download with --cid only *<daniel@norman.life>*
* `909bf6e` add --offline export to download *<daniel@norman.life>*
* `641ebd7` add radicle-artifact-client crate *<daniel@norman.life>*
* `2de6d82` resolve iroh peers over encrypted pkarr *<daniel@norman.life>*
* `8eb646b` **cli:** prompt to scope unseed across releases *<daniel@norman.life>*
* `9169d33` **cli:** record size-bytes hint on register *<daniel@norman.life>*
* `839ee2b` **cli:** show size-bytes hint human-readably *<daniel@norman.life>*
* `16da24e` **cli:** add create command for releases *<daniel@norman.life>*

### Changed

* `49e828d` **bin:** extract node CLI into its own module *<daniel@norman.life>*
* `8e69822` **seeder:** use multibase base32 lowercase for endpoint ids *<daniel@norman.life>*
* `d2f2130` consolidate iroh:// URL helpers into share::iroh_url *<daniel@norman.life>*
* `1f6d1e8` **node:** switch logging from log to tracing *<daniel@norman.life>*
* `a4a23e7` **keys:** consolidate iroh URL handling into EndpointId *<daniel@norman.life>*
* `24ae368` **keys:** move keys module from seeder to share *<daniel@norman.life>*
* `c39d863` **share:** rename endpoint module to iroh *<daniel@norman.life>*
* `70acf95` **reconcile:** rename --retract-orphaned to --remove-orphaned *<daniel@norman.life>*
* `28eab99` **protocol:** type endpoint_id as EndpointId *<daniel@norman.life>*
* `3f59e96` **seeder:** hold temp tag across persistent tag set *<daniel@norman.life>*
* `0a623be` remove unneeded ref *<daniel@norman.life>*
* `405222b` **seeder:** use binary tag-name encoding *<daniel@norman.life>*
* `8e3893b` **seeder:** length-prefix the RID inside seeded tag names *<daniel@norman.life>*
* `3fc5947` **protocol:** type rid as RepoId on the wire *<daniel@norman.life>*
* `5e8e09e` **protocol:** type cid as Cid on the wire *<daniel@norman.life>*
* `f1c16ff` **share:** rename URL scheme from iroh to radiroh *<daniel@norman.life>*
* `9bfc742` **fetch:** extract reusable download and export core *<daniel@norman.life>*
* `f90d141` **fetch:** remove the standalone ephemeral fetch path *<daniel@norman.life>*
* `1e85464` **share:** drop unused Error variants *<daniel@norman.life>*
* `4d9527f` **node:** simplify run_stream signature with AsyncFnOnce *<daniel@norman.life>*
* `ba4ff95` **seeder:** drop redundant tag lookup in artifact_size *<daniel@norman.life>*
* `8c10fba` **status:** drop unused did_locations_unmatched warning *<daniel@norman.life>*
* `812044d` **cli:** rename add command to register *<daniel@norman.life>*
* `21cc400` rename AddArtifact to RegisterArtifact *<daniel@norman.life>*
* `c7cfb0f` **display:** centralize FetchProgress rendering *<daniel@norman.life>*
* `74394f2` **client:** extract run_blocking *<daniel@norman.life>*
* `ddcb763` rename to tag *<daniel@norman.life>*
* `c6b9a75` use simpler api to check for blobs *<daniel@norman.life>*
* `d0d5615` extract log_retrieval_plan helper *<daniel@norman.life>*
* `846d1c4` make unseed accept cid as a flag *<daniel@norman.life>*
* `36be9ba` drop disk stats from node status *<daniel@norman.life>*
* `e812490` split announce into add and announce *<daniel@norman.life>*
* `98aca4e` move crate into cargo workspace layout *<daniel@norman.life>*
* `ef4e879` extract radicle-artifact-core crate *<daniel@norman.life>*
* `8437d1d` split node daemon out of radicle-artifact *<daniel@norman.life>*
* `9a5c9f1` fold seed flag and release into one option *<daniel@norman.life>*
* `7ff869a` **seeder:** short-circuit is_seeded_any scan *<daniel@norman.life>*
* `b61fcac` rename cob type to dev.radicle.artifact [**breaking**] *<daniel@norman.life>*

### Fixed

* `18f7950` **node:** print full endpoint id in status *<daniel@norman.life>*
* `4c6a7ec` **seed:** canonicalise path before sending to node *<daniel@norman.life>*
* `82deac3` **reconcile:** treat undecodable hosts as stale *<daniel@norman.life>*
* `1466e9f` **reconcile:** continue --all-repos past a failing repo *<daniel@norman.life>*
* `adda642` **node:** kill orphaned daemon on startup timeout *<daniel@norman.life>*
* `3f85b6c` **seed:** warn user if location register fails *<daniel@norman.life>*
* `4f61111` **seeder:** await relay connectivity before serving *<daniel@norman.life>*
* `aff3cf7` **seeder:** use temp tags for imports to avoid leaks *<daniel@norman.life>*
* `08e3c8b` **node:** let in-flight imports drain before shutdown *<daniel@norman.life>*
* `356cf0c` **node:** bound control-socket command read *<daniel@norman.life>*
* `5f5bf68` **node:** subscribe to shutdown before signal handler *<daniel@norman.life>*
* `1f53e86` **cli:** reject legacy iroh:// at location add and fix fetch summary *<daniel@norman.life>*
* `b0d6b0f` **fetch:** make collection export safe and atomic *<daniel@norman.life>*
* `9220b0b` **fetch:** robust temp-file cleanup for export and HTTP *<daniel@norman.life>*
* `d2a5b36` **node:** protect fast-path fetch bytes with a temp tag *<daniel@norman.life>*
* `2158775` stream progress and detect client disconnect promptly *<daniel@norman.life>*
* `8ecede5` **protocol:** mark all response payload structs non_exhaustive *<daniel@norman.life>*
* `a4cce12` use register over add in user output *<daniel@norman.life>*
* `fd297b1` **cli:** announce COB changes after seed/unseed *<daniel@norman.life>*
* `2e6002a` **fetch:** resolve output path against CLI cwd *<daniel@norman.life>*
* `64cdc34` rename revision to oid in register output *<daniel@norman.life>*
* `bf7c35d` use announce_refs_for for ref broadcast *<daniel@norman.life>*
* `7750b40` **seeder:** key seeded tags by release *<daniel@norman.life>*
* `c7d139f` **seeder:** remove TOCTOU in single-release unseed *<daniel@norman.life>*
* `93a45e4` **seeder:** bound untag_all to a snapshot of releases *<daniel@norman.life>*
* `1d36efd` encode CIDs as multibase strings [**breaking**] *<daniel@norman.life>*

### Other

* `5d7bd4d` add buildkite pipeline *<daniel@norman.life>*
* `7eb2094` add cargo doc to buildkite pipeline *<daniel@norman.life>*
* `2739576` update README for the node + seed/unseed flow *<daniel@norman.life>*
* `6f27193` edit for clarity *<daniel@norman.life>*
* `21a5ba9` update reconcile flags and note dangling tags in README *<daniel@norman.life>*
* `81acca1` **seeder:** cover all_seeded decode and tag-name layout *<daniel@norman.life>*
* `5764254` update URL scheme in integration tests *<daniel@norman.life>*
* `47e4baf` update iroh:// references to radiroh:// *<daniel@norman.life>*
* `2d6fd91` rename scheme in README and CONTEXT *<daniel@norman.life>*
* `fe467f6` **changelog:** record radiroh scheme rename and reconcile sweep *<daniel@norman.life>*
* `6efd17e` add radiroh:// URI scheme spec *<daniel@norman.life>*
* `df4ef15` correct migration for urls *<daniel@norman.life>*
* `8854385` **changelog:** record the long-running node and seeding flow *<daniel@norman.life>*
* `e970d1f` fix broken intra-doc links *<daniel@norman.life>*
* `c59c434` **fetch:** cover HTTP import, collection export, cleanup *<daniel@norman.life>*
* `83192c5` **node:** cover disconnect abort, stale socket, error paths *<daniel@norman.life>*
* `bf23a86` **node:** clarify GC re-download window is low risk *<daniel@norman.life>*
* `32a86d2` run clippy, test, doc in parallel *<daniel@norman.life>*
* `ef1c082` clarify register vs seed distinction *<daniel@norman.life>*
* `21987a4` **changelog:** note add to register rename *<daniel@norman.life>*
* `6982b76` **cli:** clarify seed tie-break & improve output *<daniel@norman.life>*
* `7990185` add note about key reuse *<daniel@norman.life>*
* `27fbaf4` document relationship to radicle node *<daniel@norman.life>*
* `616350d` use language consistently and coherently *<daniel@norman.life>*
* `674d9bc` add make check target *<daniel@norman.life>*
* `5bf6dee` define Temp Tag in CONTEXT.md *<daniel@norman.life>*
* `7c9d369` use ubiquitous language consistently *<daniel@norman.life>*
* `4703bfc` reorg README and correct inaccuracies *<daniel@norman.life>*
* `731c8a9` refine glossary for clarity *<daniel@norman.life>*
* `bf56ce5` update release plumbing for workspace split *<daniel@norman.life>*
* `89303ca` corrections for accuracy and rust doc fixes *<daniel@norman.life>*
* `be7c372` fix protocol/logging/release doc nits *<daniel@norman.life>*
* `ad08276` **changelog:** note per-release seeding scope *<daniel@norman.life>*
* `7a71f64` add git https url for direct git dependency *<daniel@norman.life>*
* `ee347ca` revisions need to be in radicle storage *<daniel@norman.life>*
* `ace756c` update changelog *<daniel@norman.life>*

## [0.14.0] - 2026-05-12

### ⭐️ Highlights

#### Validate metadata keys and value size

`ReleaseMut::set_metadata` now rejects malformed entries before they enter the COB log, so a bad input from one node never has to be replayed by every other peer.

Keys must be non-empty, at most `MAX_METADATA_KEY_LEN` (256) bytes, and free of control characters (newlines, tabs, NUL, etc.). Values are capped at `MAX_METADATA_VALUE_LEN` (8 KiB) of serialized JSON — large enough for typical build provenance or SBOM summaries.

Each rule surfaces as a dedicated `error::Metadata` variant (`EmptyKey`, `KeyTooLong`, `KeyControlChar`, `ValueTooLarge`) so callers can act on the specific failure. COB replay itself stays permissive, preserving deterministic application across nodes.

#### Honour endpoint id in `iroh://` URLs

The `rad-artifact` CLI currently reuses your radicle ed25519 key as the key for creating the iroh endpoint. That meant every iroh location in an artifact COB was pinned to its author's radicle identity, i.e. the endpoint id was always derived from the signer's DID for location URLs with the `iroh://` url scheme.

This change honors the endpoint id in `iroh://<endpoint-id>` URLs when present, while still supporting derivation from DID for bare `iroh://` urls.

Practically, this means that fetching works for `iroh://...` endpoints not derived from the radicle key, and consumers of this crate can choose whether to reuse the radicle key, or create a separate key for the iroh sharing.

Note that [endpoint address discovery](https://docs.iroh.computer/concepts/discovery) still goes through the [Radworks DNS server](src/share/endpoint.rs).

### Added

* `ba7331d` parse endpoint id from iroh:// URL *<daniel@norman.life>*
* `b6e4ec5` validate metadata keys *<daniel@norman.life>*
* `e1dd25b` cap metadata value size at 8 KiB *<daniel@norman.life>*

## [0.13.0] - 2026-05-11

### ⭐️ Highlights

#### Add metadata to artifacts

Artifacts can now carry JSON metadata, useful for recording the build environment, [SLSA metadata](https://slsa.dev/), reproducibility flags, or anything else downstream tooling cares about. Only the artifact's author or a current repository delegate can write. The metadata keyspace is shared and last-writer-wins.

Set a string value:

```sh
$ rad-artifact metadata set build-env "nix --pure"
```

Pass `--json` to parse the value as JSON instead of storing it as a string:

```sh
$ rad-artifact metadata set --json size_bytes 1048576
```

```sh
$ rad-artifact metadata set --json reproducible true
```

Without `--revision`/`--release` and `--cid`, the command will drop into an interactive release artifact picker.

Remove an entry with `rad-artifact metadata unset <key>`.

In `--json` output from `list`/`show`, metadata renders as a flat object (`"metadata": {"size_bytes": 1048576, ...}`).

#### Releases scoped by author

Release visibility now follows the same rule as artifacts: by default `list`, `show`, and the `<revision>` lookup used by every mutating command consider only releases authored by a repository delegate or by the local user. Releases from other users are skipped.

Pass `--all-authors` to widen the set:

```sh
$ rad-artifact list --all-authors
$ rad-artifact show --all-authors v1.0
$ rad-artifact attest --all-authors v1.0 --cid baf..
```

`--all-authors` has been added to `add`, `attest`, `redact`, `location add`, `location remove`, and `metadata set`/`unset`. Targeting a specific release directly with `--release <id>` continues to work regardless of who authored it.

This means that when multiple users have created releases for the same commit, the local user's own releases are now always visible without `--all-authors`.

### Added

* `14d78b1` **release:** validate tag OID on create *<daniel@norman.life>*
* `ee55697` add free-form metadata to artifacts *<daniel@norman.life>*
* `30e971d` store metadata values as JSON *<daniel@norman.life>*
* `91d6a91` **cli:** add --json flag to metadata set *<daniel@norman.life>*

### Changed

* `cbfed1b` **cli:** tighten duplicate-release UX helpers *<daniel@norman.life>*
* `988a612` address clippy comments *<daniel@norman.life>*

### Fixed

* `00b90d8` **display:** emit metadata as flat JSON object *<daniel@norman.life>*
* `99a148f` **cli:** improve duplicate-release UX *<daniel@norman.life>*
* `b92918b` **cli:** box large Metadata error variant fields *<daniel@norman.life>*
* `d125bc7` scope release lookup by author *<daniel@norman.life>*

### Other

* `d858587` update CHANGELOG *<daniel@norman.life>*
* `8c80fc7` update CHANGELOG *<daniel@norman.life>*
* `f0b724d` update README and CHANGELOG *<daniel@norman.life>*


## [0.12.0] - 2026-04-29

### ⭐️ Highlights

#### Restructured, colorized output

`list` and `show` have been redesigned to be easier to scan. List output now uses a bullet header per release with badges for attestations (✓) and redactions (⊘).

Color is enabled automatically when stdout is a TTY and respects `NO_COLOR`. CIDs always render in full so they can be copied without re-running with `--verbose`.

![cli-output](public/cli-output.png)

#### Less typing for `attest`, `redact`, and `location`

These commands now drop into a release/artifact picker when called without flags, so you don't have to look up release IDs or CIDs up front. Pass `--revision`/`--release` and `--cid` to skip the prompts in scripts.

![attest and redact demo](./public/attest-and-redact.gif)

#### One commit, many releases

With radicle-artifact, _artifacts_ are grouped into a _release_ linked to a commit. For example, when this crate is released, we pre-compile `rad-artifact` for 4 targets (x86_64, ARM64, Linux, macOS) resulting in a release with 4 artifacts. Releases, like commits, are identified by a Git object ID (like commit hashes).

Previously, releases were linked to commits and _seemingly_ had a one-to-one relationship. This design embraced a simple mental model: users didn't need to think about releases, only commits. However, the reality was more complex: two users concurrently creating a release for the same commit would end up with a different release ID. To avoid surfacing this to the user, we'd transparently union artifacts from all releases tied to a commit.

What started as a simple design ended up achieving the opposite, pushing the complexity from the data model to the implementation, e.g. a tie-breaker function was introduced to deterministically pick when the same commit had multiple releases.

From now on, commits have an explicit `1:n` relationship to releases, and releases can be additionally linked to an annotated tag OID. This means that a single commit can have more than one release, and that releases can be explicitly linked to a [canonical reference](https://radicle.xyz/2025/08/12/canonical-references), inheriting their multi-delegate trust properties.

Another aspect of this change is that the author (the radicle `did:key`) of a release is explicit and visible.

To illustrate what this looks like, consider a release process whereby a release candidate `1.1.0-rc1` is promoted to release `1.1.0`. The commit is the same, but a new tag is created, and the resulting artifacts hash changes. In such cases, it's useful to distinguish between the two releases, because even though they are from the same commit, their artifacts are different:

![diagram](public/diagram.svg)

For a smooth UX, CLI commands that interact with releases, e.g. `add`, have a new interactive prompt to select the release (or create a new one) when TTY is available. There's also a new `--release` flag to target a specific release ID for scripts and environments without stdin.

### Added

* `6796786` add optional tag OID to Release schema *<daniel@norman.life>*
* `b04d6be` record COB creator on Release *<daniel@norman.life>*
* `c0e2958` delegate-priority canonical COB selection *<daniel@norman.life>*
* `acf7cc8` delegate-priority lookup in find_unique_by_oid *<daniel@norman.life>*
* `479cd44` resolve refs to (commit, optional tag) pair *<daniel@norman.life>*
* `58dd5a7` surface tag association in release display *<daniel@norman.life>*
* `55899e0` add --release flag and disambiguation picker *<daniel@norman.life>*
* `88e573e` prompt on single-release tag mismatch *<daniel@norman.life>*
* `b98035f` surface tag name and creator in display *<daniel@norman.life>*
* `98e9c91` extend --release flag to show/attest/redact/location *<daniel@norman.life>*
* `b9ed5ae` prompt to pick release on ambiguous revision lookup *<daniel@norman.life>*
* `b6c4673` show every release for a revision *<daniel@norman.life>*
* `1e72006` show local user's artifacts and full CIDs *<daniel@norman.life>*
* `2f0e6b5` expand artifact location URLs in show -v *<daniel@norman.life>*
* `7158e0e` register artifacts in release COB *<daniel@norman.life>*
* `3259742` **display:** colorize and restructure list/show output *<daniel@norman.life>*
* `bf3b140` interactive add/remove [**breaking**] *<daniel@norman.life>*

### Changed

* `09680ca` rename CLI commit args to revision *<daniel@norman.life>*
* `9876154` drop find_or_create_by_oid; rename to by_commit *<daniel@norman.life>*
* `4e7289f` drop creation date from list/show output *<daniel@norman.life>*
* `c86237d` simplify *<daniel@norman.life>*
* `9567a43` drop redundant build in register-artifacts *<daniel@norman.life>*
* `feecc88` DRY up the Makefile *<daniel@norman.life>*

### Fixed

* `41ab3b0` prefix release IDs with release *<daniel@norman.life>*
* `e2e9e72` accept short release ids on --release flag *<daniel@norman.life>*
* `d33a513` improve HTTP collection fetch error *<daniel@norman.life>*
* `c555a60` rad-artifact add commands in Makefile *<daniel@norman.life>*
* `368c482` **display:** always render full CIDs *<daniel@norman.life>*

### Other

* `1af9b4f` add installation instructions *<daniel@norman.life>*
* `35a619c` cover tag field, creator, and delegate priority *<daniel@norman.life>*
* `d8d7319` tighten comments and remove stale notes *<daniel@norman.life>*
* `5ecab5f` refine README *<daniel@norman.life>*
* `da0b597` document artifact types *<daniel@norman.life>*
* `a4a11d1` **display:** add render snapshot for compact and detailed output *<daniel@norman.life>*
* `285df6c` parse conventional commits in cliff.toml *<daniel@norman.life>*
* `699a6e2` use conventional commits for releases *<daniel@norman.life>*
* `f7589bb` update changelog *<daniel@norman.life>*


## [0.11.0] - 2026-04-24

Small release fixing a regression introduced in `0.10.0` causing the the `serve` and `fetch` commands to fail creating an endpoint.

### Added

* `15f1c1b` add toy CI plan for Ambient to see if this can work at all *<liw@liw.fi>*

### Fixed

* `f7237fa` fix: iroh endpoint binding *<daniel@norman.life>*

### Other

* `7f6ddfa` build: chmod latest to 0644 before upload *<daniel@norman.life>*
* `0cfc62c` ci: add cargo fmt and test to ambient *<daniel@norman.life>*
* `d845dfc` chore: run cargo fmt *<daniel@norman.life>*

## [0.10.0] - 2026-04-24

This release brings a number of UX improvements to the `rad-artifact` cli, in addition to some improvements to the build and release process.

### ✨ Highlights

#### Streamlined artifact publishing with `rad-artifact add <PATH>`

You can now publish artifacts with `rad-artifact add <PATH>` and will be prompted to pick the commit/tag OID to which the artifact will be added. The CID is computed automatically, and if a release doesn't exist already, it will be created automatically.

You can still set the CID and commit/tag manually using `--cid` and `--commit`.

#### Delegate-only listing in `list` and `show` by default

`list` and `show` now hide non-delegate artifacts by default. The former `--delegates-only` flag is replaced by `--all-authors`, which widens the view back.

#### More informative pretty output in `rad-artifact list`

The artifact author's DID now sits alongside the CID of artifact so ownership is visible, and per-location rows are collapsed into a compact `scheme: count` summary so tables stay tight when an artifact is seeded from many endpoints.

### Added

* `4e571eb` Address cargo clippy warnings *<daniel@norman.life>*

### Other

* `15a6efa` Bump cid and multihash due to core2 getting yanked *<daniel@norman.life>*
* `db3a807` Bump iroh dependencies *<daniel@norman.life>*
* `8864895` cli: interactive add with path or CID source *<daniel@norman.life>*
* `bd11134` cli: show artifact author & location counts *<daniel@norman.life>*
* `0ed15ac` cli: default to only showing delegate artifacts *<daniel@norman.life>*
* `3b86140` docs: rewrite README intro *<daniel@norman.life>*
* `b48358d` cli: use commit OID in add examples *<daniel@norman.life>*
* `f7a4921` Bump radicle to 0.23.0 *<daniel@norman.life>*
* `bff8110` build: add install script *<daniel@norman.life>*
* `d7e0f06` build: update links to new radicle urls *<daniel@norman.life>*
* `818cdf7` build: fix make upload pre-check and docs drift *<daniel@norman.life>*
* `41900e2` build: support hand-written changelog notes *<daniel@norman.life>*
* `94857a2` build: add make changelog target *<daniel@norman.life>*
* `833150f` docs: update changelog *<daniel@norman.life>*
* `06be413` build: fix test compilation via radicle-oid qcheck *<daniel@norman.life>*


## [0.9.0] - 2026-04-21

### Other

* `7feb018` Improve README introduction *<daniel@norman.life>*
* `df344a4` Make release identity OID-only, not author-scoped *<daniel@norman.life>*
* `2bfdb35` Converge writes on duplicate releases per OID *<daniel@norman.life>*
* `26c4dfd` cli: add location to one release in serve cmd *<daniel@norman.life>*
* `69e1b20` Refine release pretty print output *<daniel@norman.life>*
* `f355e30` fetch: fail fast on missing or unreachable sources *<daniel@norman.life>*

## [0.8.0] - 2026-04-16

### Added

* `4a48ad2` Add rad-fetch CLI and fetch library *<daniel@norman.life>*
* `55dc5ee` Add artifact lookup helpers *<daniel@norman.life>*
* `c63045f` Add cid subcommand to rad-share *<daniel@norman.life>*
* `7c1a73c` Add list filtering and redaction hiding *<daniel@norman.life>*
* `cc4ff1a` Add commit titles to release display *<daniel@norman.life>*
* `ba087f1` Add --no-input flag and TTY check *<daniel@norman.life>*
* `4ca0640` Add examples to subcommand help text *<daniel@norman.life>*
* `70ce67f` Add --verbose to show and list commands *<daniel@norman.life>*
* `fd29aa3` Add interactive mode to attest and redact *<daniel@norman.life>*

### Changed

* `8b3bbe9` Rename fetch crate to share, add serving *<daniel@norman.life>*
* `89a05cb` Replace spaces with underscores in output name *<daniel@norman.life>*
* `31f4067` Change redact reason to --reason/-m flag *<daniel@norman.life>*
* `670afdd` Move release lookup methods to library *<daniel@norman.life>*

### Fixed

* `9589cf8` Fix iroh endpoint dropped during fetch *<daniel@norman.life>*
* `5eb4fc5` Fix failure to export after successful fetch *<daniel@norman.life>*
* `347e14b` Fix location remove failing for non-delegates *<daniel@norman.life>*
* `8eb89ed` Fix attest/redact failing for non-delegates *<daniel@norman.life>*
* `76e7cfa` Fix redacted/attested row over-indentation *<daniel@norman.life>*

### Other

* `066bf90` Use BLAKE3 CIDs and DID-derived endpoints *<daniel@norman.life>*
* `0c0d356` Show DID aliases in release display *<daniel@norman.life>*
* `707c620` Improve CLI help text and DID display *<daniel@norman.life>*
* `f4b8c92` Hide empty releases by default in list *<daniel@norman.life>*
* `523a543` Derive iroh endpoint ID from location DID *<daniel@norman.life>*
* `3036cb0` Include CID in default fetch output name *<daniel@norman.life>*
* `b2035fd` Improve fetch output and endpoint cleanup *<daniel@norman.life>*
* `589c853` Show short OID and commit title in picker *<daniel@norman.life>*
* `3bd45f3` Auto-create release COB on artifact add *<daniel@norman.life>*
* `8d848d5` Merge share crate into main crate *<daniel@norman.life>*
* `8fa325f` Print confirmation on mutating commands *<daniel@norman.life>*
* `f1a00d7` Auto-detect TTY for output format *<daniel@norman.life>*
* `f90fe10` Suggest next commands after mutations *<daniel@norman.life>*
* `11a0fb3` Color ERROR prefix red in terminal *<daniel@norman.life>*
* `4f4510e` Enforce fetch both-or-neither at parse time *<daniel@norman.life>*
* `5f5340f` Stream iroh downloads to disk with progress *<daniel@norman.life>*
* `c9155d3` cli: prefer flags in place of positional args *<daniel@norman.life>*
* `dd92c98` cli: accept shorthand commits via revparse *<daniel@norman.life>*
* `02e5bf2` Scope release lookups to delegates *<daniel@norman.life>*
* `89e5cb5` Improve list command output *<daniel@norman.life>*
* `751e3cc` Sort interactive fetch picker newest-first *<daniel@norman.life>*
* `ae39372` Prompt for passphrase on encrypted keys *<daniel@norman.life>*
* `30e26df` Narrow prompt module error types *<daniel@norman.life>*
* `044b306` Use inquire::Select for interactive picker *<daniel@norman.life>*
* `5f402ee` Render first & last 6 chars of cids in pretty mode *<daniel@norman.life>*
* `5b02745` Default endpoint preset to Radworks relay *<daniel@norman.life>*
* `142c49c` Refine serve command behavior *<daniel@norman.life>*
* `ed3e84c` Refine README *<daniel@norman.life>*
* `250d934` Use FsStore and rename cid module *<daniel@norman.life>*
* `704a56e` Stream hashing in compute_content_id *<daniel@norman.life>*
* `d8562e5` Release radicle-artifact version 0.8.0 *<daniel@norman.life>*

### Removed

* `5872d97` Drop tempfile runtime dep, use manual dirs *<daniel@norman.life>*
* `e1fb33b` Remove unused Fetcher trait *<daniel@norman.life>*

## [0.7.0] - 2026-03-25

### Other

* `a0d1544` Document how the COB is implemented *<daniel@norman.life>*
* `208f75b` Make self-attestation by artifact author no-op *<daniel@norman.life>*
* `4053ff6` Bump radicle crate to 0.22.1 *<daniel@norman.life>*
* `96251f1` Release radicle-artifact version 0.7.0 *<daniel@norman.life>*

## [0.6.0] - 2026-03-23

### Added

* `22ae509` Add redact for marking artifacts as compromised *<daniel@norman.life>*
* `e4f31a0` Add author to artifacts and allow name updates *<daniel@norman.life>*

### Changed

* `caf8d6c` Update README for artifact author and redactions *<daniel@norman.life>*

### Fixed

* `51ba100` Fix redact doc comment: max reason is 2048 bytes *<daniel@norman.life>*

### Other

* `16e4976` Document redact action in README *<daniel@norman.life>*
* `d675cd8` Clarify user vs. node and adapt explanations *<daniel@norman.life>*
* `6669b53` Prevent attestation after redaction for same DID *<daniel@norman.life>*
* `d0cd174` Release radicle-artifact version 0.6.0 *<daniel@norman.life>*

## [0.5.0] - 2026-03-19

### Changed

* `fe158fa` Rename NodeLocation to Location with did field *<daniel@norman.life>*

### Other

* `6951caa` Document the collaboration model *<daniel@norman.life>*
* `69c8717` Document build-system agnosticism *<daniel@norman.life>*
* `a42215c` Use Did instead of NodeId *<daniel@norman.life>*
* `ff843e7` Use user instead of node for consistency *<daniel@norman.life>*
* `871956b` Allow nodes to add multiple locations per artifact *<daniel@norman.life>*
* `6315178` Refine multiple locations and tighten tests *<daniel@norman.life>*
* `0d208d2` Stringify CIDs when rendering json *<daniel@norman.life>*
* `957f20f` Release radicle-artifact version 0.5.0 *<daniel@norman.life>*

## [0.4.0] - 2026-03-17

### Added

* `2d842fb` Add attestation support for artifact verification *<daniel@norman.life>*
* `c344c64` Add note about project in early development *<daniel@norman.life>*

### Changed

* `e2da22d` Update docs to reflect author and locations *<daniel@norman.life>*

### Other

* `f781788` Release radicle-artifact version 0.4.0 *<daniel@norman.life>*

## [0.3.0] - 2026-03-17

### Added

* `9d2ba99` Add author (NodeID) field to Release *<daniel@norman.life>*

### Other

* `f90b8ca` Simplify artifact locations to one URL per node *<daniel@norman.life>*
* `fe62047` Document release flag *<daniel@norman.life>*
* `dd160b5` Release radicle-artifact version 0.3.0 *<daniel@norman.life>*

## [0.2.0] - 2026-03-16

### Fixed

* `a363a05` Fix deprecated radicle 0.21.0 calls in announce *<daniel@norman.life>*

### Other

* `4882746` Bump radicle crate to 0.21.0 *<daniel@norman.life>*
* `789d7bb` Release radicle-artifact version 0.2.0 *<daniel@norman.life>*

## [0.1.0] - 2026-03-13

### Added

* `53182e9` Initial commit of radicle-artifact COB *<daniel@norman.life>*
* `6444e01` Add release tooling and changelog *<daniel@norman.life>*

### Changed

* `25b2061` Replace custom Cid wrapper with the cid crate *<daniel@norman.life>*

### Fixed

* `b443908` Fix iterator overflow, API safety, and add tests *<daniel@norman.life>*

### Other

* `32cb68f` Release radicle-artifact version 0.1.0 *<daniel@norman.life>*

