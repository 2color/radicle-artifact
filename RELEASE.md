# Releasing rad-artifact

A release has two halves:

1. **The crates** — `cargo release` bumps the shared workspace version,
   tags `releases/X.Y.Z` (once, from the `radicle-artifact` crate), pushes
   to the `rad` remote, and publishes all four crates to crates.io in
   dependency order (`radicle-artifact-core` → `radicle-artifact-client` →
   `radicle-artifact`, `radicle-artifact-node`). All crates version in
   lockstep via `[workspace.package]`, so every publish bumps the whole
   workspace together; independent per-crate releases are not supported.
2. **The binaries** — the `Makefile` builds cross-platform binaries
   (`rad-artifact` and `rad-artifact-node`) and `scp`s them to
   `files.radicle.dev` alongside the one-line install script.

## Prerequisites

- [`cargo-release`](https://github.com/crate-ci/cargo-release) — crate release automation
- [`git-cliff`](https://git-cliff.org/) — changelog drafting
- Rust toolchain with cross-compilation targets:
  ```sh
  rustup target add aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl
  ```
- `jq` — metadata extraction
- `cargo-zigbuild` (for Linux cross-compilation): `cargo install cargo-zigbuild`
- `zig` (backend for `cargo-zigbuild`): `brew install zig`
- SSH access to `files.radicle.dev` with write permission under
  `/var/www/files.radicle.dev/releases/radicle-artifact/`. The Makefile uses
  your local `$USER`; if the remote account differs, add a matching
  `User` entry in `~/.ssh/config` for this host.

## Full release flow

```sh
# 1. Draft the commit list for the upcoming release, then hand-edit prose.
#    Commit whenever — this can happen well before release day.
make changelog                 # prepends commit list under [Unreleased]
$EDITOR CHANGELOG.md           # add prose summary, reorder, drop noise
git commit -am "Update changelog"

# 2. Cut the crate release with a clean working tree.
#    This bumps Cargo.toml, rewrites the [Unreleased] header to the version,
#    commits, tags `releases/X.Y.Z`, pushes, and publishes to crates.io.
cargo release X.Y.Z --execute

# 3. Build and upload binaries + install script
make build
make upload

# 4. Record each binary's CID and download URL in the release COB.
#    Runs after upload so the announced URL is live.
make register-artifacts
```

You can use `minor`, `major`, or `patch` instead of an explicit version, and
`cargo release` will compute the next version automatically. Omit `--execute`
to dry-run.

## Changelog

The changelog is written by hand, using an auto-generated commit list as a
starting point. `CHANGELOG.md` always has an `## [Unreleased]` section at
the top that accumulates prose between releases; `cargo release` rewrites
that header to `## [X.Y.Z] - YYYY-MM-DD` at release time (via
`pre-release-replacements` in `release.toml`) and folds the rewrite into
the release commit.

```sh
# Drop the commit list for the upcoming release under [Unreleased]:
make changelog

# Then open CHANGELOG.md in your editor and add prose.
```

`make changelog` wraps `git cliff --unreleased --prepend CHANGELOG.md`.
`--prepend` inserts the new section at the top without touching older
entries, and `git cliff --unreleased` with no `--tag` emits the heading
as `## [Unreleased]`, which is exactly what the replacement regex matches.
After running it, open `CHANGELOG.md` in your editor to add a prose
summary, reorder entries, and drop noise, keeping the auto-generated
commit list below the prose. Commit the result whenever — it doesn't need
to happen at release time.

To preview the commit list without writing the file:
`git cliff --unreleased`.

**Caveat:** `--prepend` unconditionally adds a new section each time it
runs. If commits land after you've already prepended, either add them to
the existing `[Unreleased]` section by hand, or delete that section and
re-run `--prepend` to regenerate with the full list (you'll need to
re-paste your prose).

After the release, add a fresh empty `## [Unreleased]` heading back to the
top of `CHANGELOG.md` so the next cycle has something to accumulate under.

## Build

```sh
make build                # all platforms (macOS host required for macOS targets)
make build-macos          # macOS (aarch64 + x86_64)
make build-linux          # Linux musl (aarch64 + x86_64)
```

`make build` builds both macOS and Linux targets. macOS binaries can only be
built on a macOS host — on Linux or CI, run `make build-linux` and do macOS
builds separately on a Mac.

The version comes from the workspace `Cargo.toml` via `cargo metadata`, so
`cargo release` in step 2 is the only place it needs to be set. Both
binaries are written to `target/release/` as:

```
rad-artifact_<version>_<target-triple>
rad-artifact-node_<version>_<target-triple>
```

## Upload

```sh
make upload
```

This `scp`s the built binaries (two per target triple), `install.sh`, and a one-line `latest`
pointer to `files.radicle.dev`, producing this layout on the server:

```
/var/www/files.radicle.dev/releases/radicle-artifact/
├── install                         # stable URL for `curl | sh`
├── latest                          # one-line text file: newest published version
└── <version>/
    ├── rad-artifact_<version>_<target-triple>          # one per triple
    └── rad-artifact-node_<version>_<target-triple>     # one per triple
```

Public URLs:

```
https://files.radicle.dev/releases/radicle-artifact/install
https://files.radicle.dev/releases/radicle-artifact/latest
https://files.radicle.dev/releases/radicle-artifact/<version>/rad-artifact_<version>_<target-triple>
```

Per-version directories preserve historical releases automatically. The
`latest` pointer is uploaded **after** the binaries, so there's never a
window where `latest` advertises a version whose binaries aren't yet on disk.

Re-running `make upload` at the same version overwrites that version's files
but leaves other versions untouched.

## Register artifacts

```sh
make register-artifacts
```

This dogfoods `rad-artifact` on its own release. For each built binary, it
registers the artifact in the release COB tagged `releases/X.Y.Z` (the BLAKE3
CID is computed from the file contents) and announces the `files.radicle.dev`
URL as a discovery location:

```
rad-artifact register <binary> --revision releases/X.Y.Z --name <binary> --json
rad-artifact location add --cid <CID> --revision releases/X.Y.Z <public-url>
```

`register --json` prints `{cid, release_id, revision}`; the CID is read back
from there and passed to `location add`. The COB is created on the first
`register` and reused for the remaining binaries. After this step, users can
discover and fetch releases with:

```sh
rad-artifact list
rad-artifact fetch releases/X.Y.Z --cid <CID>
```

Run this **after** `make upload` so the announced URL resolves. Re-running is
safe: `register` with the same `(revision, cid)` from the same author is
idempotent, and `location add` dedups URLs.

Unlike `make build` / `make upload`, this step talks to your local Radicle
profile and the network, so it requires `rad auth` to be configured with a
key that has publishing rights on this repo. Inspect the result with
`rad-artifact show releases/X.Y.Z --pretty`.

## Install script

`install.sh` at the repo root is the one-line installer users run via:

```sh
curl -sSf https://files.radicle.dev/releases/radicle-artifact/install | sh
```

`make upload` copies it to the server as `install` (no `.sh` suffix, so the
URL reads cleanly). The installer has no hardcoded version — at runtime it
reads `/latest` to decide which binary to fetch. So a single `make upload`
publishes the binaries, the script, and the pointer in one shot. Users can
still pin with `--version=X.Y.Z`.

## Cleanup

```sh
make clean                # remove release binaries
make clean-all            # also run cargo clean
```