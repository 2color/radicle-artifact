# Handoff: migrate radicle-desktop onto the `rad-artifact` node

**Audience:** whoever wires `radicle-desktop` onto the new long-running
seeder node in `radicle-artifact`.
**Status of the producer side (`radicle-artifact`):** the node, client,
protocol, and seeder all landed on `main` (crate `0.14`). The
`switch-url-scheme` branch renames the location scheme `iroh://` →
`radiroh://`; land/merge that before starting so the desktop targets the
final scheme. The fetch read-side protocol (`Has`/`Export`/`Fetch`) is
specified in `fetch-protocol-design.md` but not yet implemented — the
read-side migration is gated on it; the write side is not.
**Status of the consumer side (`radicle-desktop`):** unstarted. Desktop
still runs its own in-process seeder (`crates/radicle-types/src/seeder.rs`)
and pins `radicle-artifact = "0.13"`.

> **This is not a drop-in swap.** The desktop's iroh `FsStore` is used for
> *both* seeding and fetching, and four things changed underneath it
> (store ownership, identity, URL scheme, tag layout). Read "The blocker"
> before estimating.

---

## What radicle-artifact now exposes

All under the `share` feature, crate `radicle_artifact`:

- `client::Client` — Unix-socket client. Async-first, with a
  `call_blocking` wrapper.
  - `Client::default_socket(home) -> PathBuf` — `$RAD_ARTIFACT_SOCKET`
    else `<home>/artifacts/control.sock`.
  - `Client::new(socket)`, `.is_running() -> bool` (500 ms probe).
  - `.seed(rid, cid, path, kind, mode) -> SeedReceipt`
  - `.unseed(rid, cid) -> UnseedReceipt`
  - `.is_seeding(rid, cid) -> bool`
  - `.list_seeded(rid) -> Vec<SeededEntry>`
  - `.status() -> Status`
  - `.shutdown()`
  - Note the **typed** args: `rid: RepoId`, `cid: Cid` (not strings, as of
    the `type … on the wire` refactors). `kind: ArtifactKind`,
    `mode: ImportMode` (`Copy` | `Reference`).
- `protocol::{Command, CommandResult, CommandError, ErrorCode,
  SeedReceipt, UnseedReceipt, SeededEntry, Status, …}` — the wire types.
  `SeedReceipt.endpoint_id` / `Status.endpoint_id` are a typed
  `share::keys::EndpointId` that serializes as a canonical
  `radiroh://<base32>` URL.
- `node::run(home, secret)` + `node::lifecycle::{resolve_passphrase,
  rotate_log, spawn_detached, wait_until_running, log_path}` — the daemon
  and its parent-side startup helpers. The CLI's `rad-artifact node start`
  is the reference caller (`src/bin/rad-artifact/node.rs`).

The node **writes no COB ops** — every `add_location` / `remove_location`
stays client-side, signed by the user. That part of the desktop's flow
does not move.

---

## How the desktop uses the seeder today

`IrohState { blobs: FsStore, iroh_router: Router }` is built once at
startup (`crates/radicle-tauri/src/commands/startup.rs:89`) and managed as
Tauri state. It backs two distinct concerns:

### 1. Seeding (write side) — maps cleanly onto `Client`

| Desktop call site | Current call | Replacement |
|---|---|---|
| `commands/cob/release.rs:192` (`seed_artifact`) | `seeder::seed(&iroh.blobs, &cid, &source)` | `client.seed(rid, cid, &source, kind, Copy)` |
| `release.rs:197/216/414` | `seeder::our_iroh_url(router.endpoint())` | `SeedReceipt.endpoint_id` (already the URL) |
| `release.rs:225` (`unseed_artifact`) | `seeder::unseed(&iroh.blobs, &cid)` | `client.unseed(rid, cid)` |
| `release.rs:231` (`is_seeding`) | `seeder::is_seeded_str(&iroh.blobs, &cid)` | `client.is_seeding(rid, cid)` |
| `release.rs:242` (`seeded list`) | `seeder::seeded_cids(&iroh.blobs)` | `client.list_seeded(rid)` (per-repo) |
| `release.rs:254` | `seeder::artifact_size_str(&iroh.blobs, &cid)` | `SeededEntry.bytes` from `list_seeded` |
| `release.rs:409` (`maybe_auto_seed`) | `seeder::tag_artifact_in_store(...)` | `client.seed(...)` (re-verifies + tags) |

The Tauri commands already receive `rid: RepoId`, so feeding the
per-repo API is straightforward.

### 2. Fetching (read side) — **the blocker**

The same `iroh.blobs` store is also the download target and source:

- `release.rs:303,408` — `iroh.blobs.blobs().has(hash)` (fast path: bytes
  already local).
- `release.rs:305,366` — `fetch::export(&iroh.blobs, hash, kind, &dest)`
  writes store bytes to disk.
- `release.rs:343` — `fetch::fetch_artifact(&iroh.blobs, router.endpoint(),
  …)` downloads a CID from peer locations *into* the store
  (`crates/radicle-types/src/fetch.rs`, uses `iroh_blobs … Downloader`).

**The node protocol has no fetch / export / has command.** It only does
Status/Seed/Unseed/IsSeeding/ListSeeded/Shutdown. And `FsStore` is
single-writer: if the node owns `<home>/artifacts/store/`, the desktop
**cannot** open its own `FsStore` on the same `RAD_HOME` to fetch into —
the second open blocks on the lock.

So you cannot just delete the seeder and keep `fetch.rs` pointed at a
local store. One of these has to happen:

- **Option A — extend the protocol.** Add `Fetch { rid, cid, locations,
  dest }` (+ progress streaming) and `Export`/`Has` commands so the node
  owns all blob I/O and the desktop opens no store at all. Cleanest
  end state; biggest change (new wire commands + a streaming/`Subscribe`
  mechanism the protocol doesn't have yet — `Command` is `#[non_exhaustive]`
  precisely for this).
- **Option B — split stores.** Node owns the seeding store; the desktop
  keeps a *separate ephemeral* endpoint+store for fetching (mirror the
  CLI's one-shot `share::download` path). Smaller change, but: two iroh
  endpoints, and the "bytes already local → skip network" fast path
  breaks because seeded bytes live in the node's store, not the
  fetcher's. Auto-seed-after-fetch becomes a `client.seed(path)` of the
  exported file rather than a tag of already-present bytes.
- **Option C — embed the node in-process.** Call `node::run` from the
  Tauri runtime instead of shelling out. Avoids a separate process but
  reintroduces the single-writer conflict the moment a CLI node is also
  started on the same home, and you lose the "one seeder per host" model.
  Not recommended.

**Decision (2026-05): go straight to Option A.** The streaming fetch
protocol is now specified in `fetch-protocol-design.md` (`Has` / `Export`
/ `Fetch` commands, a `StreamEvent` envelope, provider-level progress).
That removes the reason to build — and then throw away — the Option B
stopgap. Option B/C are retained above only as rejected alternatives.

Consequence for this migration: the work splits cleanly in two.

- **Write side (seeding) — independent, do it first.** The seven call
  sites in the table above move onto the existing `Client` with *no*
  protocol change. This is unblocked today and need not wait on anything.
- **Read side (fetching) — gated on the new protocol.** Once `Has` /
  `Export` / `Fetch` land in `radicle-artifact` per the design doc, the
  three read-side call sites map straight onto `client.has` /
  `client.export` / `client.fetch` (see that doc's "Desktop mapping").
  Bridge the streamed `FetchProgress` frames into the existing
  `artifact_progress` Tauri event.

---

## Four mismatches to migrate, not just an API swap

1. **Store ownership / lock.** Desktop currently opens its own `FsStore`
   at startup. Once the node owns `<home>/artifacts/store/`, that open
   must go away (or move to a different dir under Option B). Decide who
   starts the node — see "Runtime/distribution" below.
2. **Identity changes.** Desktop generates and persists an *independent*
   `iroh.key` (`seeder.rs:load_or_generate_key`). The node derives its
   key from the radicle keystore (`share::keys::radicle_secret_to_iroh`).
   Switching means the desktop's **endpoint id changes**, so every
   location it previously announced under the old key goes stale. Plan a
   one-time `reconcile`-style cleanup (the CLI's `rad-artifact reconcile
   --remove-orphaned-self` already does exactly this for the node's key;
   the desktop's *old* key won't be recognized, so those URLs need an
   explicit sweep — see commit `d3dd21f` "sweep legacy iroh:// URLs").
3. **URL scheme + encoding.** Desktop writes `iroh://{endpoint.id()}`
   where `endpoint.id()` is iroh's Display (z-base-32). radicle-artifact
   now writes `radiroh://{base32}` via the typed `EndpointId`. These are
   not interchangeable. After migration the desktop should stop building
   URLs by hand and use `SeedReceipt.endpoint_id` verbatim.
4. **Tag layout.** Desktop tags are global `seeded/{cid}`. The node uses
   per-repo, binary length-prefixed `seeded/{rid}/{cid}` (commits
   `8e3893b`, `405222b`). The desktop's existing tags are invisible to
   the node, so a user upgrading will appear to stop seeding everything
   until re-seeded. Either migrate tags on first run or document the
   re-seed.

---

## Runtime / distribution

The node is a separate process. Three ways for the desktop to ensure one
is running (from the design doc, in increasing effort):

1. **Expect `rad-artifact` installed** (v1 default). On startup probe
   `Client::is_running()`; if false, surface a setup prompt
   ("install rad-artifact, run `rad-artifact node start`"). Smallest
   change, no build-pipeline work.
2. **Tauri sidecar.** Bundle the `rad-artifact` binary in app resources
   and spawn `node start` on launch. Needs the desktop build to
   cross-compile/ship the binary.
3. **Spawn via `node::lifecycle::spawn_detached`.** The desktop already
   links the crate, so it can start a detached node itself given the
   keystore passphrase. Reuses the exact CLI startup path.

Passphrase: an encrypted keystore needs `RAD_PASSPHRASE` (or a prompt) at
node start. The desktop already unlocks the keystore, so it can pass the
secret through `spawn_detached` / the env.

---

## Suggested sequence

1. Bump `radicle-artifact` to `0.14`+ (path or git dep until published),
   `features = ["share"]`.
2. **Write side (independent, land first):** replace `IrohState`'s seeding
   role — add a `Client` to app state; rewrite the seven write-side call
   sites (table above) to go through it. Keep `rid` threading — it's
   already at every call site. No protocol change needed; not gated on the
   fetch work.
3. **Read side (gated on the fetch protocol):** once `Has`/`Export`/`Fetch`
   land per `fetch-protocol-design.md`, rewrite the three read-side call
   sites onto `client.has`/`export`/`fetch`; bridge `FetchProgress` frames
   to the `artifact_progress` event.
4. Handle identity + scheme + tag migration (the four mismatches). At
   minimum: stop hand-building URLs, sweep stale `iroh://` URLs under the
   user's DID, document/auto-handle the re-seed.
5. Pick a runtime story (1/2/3) and add the "node not running" UX.
6. Delete `crates/radicle-types/src/seeder.rs` and the `iroh.key` /
   `FsStore` startup once nothing references them.

## Reference points in radicle-artifact

- `src/bin/rad-artifact/node.rs` — the canonical `Client` caller for every
  command, including seed-then-`add_location` and the reconcile sweep.
- `src/client/mod.rs` — full client surface + `ClientError` (note the
  ConnectionRefused/NotFound → "not running" mapping in
  `node.rs:client_err`).
- `src/protocol/mod.rs` — wire types; `endpoint_id` is the
  `radiroh://<base32>` URL you announce.
- `src/node/lifecycle.rs` — `spawn_detached`, `resolve_passphrase`,
  `wait_until_running` for the runtime story.
- `docs/` / README "Seeding via the local node" — user-facing model.
