# Design: fetch over the node control protocol

**Audience:** whoever implements blob fetching on the `rad-artifact` node, and the desktop migration that consumes it.
**Status:** proposed. The node today does Status/Seed/Unseed/IsSeeding/ListSeeded/Shutdown only — all one-shot. This adds the read side (`Has`/`Export`/`Fetch`) so the node owns *all* blob I/O.
**Relation to other docs:** this is "Option A" from `desktop-node-migration.md` (lines 97-119). Landing it unblocks the desktop's fetch migration and removes the need for the Option B stopgap.

---

## Goals

1. One protocol that serves both callers: the CLI's `rad-artifact fetch` and the desktop's read side (`has` / `export` / `fetch_artifact`, `artifact_progress` events).
2. The node owns the single `FsStore` and the single iroh endpoint. No other process opens a store (the single-writer lock makes that impossible anyway).
3. Fetched bytes land in the node store so they can be re-served — plain fetch caches (reclaimed by GC), `--seed` pins and serves.
4. Long downloads stream progress back to the caller.
5. Abort-safe at every step: a killed fetch leaves only reclaimable partial bytes, never a false-complete file or a tag advertising bytes the node doesn't have.

## Principles that constrain the design

- **Blob I/O requires the node; COB/identity ops do not.** Seeding *metadata* (`add_location` / `remove_location`) stays client-side, signed by the user, and keeps working with no node running. Only operations that touch *bytes* (seed, fetch, export) go through the node. So `fetch` requires a node; announcing a location does not.
- **The node writes no COBs.** Consequence for fetch-then-seed: the node tags and serves the bytes, but making yourself *discoverable* (`add_location`) is a separate signed write the caller does afterward — see "The add_location split."
- **The node does no identity resolution.** Bare `radiroh://` locations are resolved to concrete endpoint ids by the caller (it has the COB/DID knowledge). The node receives already-resolved locations.

---

## Wire protocol additions

All additive. `Command` and `ErrorCode` are already `#[non_exhaustive]`. New response structs are marked `#[non_exhaustive]` so fields can be added without breaking the desktop's Rust API.

### Resolved location type

The node receives fully-resolved locations, not COB URLs:

```rust
/// A resolved place to fetch from. Owned + serde, unlike `fetch::Location<'a>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FetchLocation {
    /// Serializes as the URL string, e.g. "https://…".
    Url(Url),
    /// Serializes as the canonical "radiroh://<base32>" string.
    Iroh(EndpointId),
}
```

The caller builds `Vec<FetchLocation>` from COB data (the CLI's `artifact_locations` already does exactly this resolution, including DID-derivation for bare `radiroh://`). The node partitions by variant — the same split `fetch::partition_locations` does today.

### Three new commands

```rust
pub enum Command {
    // … existing …

    /// Cheap predicate: is this CID's content present/complete locally?
    /// No network. Used for UI state and fast-path decisions.
    Has {
        #[serde(with = "cid_string")] cid: Cid,
    },

    /// Export already-local bytes to `dest`. No network. Streams byte
    /// progress (large local copies). Errors with `NotLocal` if the
    /// content isn't complete in the store.
    Export {
        #[serde(with = "cid_string")] cid: Cid,
        dest: PathBuf,
    },

    /// Full fetch: fast-path export if local, else download from
    /// `locations` into the store, export to `dest`, optionally tag as
    /// seeded. Streams progress (see the streaming envelope below).
    Fetch {
        rid: RepoId,
        #[serde(with = "cid_string")] cid: Cid,
        locations: Vec<FetchLocation>,
        dest: PathBuf,
        /// Tag `seeded/{rid}/{cid}` after completion so the node serves it.
        seed: bool,
    },
}
```

`kind` (blob vs collection) is **not** on the wire — the node derives it from the CID codec via `cid_utils::artifact_kind`, which is already the source of truth that `download` / `download_collection` dispatch on. `rid` is only needed on `Fetch` (for the per-repo `seeded/{rid}/{cid}` tag when `seed: true`); `Has` and `Export` are hash-keyed and repo-agnostic.

### Streaming envelope

`Has` stays one-shot (`CommandResult<HasResult>`). `Fetch` and `Export` both stream a sequence of newline-delimited frames — zero or more `progress`, then exactly one terminal `okay` / `error`. One generic envelope covers both, parameterised by the terminal payload:

```rust
/// Streaming frames. Externally tagged. The `okay`/`error` tags match
/// `CommandResult` so a terminal frame is recognizable to a generic
/// reader; `progress` is the new, repeatable, non-terminal frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamEvent<T> {
    Progress(FetchProgress), // {"progress": {…}}  repeatable, non-terminal
    Okay(T),                 // {"okay": {…}}       terminal success
    Error(CommandError),     // {"error": {…}}      terminal failure
}
```

`Fetch` emits `StreamEvent<FetchReceipt>`; `Export` emits `StreamEvent<ExportReceipt>`. Export only ever emits `Exporting` progress variants (no network, no providers).

### Response payloads

```rust
#[non_exhaustive]
pub struct HasResult {
    pub present: bool,   // any bytes in the store
    pub complete: bool,  // fully downloaded
    pub bytes: u64,      // logical size known so far
}

#[non_exhaustive]
pub struct ExportReceipt {
    #[serde(with = "cid_string")] pub cid: Cid,
    pub dest: PathBuf,
    pub bytes: u64,
}

/// One progress frame. An enum, not a struct, so provider-level events
/// (which carry no byte offset) and byte-movement events are modeled
/// distinctly. These map 1:1 onto the iroh `DownloadProgressItem` kinds
/// the download loop already produces (fetch.rs:181-195) — today they're
/// `eprintln`'d; here they become frames.
#[non_exhaustive]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum FetchProgress {
    /// Binding/relay setup before any provider is tried.
    Connecting,
    /// Provider-level: now attempting this endpoint.
    TryingProvider { endpoint_id: EndpointId },
    /// Provider-level: this endpoint failed; moving on.
    ProviderFailed { endpoint_id: EndpointId },
    /// Byte movement during download.
    Downloading { offset: u64, total: Option<u64> },
    /// Byte movement while writing the store out to disk. `entry` names
    /// the collection member being exported (None for a single blob).
    Exporting { offset: u64, total: Option<u64>, entry: Option<String> },
}

#[non_exhaustive]
pub struct FetchReceipt {
    pub rid: RepoId,
    #[serde(with = "cid_string")] pub cid: Cid,
    pub dest: PathBuf,
    pub bytes: u64,
    pub from_cache: bool,        // bytes were already local; no network
    pub seeded: bool,            // a seeded tag is now set
    /// Endpoint id the node serves on, as "radiroh://<base32>". Present so
    /// the caller can write the add_location COB after seed=true. Mirrors
    /// SeedReceipt.endpoint_id.
    pub endpoint_id: EndpointId,
}
```

`FetchProgress` drives both the CLI's indicatif bar and the desktop's `artifact_progress` event. The byte-movement variants (`Downloading`/`Exporting`) give offset/total for a percentage; `TryingProvider`/`ProviderFailed` give a per-provider status line. The download loop already receives these from the iroh `Downloader` and currently discards the provider ones to stderr — surfacing them as frames is essentially free.

### New error codes

```rust
pub enum ErrorCode {
    // … existing …
    NotLocal,     // Export/Has-driven path needed local bytes that aren't present
    NoLocations,  // Fetch with an empty/unusable location set
    AllFailed,    // every provider/URL failed (message lists them)
}
```

### Wire examples

```
→ {"command":"has","cid":"bafy…"}
← {"okay":{"present":true,"complete":false,"bytes":1024}}

→ {"command":"fetch","rid":"rad:z2u…","cid":"bafy…","locations":[{"iroh":"radiroh://abc…"},{"url":"https://e.x/f"}],"dest":"/tmp/out","seed":true}
← {"progress":{"kind":"connecting"}}
← {"progress":{"kind":"trying-provider","endpoint_id":"radiroh://abc…"}}
← {"progress":{"kind":"downloading","offset":65536,"total":1048576}}
← {"progress":{"kind":"exporting","offset":1048576,"total":1048576,"entry":null}}
← {"okay":{"rid":"rad:z2u…","cid":"bafy…","dest":"/tmp/out","bytes":1048576,"from_cache":false,"seeded":true,"endpoint_id":"radiroh://self…"}}

→ {"command":"export","cid":"bafy…","dest":"/tmp/out2"}
← {"progress":{"kind":"exporting","offset":524288,"total":1048576,"entry":null}}
← {"okay":{"cid":"bafy…","dest":"/tmp/out2","bytes":1048576}}
```

---

## Client API additions

`Has` reuses the existing `call` / `call_blocking` — one-shot, just a typed wrapper:

```rust
impl Client {
    pub async fn has(&self, cid: Cid) -> Result<HasResult, ClientError>;
}
```

`Fetch` and `Export` both stream, so they need a new reader: `call_inner` reads exactly one line today, whereas these read frames in a loop and invoke a callback per progress frame. One generic streaming method backs both:

```rust
impl Client {
    /// Drives a streaming command. `on_progress` fires per progress frame;
    /// returns the terminal `okay` payload. Idle-timeout (no frame for
    /// `idle`) aborts — mirrors fetch.rs's IDLE_TIMEOUT, reset on every
    /// frame, not a total cap.
    async fn call_streaming<T: DeserializeOwned>(
        &self,
        cmd: &Command,
        idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<T, ClientError>;

    // Thin typed wrappers over call_streaming:
    pub async fn fetch(
        &self, args: FetchArgs, idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<FetchReceipt, ClientError>;

    pub async fn export(
        &self, cid: Cid, dest: &Path, idle: Duration,
        on_progress: impl FnMut(&FetchProgress),
    ) -> Result<ExportReceipt, ClientError>;

    // Blocking wrappers for the CLI (build a current-thread runtime,
    // like call_blocking).
    pub fn fetch_blocking(/* … */) -> Result<FetchReceipt, ClientError>;
    pub fn export_blocking(/* … */) -> Result<ExportReceipt, ClientError>;
}
```

Timeout model for both: **no total cap.** The client wraps each `read_line` in `timeout(idle, …)` and resets on every frame received — the same idle-deadline logic `iroh_fetch_to_store` already uses around its download stream (fetch.rs:169-195). A terminal `error` frame surfaces as `ClientError::Remote`; the idle timeout surfaces as `ClientError::Timeout`. (Export has no network stall risk, but a slow disk still benefits from idle-rather-than-total bounding.)

The desktop bridges `on_progress` straight into a Tauri `emit("artifact_progress", …)`. The CLI bridges it into the existing indicatif bar.

---

## Node side

### Threading the endpoint into handlers

Today `dispatch`/`handle_connection` take `store: &FsStore` plus loose params. `Fetch` also needs the endpoint (to build a `Downloader`). Introduce a context struct built once at bootstrap and shared (cheap clones / `Arc`):

```rust
struct NodeCtx {
    store: FsStore,
    downloader: Downloader,  // built once on the seeder endpoint, pools connections
    endpoint_id: EndpointId,
    started_at_unix: i64,
}
```

`run()` builds the `Downloader` from `seeder.router.endpoint()` once (reuses the connection pool across all fetches, instead of fetch.rs's per-call bind) and passes `&NodeCtx` to handlers. This also tidies the current 5-argument `dispatch`.

### Fetch handler flow (abort-safe ordering)

1. Derive `hash` + `kind` from the CID. Check completeness: `store.remote().local(haf).await?.is_complete()`.
2. **Fast path** — if complete: export to `dest` (atomic + streamed, see below), ensure the seeded tag iff `seed`, emit `okay` with `from_cache: true`. No network, but still emits `Exporting` progress.
3. **Download path** — else:
   - Import-protect with a **temp tag** for the whole download (abort-safe: if the task dies, the temp tag drops and GC reclaims the partial).
   - iroh providers: `downloader.download(haf, Shuffled::new(providers))`, consume the progress stream, mapping `DownloadProgressItem` → `FetchProgress` frames: `TryProvider`/`ProviderFailed` → the provider-level variants (today these are `eprintln`'d at fetch.rs:181-189), `Progress`/`PartComplete` → `Downloading`. Keep the idle deadline. This is the existing `iroh_fetch_to_store` loop, reused.
   - HTTP URLs (fallback, blobs only): download to a temp file, `add_path` (Copy) into the store, verify hash == cid. Routing HTTP *through the store* keeps the seed/cache/fast-path behavior identical across transports — HTTP-fetched blobs become seedable iroh blobs.
   - After the store reports complete: **export atomically** — export to `dest.partial` (or temp path) then rename, so a kill mid-export never leaves a truncated file at `dest`, emitting `Exporting` frames as bytes land. (Blob: `store.blobs().export`. Collection: load the `Collection`, export each entry under `dest`, with `entry` set per member.)
   - **Tag last**: if `seed`, set `seeded/{rid}/{cid}` *after* the export verifies complete; then drop the temp tag. If not `seed`, drop the temp tag → GC reclaims later (cache-via-GC).
   - Emit terminal `okay` / `error`.

### Export and Has handlers

- **`Export`** is the fast path in isolation: resolve hash from CID, require `store.blobs().has(hash)` complete (else `NotLocal`), then stream the same atomic export as step 3. Repo-agnostic — it serves bytes the store holds regardless of which repo tagged them (cross-repo sharing is intended; the hash is the only key).
- **`Has`** resolves hash from CID and reports `store.blobs().has(hash)` / completeness — one cheap one-shot reply, no streaming.

### Streaming framing + disconnect → abort

The connection handler for the streaming commands (`Fetch`, `Export`) writes many lines instead of one. Crucially for `Fetch`: **a write error on a progress frame means the client is gone → drop the download future → temp tag drops → partial reclaimed.** Today nothing long-running can be orphaned; this is the new wiring that keeps it that way. Implement by selecting the download future against the socket write; on `BrokenPipe`, cancel. (`Export` is local-only, so a disconnect there just aborts a disk copy — no temp tag to reclaim.)

### Timeout / shutdown interactions

- The initial command-line `READ_TIMEOUT` (30s) is unchanged.
- The download itself is bounded by the **idle deadline** around the stream (reuse `IDLE_TIMEOUT`), not a total cap — a cascade of dead providers can't extend it.
- `DRAIN_TIMEOUT` (300s) on shutdown may force-cut an in-flight fetch. That's safe by the ordering above: partial bytes resume next time, the temp tag drop is harmless. Document the fetch handler's abort-safety as a maintained invariant.

---

## `share/fetch.rs` refactor

Because the CLI now routes through the node too (no standalone path — per the "require a node" decision), the ephemeral scaffolding in `share/fetch.rs` is no longer the fetch entry point:

- **Extract & keep** the reusable core: the download-progress loop with the idle deadline, the completeness check, and the blob/collection export. These now run against the node's *persistent* store + shared endpoint, called from the node handler.
- **Retire** the ephemeral wrappers: `run_iroh_attempt`'s per-call runtime, `open_ephemeral_store`, `iroh_store_dir`, and the per-call `Endpoint::builder().bind()`. The node already has a runtime, store, and endpoint.
- **Move HTTP fetch server-side** (`fetch_http` and the `.partial`/verify/rename dance) so the node is the single owner of all blob I/O, per the migration doc's Option A.
- The public sync `download` / `download_collection` entry points go away (or become thin internal helpers); the doc comments in `share/mod.rs` about "ephemeral runtime/endpoint/store per call" get updated.

Net: the *logic* survives, the *ownership* moves from "ephemeral per CLI call" to "persistent on the node."

---

## CLI changes

`run_fetch` (main.rs:1047) stops calling `share::download` and instead:

1. Resolves locations from COBs as it does now (`artifact_locations`) → `Vec<FetchLocation>`.
2. Dials the node: `Client::default_socket(home)`; if `!is_running()`, error with the install/start hint (acceptable per decision).
3. Calls `client.fetch_blocking(args, IDLE_TIMEOUT, |p| update_bar(p))`, driving the indicatif bar from progress frames (byte variants → position; provider variants → status line).
4. On `seed: true` (a new `fetch --seed` flag), after success writes the `add_location` COB using `receipt.endpoint_id` — see below.

`rad-artifact export <cid> <dest>` and a `has` check come for free as thin wrappers — `export` drives the same bar from its `Exporting` frames.

---

## Desktop mapping

The three blocked read-side call sites (migration doc lines 77-86) map one-to-one:

| Desktop today | Replacement |
|---|---|
| `iroh.blobs.blobs().has(hash)` (release.rs:303,408) | `client.has(cid)` |
| `fetch::export(&iroh.blobs, hash, kind, &dest)` (release.rs:305,366) | `client.export(cid, &dest, idle, on_progress)` → bridge to `emit("artifact_progress")` |
| `fetch::fetch_artifact(&iroh.blobs, endpoint, …)` (release.rs:343) | `client.fetch(args, idle, on_progress)` → bridge `on_progress` to `emit("artifact_progress")` |

After this, the desktop opens **no** `FsStore` and **no** iroh endpoint of its own — resolving the identity/dual-endpoint mismatch (migration doc #2) on the read side, and letting `crates/radicle-types/src/{seeder,fetch}.rs` and the `iroh.key`/`FsStore` startup be deleted.

Note on cache-via-GC: a previously-fetched-but-unseeded artifact may have been swept, so `has` is not a durable guarantee — the fast path must tolerate a miss and fall through to `fetch`. The desktop's `maybe_auto_seed` tags most fetched artifacts anyway, so in practice this only bites fetches the user chose not to seed.

---

## The add_location split

`seed: true` makes the node *serve* the bytes; it does not make you *discoverable*. Discoverability is the `add_location` COB, which only the user can sign and which works with no node running. So the post-fetch seed flow is two steps, exactly like the existing seed path (`node.rs` does seed-then-`add_location`):

1. `client.fetch(… seed: true)` → node tags + serves, returns `endpoint_id`.
2. Caller writes `add_location(rid, cid, receipt.endpoint_id)` as a signed COB.

This keeps the "COB ops work without the node" guarantee intact: you can fetch+seed bytes (needs node) and separately announce the location (no node needed).

---

## Backward compatibility & sequencing

- **Additive wire-wise.** New `Command` variants, new `ErrorCode`s, new response types — nothing existing changes shape. The `okay`/`error` terminal tags on `StreamEvent` match `CommandResult`.
- **Write side is split out as independent work.** The desktop's seven seeding call sites migrate onto the existing `Client` with no protocol change, so that piece can land first and in parallel — it is now tracked separately in `desktop-node-migration.md` rather than gated on this design.
- **Read side gates on this.** Suggested order:
  1. Protocol types (`FetchLocation`, commands, `StreamEvent`, `FetchProgress`, payloads, error codes) + wire-snapshot tests.
  2. `share/fetch.rs` refactor: extract the reusable core, move HTTP server-side.
  3. Node `NodeCtx` + shared `Downloader`; `Has` (one-shot) + `Export` (streaming) handlers.
  4. `Fetch` handler: fast path, download, atomic export, tag-last, disconnect→abort, streaming frames.
  5. Client `has` (one-shot) + `call_streaming` backing `fetch`/`export` (+blocking) with the idle-timeout reader.
  6. CLI `run_fetch` routes through the node; add `fetch --seed` + the `add_location` follow-up; add `export`.
  7. Desktop migration consumes the above.

## Testing

- **Wire snapshots** for every new command/result (extend `protocol::tests`), pinning JSON exactly as the existing ones do.
- **Node round-trip** (extend `node::tests::node_round_trip`): seed bytes on one store, `Has` → complete, `Export` → streams `Exporting` then `okay`, file on disk matches; `Fetch` with `from_cache: true` skips network.
- **Cross-repo export**: seed `(rid_a, cid)`, then `Export`/`Has` for `cid` succeed without naming `rid_a` — bytes are served by hash regardless of which repo tagged them.
- **Streaming**: a `Fetch` that emits progress then `okay`, including provider-level (`trying-provider`/`provider-failed`) frames; a client that disconnects mid-stream and a node-side assertion that the temp tag was dropped (partial reclaimable).
- **Abort-safety**: kill mid-download, assert no file at `dest`, no `seeded` tag, and a resumable partial in the store.
- **Idle timeout**: a provider that connects then stalls → `ClientError::Timeout` within `idle` + slack (mirror `download_http_connect_times_out_fast`).

## Resolved decisions

- **`Export` streams** with provider-irrelevant `Exporting` byte progress (no providers involved). It is no longer one-shot.
- **`Fetch` progress is provider-level**: `FetchProgress` is an enum including `TryingProvider`/`ProviderFailed`, mapping the iroh `DownloadProgressItem` kinds the loop already receives.
- **Single shared `Downloader`** on the seeder endpoint, built once at bootstrap (pools connections). Revisit only if concurrent large fetches contend.
- **`fetch --seed` auto-writes the `add_location` COB** after a successful seed, using `FetchReceipt.endpoint_id` (see "The add_location split").
- **HTTP artifacts are seedable**: HTTP downloads route through the store, so HTTP-fetched blobs can be re-served like iroh ones.
- **Cross-repo sharing is intended**: `Has`/`Export` are hash-keyed (`store.blobs().has(hash)`, hash from the CID), not scoped to a `rid`.
- **No node-less fetch for now.** `rad-artifact fetch` requires a running node; the standalone ephemeral path is retired. Could be revisited if it can be reintroduced cheaply, but it is not a goal.