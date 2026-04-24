# Releasing rad-artifact

The Makefile builds cross-platform release binaries for `rad-artifact` and
`scp`s them to `files.radicle.dev` alongside the one-line install script.

## Prerequisites

- Rust toolchain with the following targets installed:
  ```sh
  rustup target add aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl
  ```
- `jq` — metadata extraction
- `cargo-zigbuild` (for Linux cross-compilation): `cargo install cargo-zigbuild`
- `zig` (backend for `cargo-zigbuild`): `brew install zig`
- SSH access to `files.radicle.dev` with write permission under
  `/var/www/files.radicle.dev/releases/radicle-artifact/`

## Build

```sh
# From this directory:
make release              # all platforms (macOS host required for macOS targets)
make release-macos        # macOS (aarch64 + x86_64)
make release-linux        # Linux musl (aarch64 + x86_64)
```

`make release` builds both macOS and Linux targets. macOS binaries can only be
built on a macOS host — on Linux or CI, run `make release-linux` and do macOS
builds separately on a Mac.

Binaries are written to the workspace `target/release/` directory as:

```
rad-artifact_<version>_<target-triple>
```

## Upload

```sh
make upload
```

This `scp`s the four built binaries, `install.sh`, and a one-line `latest`
pointer to `files.radicle.dev`, producing this layout on the server:

```
/var/www/files.radicle.dev/releases/radicle-artifact/
├── install                         # stable URL for `curl | sh`
├── latest                          # one-line text file: newest published version
└── <version>/
    ├── rad-artifact_<version>_aarch64-apple-darwin
    ├── rad-artifact_<version>_x86_64-apple-darwin
    ├── rad-artifact_<version>_aarch64-unknown-linux-musl
    └── rad-artifact_<version>_x86_64-unknown-linux-musl
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

## Bumping the version

Update the `version` field in `Cargo.toml` — the Makefile reads it
automatically via `cargo metadata`, and `upload` writes the same value to
the `latest` pointer. Nothing else to edit.

## Cleanup

```sh
make clean                # remove release binaries
make clean-all            # also run cargo clean
```