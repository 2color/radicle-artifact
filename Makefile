.PHONY: release release-macos release-linux upload-s3 clean clean-all help

# Version and binary name from Cargo.toml using cargo metadata
VERSION := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.packages[] | select(.name == "radicle-artifact") | .version')
BINARY_NAME := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.packages[] | select(.name == "radicle-artifact") | .targets[] | select(.kind[] == "bin") | .name')
TARGET_DIR := $(shell cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')
RELEASE_DIR := $(TARGET_DIR)/release

# Scaleway S3 settings
SCW_BUCKET := radworks-releases
SCW_REGION := fr-par
SCW_ENDPOINT := https://s3.$(SCW_REGION).scw.cloud
SCW_PREFIX := rad-artifact

help:
	@echo "Available targets:"
	@echo "  make release          - Build all architectures (macOS + Linux)"
	@echo "  make release-macos    - Build native macOS architectures (run on macOS)"
	@echo "  make release-linux    - Build Linux musl architectures (cross via zigbuild)"
	@echo "  make upload-s3        - Upload release binaries and install script to Scaleway S3"
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

# Upload release binaries and install script to Scaleway S3.
# Requires s3cmd configured for Scaleway (see RELEASE.md).
upload-s3:
	@command -v s3cmd >/dev/null 2>&1 || \
	    (echo "s3cmd not found. Install with: brew install s3cmd" && exit 1)
	@for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do \
	    bin="$(RELEASE_DIR)/$(BINARY_NAME)_$(VERSION)_$$target"; \
	    if [ ! -f "$$bin" ]; then \
	        echo "⚠ Skipping $$target (not built)"; \
	        continue; \
	    fi; \
	    echo "Uploading $$target..."; \
	    s3cmd put --acl-public "$$bin" \
	        "s3://$(SCW_BUCKET)/$(SCW_PREFIX)/$(BINARY_NAME)_$(VERSION)_$$target"; \
	    echo "✓ https://$(SCW_BUCKET).s3.$(SCW_REGION).scw.cloud/$(SCW_PREFIX)/$(BINARY_NAME)_$(VERSION)_$$target"; \
	done
	@if [ -f install.sh ]; then \
	    echo "Uploading install.sh..."; \
	    s3cmd put --acl-public --mime-type=text/x-shellscript install.sh \
	        "s3://$(SCW_BUCKET)/$(SCW_PREFIX)/install"; \
	    echo "✓ https://$(SCW_BUCKET).s3.$(SCW_REGION).scw.cloud/$(SCW_PREFIX)/install"; \
	else \
	    echo "⚠ Skipping install.sh (not present)"; \
	fi

# Clean up built binaries (keep target/ directory structure)
clean:
	@rm -f $(RELEASE_DIR)/$(BINARY_NAME)_*_*-*
	@echo "✓ Cleaned release binaries"

# Clean everything including build artifacts
clean-all: clean
	cargo clean
	@echo "✓ Cleaned all build artifacts"
