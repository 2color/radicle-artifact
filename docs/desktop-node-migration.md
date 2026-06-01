# Handoff: migrate radicle-desktop onto the `rad-artifact` node

**Audience:** whoever wires `radicle-desktop` onto the new long-running
seeder node in `radicle-artifact`.
**Status of the producer side (`radicle-artifact`):** the node, client,
protocol, and seeder landed earlier; the `radiroh://` location scheme is
already in place (the legacy `iroh://` scheme is rejected on read). The
fetch read-side protocol (`Has`/`Export`/`Fetch`, streaming) is now
**implemented** — patch `659e57a` (pending merge to `main`) per
`fetch-protocol-design.md`. So neither side is gated any longer: both the
write side and the read side can be migrated against the shipped API.
**Status of the consumer side (`radicle-desktop`):** unstarted. Desktop
still runs its own in-process seeder (`crates/radicle-types/src/seeder.rs`)
and pins `radicle-artifact = "0.13"`.

> **This is not a drop-in swap.** The desktop's iroh `FsStore` is used for
> *both* seeding and fetching, and four things changed underneath it
> (store ownership, identity, URL scheme, tag layout). Read "Four
> mismatches" before estimating.

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
  - **Read side (new):**
    - `.has(cid) -> HasResult` — `{ present, complete, bytes }`, one-shot,
      no network. The "bytes already local?" fast-path probe.
    - `.export(cid, dest, idle, on_progress) -> ExportReceipt` — stream
      already-local bytes to disk; `ErrorCode::NotLocal` if absent.
    - `.fetch(args: FetchArgs, idle, on_progress) -> FetchReceipt` —
      fast-path export if local, else download from `args.locations` into
      the store, export, and (if `args.seed`) tag as seeded.
    - `.fetch_blocking` / `.export_blocking` — sync wrappers (the CLI uses
      these). `on_progress: impl FnMut(&FetchProgress)`; `idle: Duration`
      bounds each frame, not the whole transfer.
    - `FetchArgs { rid, cid, locations: Vec<FetchLocation>, dest, seed }`.
      The caller resolves COB locations into `Vec<FetchLocation>` itself
      (the node does no DID resolution); `src/bin/rad-artifact/main.rs`
      `artifact_locations` is the reference resolver.
  - Note the **typed** args: `rid: RepoId`, `cid: Cid` (not strings, as of
    the `type … on the wire` refactors). `kind: ArtifactKind`,
    `mode: ImportMode` (`Copy` | `Reference`).
- `protocol::{Command, CommandResult, CommandError, ErrorCode,
  SeedReceipt, UnseedReceipt, SeededEntry, Status,
  FetchLocation, HasResult, ExportReceipt, FetchReceipt, FetchProgress,
  StreamEvent, …}` — the wire types. `SeedReceipt.endpoint_id` /
  `FetchReceipt.endpoint_id` / `Status.endpoint_id` are a typed
  `share::keys::EndpointId` that serializes as a canonical
  `radiroh://<base32>` URL. `FetchProgress` is an enum
  (`connecting` / `trying-provider` / `provider-failed` / `downloading` /
  `exporting`) — bridge it to the `artifact_progress` Tauri event.
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

### 2. Fetching (read side) — was the blocker, now unblocked

The same `iroh.blobs` store is also the download target and source. The
three read-side call sites map one-to-one onto the new client methods:

| Desktop call site | Current call | Replacement |
|---|---|---|
| `release.rs:303,408` | `iroh.blobs.blobs().has(hash)` (fast path) | `client.has(cid)` → `HasResult.complete` |
| `release.rs:305,366` | `fetch::export(&iroh.blobs, hash, kind, &dest)` | `client.export(cid, dest, idle, on_progress)` |
| `release.rs:343` | `fetch::fetch_artifact(&iroh.blobs, endpoint, …)` | `client.fetch(FetchArgs{…}, idle, on_progress)` |

Why this needed a protocol change at all: `FsStore` is single-writer, so
once the node owns `<home>/artifacts/store/`, the desktop **cannot** open
its own `FsStore` on the same `RAD_HOME` — the second open blocks on the
lock. The node had to grow `Has`/`Export`/`Fetch` so the desktop opens no
store of its own. That work is **done** (patch `659e57a`):

- **Option A — extend the protocol (chosen, implemented).** The node owns
  all blob I/O; the desktop opens no store. `Has`/`Export`/`Fetch` plus the
  `StreamEvent` streaming envelope shipped per `fetch-protocol-design.md`.
- **Option B — split stores** *(rejected).* Node owns the seeding store;
  the desktop keeps a separate ephemeral endpoint+store for fetching. Two
  iroh endpoints, and the local-bytes fast path breaks because seeded bytes
  live in the node's store. Retained only as a rejected alternative.
- **Option C — embed the node in-process** *(rejected).* Reintroduces the
  single-writer conflict the moment a CLI node also runs on the same home,
  and loses the "one seeder per host" model.

Consequence for this migration: both halves are now unblocked.

- **Write side (seeding) — independent.** The seven call sites in the
  table above move onto the existing `Client`; no protocol change.
- **Read side (fetching) — ready.** The protocol shipped, so the three
  read-side call sites map straight onto `client.has` / `client.export` /
  `client.fetch` (see `fetch-protocol-design.md` "Desktop mapping").
  The caller resolves COB locations into `Vec<FetchLocation>` first
  (reference resolver: `artifact_locations` in the CLI).
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

1. Bump `radicle-artifact` to the version carrying the fetch protocol
   (patch `659e57a`; path or git dep until published), `features =
   ["share"]`.
2. **Write side (independent):** replace `IrohState`'s seeding role — add a
   `Client` to app state; rewrite the seven write-side call sites (table
   above) to go through it. Keep `rid` threading — it's already at every
   call site. No protocol change needed.
3. **Read side (now available):** rewrite the three read-side call sites
   onto `client.has`/`export`/`fetch`; bridge `FetchProgress` frames to the
   `artifact_progress` event. The protocol has shipped — no longer gated.
4. Handle identity + scheme + tag migration (the four mismatches). At
   minimum: stop hand-building URLs, sweep stale `iroh://` URLs under the
   user's DID, document/auto-handle the re-seed.
5. Pick a runtime story (1/2/3) and add the "node not running" UX.
6. Delete `crates/radicle-types/src/seeder.rs` and the `iroh.key` /
   `FsStore` startup once nothing references them.

## Reference points in radicle-artifact

- `src/bin/rad-artifact/node.rs` — the canonical `Client` caller for the
  seeding commands, including seed-then-`add_location` and the reconcile sweep.
- `src/bin/rad-artifact/main.rs` `run_fetch` — reference read-side caller:
  resolves COB locations via `artifact_locations` → `Vec<FetchLocation>`,
  drives `client.fetch_blocking` with a progress bar, and (on `--seed`)
  writes the `add_location` COB from `FetchReceipt.endpoint_id`.
- `src/share/fetch.rs` — the node-side fetch/export core, if the desktop
  ever needs to understand what the node does behind the protocol.
- `src/client/mod.rs` — full client surface + `ClientError` (note the
  ConnectionRefused/NotFound → "not running" mapping in
  `node.rs:client_err`).
- `src/protocol/mod.rs` — wire types; `endpoint_id` is the
  `radiroh://<base32>` URL you announce.
- `src/node/lifecycle.rs` — `spawn_detached`, `resolve_passphrase`,
  `wait_until_running` for the runtime story.
- `docs/` / README "Seeding via the local node" — user-facing model.
