.PHONY: changelog release release-macos release-linux check-macos-host upload register-artifacts check-bins clean clean-all check help

# Version from the workspace (all crates version in lockstep)
VERSION := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.packages[] | select(.name == "radicle-artifact") | .version')
# Both shipped binaries: the CLI and the seeding daemon it spawns.
BINARIES := rad-artifact rad-artifact-node
TARGET_DIR := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')
RELEASE_DIR := $(TARGET_DIR)/release

# Target triples. Adding a new triple: append here, set BUILD_CMD_<triple>
# below, and (if applicable) group it into MACOS_TARGETS / LINUX_TARGETS.
MACOS_TARGETS := aarch64-apple-darwin x86_64-apple-darwin
LINUX_TARGETS := aarch64-unknown-linux-musl x86_64-unknown-linux-musl
ALL_TARGETS   := $(MACOS_TARGETS) $(LINUX_TARGETS)

# Per-triple build command. Linux targets cross-compile via cargo-zigbuild and
# override AR because zig >=0.16 ships a broken `zig ar`.
BUILD_CMD_aarch64-apple-darwin       := cargo build
BUILD_CMD_x86_64-apple-darwin        := cargo build
BUILD_CMD_aarch64-unknown-linux-musl := AR_aarch64_unknown_linux_musl=/usr/bin/ar cargo zigbuild
BUILD_CMD_x86_64-unknown-linux-musl  := AR_x86_64_unknown_linux_musl=/usr/bin/ar cargo zigbuild

# Final artifact paths derived once and reused by upload / register-artifacts.
RELEASE_BINS := $(foreach t,$(ALL_TARGETS),$(foreach b,$(BINARIES),$(RELEASE_DIR)/$(b)_$(VERSION)_$(t)))

# Upload destination (scp to the Radicle seed server)
UPLOAD_HOST := files.radicle.dev
UPLOAD_PATH := /var/www/files.radicle.dev/releases/radicle-artifact
BASE_URL    := https://files.radicle.dev/releases/radicle-artifact

help:
	@echo "Available targets:"
	@echo "  make check            - Build, fmt, and clippy"
	@echo "  make changelog        - Prepend commit list since last tag to CHANGELOG.md under [Unreleased]"
	@echo "  make release          - Build all architectures (macOS + Linux)"
	@echo "  make release-macos    - Build native macOS architectures (run on macOS)"
	@echo "  make release-linux    - Build Linux musl architectures (cross via zigbuild)"
	@echo "  make upload           - scp binaries + install script to $(UPLOAD_HOST)"
	@echo "  make register-artifacts - Record binary CIDs + download URLs in the release COB"
	@echo "  make clean            - Remove built release binaries"
	@echo "  make clean-all        - Also run cargo clean"

check:
	cargo fmt --check
	cargo clippy

# Draft the changelog section for the upcoming release. Prepends a new
# [Unreleased] block with the commit list since the last tag via git-cliff,
# then opens $EDITOR for hand-written prose.
changelog:
	@command -v git-cliff >/dev/null 2>&1 || \
	    (echo "git-cliff not found. Install with: cargo install git-cliff" && exit 1)
	git cliff --unreleased --prepend CHANGELOG.md

# Build all targets. Note: release-macos only works on macOS hosts; run
# release-macos / release-linux individually on single-OS machines.
release: release-macos release-linux
	@echo "✓ All builds complete"

# Per-triple build rule. The stem ($*) is the target triple; BUILD_CMD_$*
# selects the cargo invocation (plain cargo for macOS, cargo-zigbuild with
# an AR override for linux-musl). Final binary is renamed to include the
# version and triple, matching what `upload` and `register-artifacts` expect.
build-%:
	@mkdir -p $(RELEASE_DIR)
	@echo "Building for $*..."
	$(BUILD_CMD_$*) --release --package radicle-artifact --package radicle-artifact-node --target $*
	@for b in $(BINARIES); do \
	    cp $(TARGET_DIR)/$*/release/$$b $(RELEASE_DIR)/$${b}_$(VERSION)_$*; \
	    echo "✓ Created: $(RELEASE_DIR)/$${b}_$(VERSION)_$*"; \
	done

# macOS targets build with plain `cargo build`, which has no Darwin cross
# toolchain on other hosts; guard so a Linux `make release` fails fast with a
# clear message
release-macos: check-macos-host $(addprefix build-,$(MACOS_TARGETS))

check-macos-host:
	@[ "$$(uname -s)" = "Darwin" ] || \
	    (echo "release-macos must run on a macOS host (got $$(uname -s)). Run 'make release-linux' here instead." && exit 1)

# Linux targets also need cargo-zigbuild + zig installed.
release-linux: check-zig $(addprefix build-,$(LINUX_TARGETS))

check-zig:
	@command -v cargo-zigbuild >/dev/null 2>&1 || \
	    (echo "cargo-zigbuild not found. Install with: cargo install cargo-zigbuild" && exit 1)
	@command -v zig >/dev/null 2>&1 || \
	    (echo "zig not found. Install with: brew install zig (or see https://ziglang.org/download/)" && exit 1)

# Pre-flight check: every release binary must exist on disk. Shared by
# `upload` and `register-artifacts` so both fail early with the same
# message if `make release` hasn't been run.
check-bins:
	@for bin in $(RELEASE_BINS); do \
	    if [ ! -f "$$bin" ]; then \
	        echo "Missing $$bin — run 'make release' first"; \
	        exit 1; \
	    fi; \
	done

# Upload binaries and install script to the Radicle seed server via scp.
# Layout on the server:
#   $(UPLOAD_PATH)/install               — stable URL for `curl | sh`
#   $(UPLOAD_PATH)/latest                — one-line text file with newest version
#   $(UPLOAD_PATH)/<version>/<binary>    — per-target binaries, versioned
upload: check-bins
	@[ -f install.sh ] || (echo "install.sh missing" && exit 1)
	@echo "Creating $(UPLOAD_PATH)/$(VERSION)/ on $(UPLOAD_HOST)..."
	@ssh $(UPLOAD_HOST) "mkdir -p $(UPLOAD_PATH)/$(VERSION)"
	@echo "Uploading binaries..."
	@scp $(RELEASE_BINS) $(UPLOAD_HOST):$(UPLOAD_PATH)/$(VERSION)/
	@echo "Uploading install.sh as $(UPLOAD_PATH)/install..."
	@scp install.sh $(UPLOAD_HOST):$(UPLOAD_PATH)/install
	@# Publish the `latest` pointer last: install.sh reads this to decide which
	@# version to fetch, so it must only flip once the new binaries are live.
	@echo "Updating latest pointer to $(VERSION)..."
	@# chmod 644 before scp: mktemp creates files mode 0600 and scp preserves
	@# that, which would leave the webserver unable to read `latest` (403).
	@tmp_latest=$$(mktemp) && printf '%s\n' "$(VERSION)" > "$$tmp_latest" && \
	    chmod 644 "$$tmp_latest" && \
	    scp "$$tmp_latest" $(UPLOAD_HOST):$(UPLOAD_PATH)/latest && \
	    rm -f "$$tmp_latest"
	@echo
	@echo "✓ $(BASE_URL)/$(VERSION)/  (binaries)"
	@echo "✓ $(BASE_URL)/install"
	@echo "✓ $(BASE_URL)/latest → $(VERSION)"

# Record each built binary as an artifact in the release COB tagged
# `releases/$(VERSION)`, and attach its public download URL as a location.
# Runs AFTER `make upload` so the announced URL is live before it's published.
# Uses `cargo run` so we don't depend on a pre-installed `rad-artifact` on PATH.
register-artifacts: check-bins
	@RAD_ARTIFACT="cargo run --release --quiet --package radicle-artifact --bin rad-artifact --"; \
	for target in $(ALL_TARGETS); do \
	  for b in $(BINARIES); do \
	    name="$${b}_$(VERSION)_$$target"; \
	    bin="$(RELEASE_DIR)/$$name"; \
	    url="$(BASE_URL)/$(VERSION)/$$name"; \
	    echo "→ $$name"; \
	    json=$$($$RAD_ARTIFACT --no-input register "$$bin" --revision "releases/$(VERSION)" --name "$$name" --json) || exit 1; \
	    cid=$$(echo "$$json" | jq -r '.cid'); \
	    echo "   cid: $$cid"; \
	    $$RAD_ARTIFACT --no-input location add --cid "$$cid" --revision "releases/$(VERSION)" "$$url" || exit 1; \
	  done; \
	done
	@echo
	@echo "✓ Registered $(words $(RELEASE_BINS)) artifacts under releases/$(VERSION)"
	@echo "  Inspect with: rad-artifact show releases/$(VERSION) --pretty"

# Clean up built binaries (keep target/ directory structure)
clean:
	@for b in $(BINARIES); do rm -f $(RELEASE_DIR)/$${b}_*_*-*; done
	@echo "✓ Cleaned release binaries"

# Clean everything including build artifacts
clean-all: clean
	cargo clean
	@echo "✓ Cleaned all build artifacts"
