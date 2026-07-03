# 1. camelCase for machine-facing JSON

Status: accepted

## Context

The project emits JSON on two machine-facing surfaces: the CLI `--json`
output and the node control-socket wire protocol. Historically these mixed
conventions: struct fields serialized as snake_case (the serde default),
enum tags as kebab-case, and one system metadata key as `size-bytes`. The
mix was undocumented and unenforced, so new types drifted toward whatever
serde produced by default.

Separately, the release data itself is stored as a radicle collaborative
object (COB). Each `Action` serializes to canonical JSON that is committed
to git, replicated to peers, and signed. Upstream radicle uses camelCase
for its own COB documents.

There is no external consumer of our output yet, so the goal is a single
clean convention that future consumers and contributors can rely on,
rather than compatibility with a specific client.

## Decision

Use **camelCase for every key we author** on machine-facing output:

- Structural / protocol keys in CLI `--json` and the control-socket wire
  (via `#[serde(rename_all = "camelCase")]`, and `rename_all_fields` on
  enums with struct variants).
- System-defined metadata keys we set ourselves, e.g. `sizeBytes`.

Two things are exempt:

- **User-provided metadata keys** pass through verbatim. Metadata always
  lives under a `metadata` object; we never re-case a key the user chose.
- **The COB storage format** (`Action`, `Release`, `Artifact` serde
  representations). Its variant tags (PascalCase) and field names
  (snake_case) are a frozen, signed, replicated wire format. Re-casing a
  serde field or variant name would break deserialization of existing
  entries and their signatures, so it stays as-is regardless of this
  convention.

Note the deliberate asymmetry: our *output* is camelCase while our *COB
storage* stays snake/PascalCase. They serve different audiences (consumers
vs. the replicated log) and have different change costs.

## Consequences

- All `--json` payloads and wire tags are camelCase; `register --json`
  nests the size hint as `{"metadata": {"sizeBytes": N}}`, matching
  `list`/`show`.
- Collections are always present, empty when unset; only optional scalars
  are omitted. `metadata` follows this (always a `{}` object, even when no
  size is recorded), like the sibling `locations`/`attestations`/`redactions`
  arrays, so consumers get a stable shape and never null-check the container.
- Changing existing `--json` keys and wire tags is a breaking change to
  those surfaces; landed as a `feat!`.
- The convention metadata key was renamed `size-bytes` -> `sizeBytes`. A
  metadata-map key is data (not a serde field name), so old entries still
  deserialize; but artifacts registered before the rename keep `size-bytes`
  and render their size as a raw integer rather than a human-readable value.
  No compatibility fallback was added.
- A guard comment on the COB `Action` type points here so the storage
  exemption is not "fixed" by a later contributor.
- Enforcement: `--json` shapes are pinned by snapshot-style tests
  (alongside the existing `wire_snapshot_*` tests) so a forgotten
  `rename_all` fails in review.
