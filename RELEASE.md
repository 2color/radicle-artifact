# Releasing rad-artifact

The Makefile builds cross-platform release binaries for `rad-artifact` and
uploads them to Scaleway Object Storage alongside the one-line install script.

## Prerequisites

- Rust toolchain with the following targets installed:
  ```sh
  rustup target add aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl
  ```
- `jq` — metadata extraction
- `cargo-zigbuild` (for Linux cross-compilation): `cargo install cargo-zigbuild`
- `zig` (backend for `cargo-zigbuild`): `brew install zig`
- `s3cmd` configured for Scaleway: `brew install s3cmd`

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

Binaries and the install script are uploaded to Scaleway Object Storage via
`s3cmd`:

```sh
make upload-s3
```

This uploads to `s3://radworks-releases/rad-artifact/` with public-read ACL.
Binaries are then available at:

```
https://radworks-releases.s3.fr-par.scw.cloud/rad-artifact/rad-artifact_<version>_<target-triple>
```

Uploads overwrite existing files with the same name — bump the version in
`Cargo.toml` before re-uploading if you want to preserve the previous release.

Create `~/.s3cfg` with your Scaleway API keys:

```ini
[default]
host_base = s3.fr-par.scw.cloud
host_bucket = %(bucket)s.s3.fr-par.scw.cloud
bucket_location = fr-par
use_https = True
access_key = <SCW_ACCESS_KEY>
secret_key = <SCW_SECRET_KEY>
```

Or generate it with the Scaleway CLI: `scw object config get type=s3cmd region=fr-par`

## Install script

`install.sh` at the repo root is the one-line installer users run via:

```sh
curl -sSf https://radworks-releases.s3.fr-par.scw.cloud/rad-artifact/install | sh
```

`make upload-s3` uploads it alongside the binaries as
`s3://radworks-releases/rad-artifact/install` (no `.sh` suffix, so the URL
reads cleanly). The installer pins the version it fetches — after cutting a
new release, edit `RAD_ARTIFACT_VERSION` in `install.sh` and re-run
`make upload-s3` so new users get the new binary.

## Bumping the version

Update the `version` field in `Cargo.toml` — the Makefile reads it
automatically via `cargo metadata`. Also bump `RAD_ARTIFACT_VERSION` in
`install.sh` so the hosted installer fetches the matching binary.

## Cleanup

```sh
make clean                # remove release binaries
make clean-all            # also run cargo clean
```

## Testing the release pipeline

Start cheap and local; escalate only if earlier steps pass.

### 1. Static checks (seconds)

```sh
sh -n install.sh              # POSIX parse
./install.sh --help           # usage prints, no side effects
make help                     # targets parse
make -n release-macos         # dry-run: verify expanded commands
```

Optional: `brew install shellcheck && shellcheck install.sh` catches quoting
bugs the shell won't.

### 2. Build one binary (~1–3 min)

```sh
rustup target add aarch64-apple-darwin    # if not already installed
make release-macos
./target/release/rad-artifact_<version>_aarch64-apple-darwin --help
```

If `--help` prints, cross-compile and binary-naming are correct. Skip
`release-linux` unless `cargo-zigbuild` + `zig` are installed — the target
fails fast with a clear message otherwise.

### 3. End-to-end install test against a local S3 stand-in

Exercises download → smoke-test → PATH wiring without touching Scaleway.

```sh
# Serve the built binaries so the script can download them
(cd target/release && python3 -m http.server 8000) &
SERVER_PID=$!

# Point a copy of the installer at the local server
sed 's|^RAD_ARTIFACT_BASE=.*|RAD_ARTIFACT_BASE="http://localhost:8000"|' \
    install.sh > /tmp/install-local.sh
chmod +x /tmp/install-local.sh

# Install into a throwaway prefix
TMPPREFIX=$(mktemp -d)
/tmp/install-local.sh --prefix="$TMPPREFIX" -y

# Verify
"$TMPPREFIX/bin/rad-artifact" --version

kill $SERVER_PID
rm -rf "$TMPPREFIX" /tmp/install-local.sh
```

`-y` skips the "Install Radicle?" prompt. To also test the Radicle-missing
path, drop `-y` and answer `n` — the script should warn and continue.

### 4. Publish and test the real URL

Only after steps 1–3 pass:

```sh
make release                  # full build (macOS host; needs zigbuild for Linux)
make upload-s3                # publishes binaries + installer to Scaleway

# Then on a clean shell:
curl -sSf https://radworks-releases.s3.fr-par.scw.cloud/rad-artifact/install \
  | sh -s -- --prefix=$(mktemp -d) -y
```

### What each step catches

| Step | Catches                                                               |
| ---- | --------------------------------------------------------------------- |
| 1    | shell syntax errors, Makefile typos                                   |
| 2    | wrong package/binary name in cargo flags, missing rustup target       |
| 3    | bad URL construction, arch detection, PATH/shadowing, trap cleanup    |
| 4    | S3 ACL / MIME / URL reality, real `curl \| sh` under a fresh env      |
