.PHONY: release release-macos release-linux upload clean clean-all help

# Version and binary name from Cargo.toml using cargo metadata
VERSION := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.packages[] | select(.name == "radicle-artifact") | .version')
BINARY_NAME := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.packages[] | select(.name == "radicle-artifact") | .targets[] | select(.kind[] == "bin") | .name')
TARGET_DIR := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')
RELEASE_DIR := $(TARGET_DIR)/release

# Upload destination (scp to the Radicle seed server)
UPLOAD_HOST := files.radicle.dev
UPLOAD_PATH := /var/www/files.radicle.dev/releases/radicle-artifact
BASE_URL    := https://files.radicle.dev/releases/radicle-artifact

help:
	@echo "Available targets:"
	@echo "  make release          - Build all architectures (macOS + Linux)"
	@echo "  make release-macos    - Build native macOS architectures (run on macOS)"
	@echo "  make release-linux    - Build Linux musl architectures (cross via zigbuild)"
	@echo "  make upload           - scp binaries + install script to $(UPLOAD_HOST)"
	@echo "  make clean            - Remove built release binaries"
	@echo "  make clean-all        - Also run cargo clean"

# Build all targets. Note: release-macos only works on macOS hosts; run
# release-macos / release-linux individually on single-OS machines.
release: release-macos release-linux
	@echo "✓ All builds complete"

# Build native macOS targets (both arm64 and x86_64)
release-macos:
	@mkdir -p $(RELEASE_DIR)
	@echo "Building for aarch64-apple-darwin..."
	cargo build --release --package radicle-artifact --target aarch64-apple-darwin
	@cp $(TARGET_DIR)/aarch64-apple-darwin/release/$(BINARY_NAME) \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_aarch64-apple-darwin
	@echo "✓ Created: $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_aarch64-apple-darwin"
	@echo "Building for x86_64-apple-darwin..."
	cargo build --release --package radicle-artifact --target x86_64-apple-darwin
	@cp $(TARGET_DIR)/x86_64-apple-darwin/release/$(BINARY_NAME) \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_x86_64-apple-darwin
	@echo "✓ Created: $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_x86_64-apple-darwin"

# Build Linux targets (requires cargo-zigbuild + zig)
# Note: zig >=0.16 has a broken `zig ar` that can't create archives, so we
# override AR for each target to use the system archiver instead.
release-linux:
	@command -v cargo-zigbuild >/dev/null 2>&1 || \
	    (echo "cargo-zigbuild not found. Install with: cargo install cargo-zigbuild" && exit 1)
	@command -v zig >/dev/null 2>&1 || \
	    (echo "zig not found. Install with: brew install zig (or see https://ziglang.org/download/)" && exit 1)
	@mkdir -p $(RELEASE_DIR)
	@echo "Building for aarch64-unknown-linux-musl..."
	AR_aarch64_unknown_linux_musl=/usr/bin/ar \
	    cargo zigbuild --release --package radicle-artifact --target aarch64-unknown-linux-musl
	@cp $(TARGET_DIR)/aarch64-unknown-linux-musl/release/$(BINARY_NAME) \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_aarch64-unknown-linux-musl
	@echo "✓ Created: $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_aarch64-unknown-linux-musl"
	@echo "Building for x86_64-unknown-linux-musl..."
	AR_x86_64_unknown_linux_musl=/usr/bin/ar \
	    cargo zigbuild --release --package radicle-artifact --target x86_64-unknown-linux-musl
	@cp $(TARGET_DIR)/x86_64-unknown-linux-musl/release/$(BINARY_NAME) \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_x86_64-unknown-linux-musl
	@echo "✓ Created: $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_x86_64-unknown-linux-musl"

# Upload binaries and install script to the Radicle seed server via scp.
# Layout on the server:
#   $(UPLOAD_PATH)/install               — stable URL for `curl | sh`
#   $(UPLOAD_PATH)/latest                — one-line text file with newest version
#   $(UPLOAD_PATH)/<version>/<binary>    — per-target binaries, versioned
upload:
	@[ -f install.sh ] || (echo "install.sh missing" && exit 1)
	@for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do \
	    bin="$(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_$$target"; \
	    if [ ! -f "$$bin" ]; then \
	        echo "Missing $$bin — run 'make release' first"; \
	        exit 1; \
	    fi; \
	done
	@echo "Creating $(UPLOAD_PATH)/$(VERSION)/ on $(UPLOAD_HOST)..."
	@ssh $(UPLOAD_HOST) "mkdir -p $(UPLOAD_PATH)/$(VERSION)"
	@echo "Uploading binaries..."
	@scp $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_aarch64-apple-darwin \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_x86_64-apple-darwin \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_aarch64-unknown-linux-musl \
	     $(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_x86_64-unknown-linux-musl \
	     $(UPLOAD_HOST):$(UPLOAD_PATH)/$(VERSION)/
	@echo "Uploading install.sh as $(UPLOAD_PATH)/install..."
	@scp install.sh $(UPLOAD_HOST):$(UPLOAD_PATH)/install
	@# Publish the `latest` pointer last: install.sh reads this to decide which
	@# version to fetch, so it must only flip once the new binaries are live.
	@echo "Updating latest pointer to $(VERSION)..."
	@tmp_latest=$$(mktemp) && printf '%s\n' "$(VERSION)" > "$$tmp_latest" && \
	    scp "$$tmp_latest" $(UPLOAD_HOST):$(UPLOAD_PATH)/latest && \
	    rm -f "$$tmp_latest"
	@echo
	@echo "✓ $(BASE_URL)/$(VERSION)/  (binaries)"
	@echo "✓ $(BASE_URL)/install"
	@echo "✓ $(BASE_URL)/latest → $(VERSION)"

# Clean up built binaries (keep target/ directory structure)
clean:
	@rm -f $(RELEASE_DIR)/$(BINARY_NAME)_*_*-*
	@echo "✓ Cleaned release binaries"

# Clean everything including build artifacts
clean-all: clean
	cargo clean
	@echo "✓ Cleaned all build artifacts"
