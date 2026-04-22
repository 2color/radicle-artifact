#!/bin/sh
# radicle-artifact installer
# Usage: curl -sSf https://radworks-releases.s3.fr-par.scw.cloud/rad-artifact/install | sh
#        curl -sSf <URL> | sh -s -- --prefix=/usr/local --version=0.9.0 -y
set -eu

# Pinned version: both the minimum-version sentinel (skip if already met) and
# the version fetched when we need to install. Overridable with --version=X.Y.Z.
RAD_ARTIFACT_VERSION="0.9.0"
RAD_ARTIFACT_BASE="https://radworks-releases.s3.fr-par.scw.cloud/rad-artifact"

info() { printf '  \033[1m%s\033[0m\n' "$*"; }
step() { printf '\n\033[1;32m▸ %s\033[0m\n' "$*"; }
warn() { printf '  \033[33m%s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31merror: %s\033[0m\n' "$*" >&2; exit 1; }

# Compare two dotted version strings: returns 0 if $1 >= $2
version_gte() {
  OLDEST=$(printf '%s\n%s' "$1" "$2" | sort -V | head -1)
  [ "$OLDEST" = "$2" ]
}

usage() {
  cat <<EOF
rad-artifact installer

Usage:
  curl -sSf https://radworks-releases.s3.fr-par.scw.cloud/rad-artifact/install | sh
  curl -sSf <URL> | sh -s -- [OPTIONS]

Options:
  --prefix=PATH       Installation prefix (default: \$RAD_HOME or ~/.radicle)
  --version=X.Y.Z     Install a specific version (default: $RAD_ARTIFACT_VERSION)
  -y, --yes           Accept all prompts (non-interactive)
  -h, --help          Show this help
EOF
}

# Prompt the user; returns 0 for yes, 1 for no.
# Automatically returns 0 when YES_ALL is set.
confirm() {
  if [ "$YES_ALL" = true ]; then
    return 0
  fi
  printf '  %s [Y/n] ' "$1"
  read -r answer </dev/tty
  case "$answer" in
    [nN]*) return 1 ;;
    *)     return 0 ;;
  esac
}

# ── Parse arguments ──────────────────────────────────────────────────
PREFIX=${RAD_HOME:-"$HOME/.radicle"}
YES_ALL=false
VERSION="$RAD_ARTIFACT_VERSION"

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix=*)
      PREFIX=${1#*=}
      ;;
    --version=*)
      VERSION=${1#*=}
      ;;
    -y|--yes)
      YES_ALL=true
      ;;
    -h|--help)
      usage
      exit 0 ;;
    -*)
      fail "Unrecognized argument '$1'" ;;
    *)
      break ;;
  esac
  shift
done

if [ -z "$PREFIX" ]; then
  fail "Empty installation prefix; use --prefix=PATH or set RAD_HOME"
fi

INSTALL_DIR="$PREFIX/bin"

# ── 1. Radicle ────────────────────────────────────────────────────────
# rad-artifact runs inside a Radicle repo and relies on `rad` for identity,
# storage, and node interaction — so we install Radicle first if it's missing.
# We don't auto-create an identity or start the node; the radicle.xyz installer
# handles those prompts itself.
if command -v rad >/dev/null 2>&1; then
  step "Radicle already installed ($(rad --version 2>/dev/null || echo 'unknown version'))"
else
  step "rad-artifact requires the Radicle CLI. Skip if you plan to install it manually."
  if confirm "Install Radicle?"; then
    curl -sSf https://radicle.xyz/install | sh -s -- --prefix="$PREFIX"
  else
    warn "Skipped Radicle installation — rad-artifact won't work end-to-end until 'rad' is on PATH."
  fi
fi

# Ensure a freshly-installed rad is findable in this shell session.
export PATH="$PREFIX/bin:$PATH"

# ── 2. rad-artifact ──────────────────────────────────────────────────
NEED_INSTALL=false

if command -v rad-artifact >/dev/null 2>&1; then
  INSTALLED_VER=$(rad-artifact --version 2>/dev/null | awk '{print $2}') || INSTALLED_VER="0.0.0"
  if version_gte "$INSTALLED_VER" "$VERSION"; then
    step "rad-artifact $INSTALLED_VER already installed (>= $VERSION)"
  else
    step "rad-artifact $INSTALLED_VER is older than $VERSION — updating"
    NEED_INSTALL=true
  fi
else
  NEED_INSTALL=true
fi

if [ "$NEED_INSTALL" = true ]; then
  step "Install rad-artifact $VERSION"
  if confirm "Install rad-artifact?"; then
    case "$(uname)/$(uname -m)" in
      Darwin/arm64)              TARGET="aarch64-apple-darwin" ;;
      Darwin/x86_64)             TARGET="x86_64-apple-darwin" ;;
      Linux/arm64|Linux/aarch64) TARGET="aarch64-unknown-linux-musl" ;;
      Linux/x86_64)              TARGET="x86_64-unknown-linux-musl" ;;
      *) fail "Unsupported platform: $(uname)/$(uname -m)" ;;
    esac

    URL="${RAD_ARTIFACT_BASE}/rad-artifact_${VERSION}_${TARGET}"
    mkdir -p "$INSTALL_DIR"

    info "Downloading rad-artifact ${VERSION} for ${TARGET}..."
    tmp=$(mktemp -t rad-artifact.XXXXXX)
    trap 'rm -f "$tmp"' EXIT
    curl -fSL --retry 3 --retry-delay 2 --progress-bar -o "$tmp" "$URL"
    chmod +x "$tmp"

    # Verify the binary actually runs before placing it on PATH —
    # catches wrong-arch downloads and missing shared libs early.
    if ! "$tmp" --help >/dev/null 2>&1; then
      fail "rad-artifact downloaded but doesn't run (wrong arch or missing shared libs?)"
    fi

    mv "$tmp" "$INSTALL_DIR/rad-artifact"
    trap - EXIT

    # Verify the shell will actually resolve to the binary we just installed.
    # `hash -r` clears any cached path for rad-artifact from this shell.
    hash -r 2>/dev/null || true
    RESOLVED=$(command -v rad-artifact 2>/dev/null || true)
    if [ "$RESOLVED" != "$INSTALL_DIR/rad-artifact" ]; then
      if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        warn "$INSTALL_DIR is not in your PATH"
        warn "Add it with: export PATH=\"$INSTALL_DIR:\$PATH\""
      elif [ -n "$RESOLVED" ]; then
        warn "rad-artifact is shadowed: your shell resolves to $RESOLVED"
        warn "The new $VERSION binary is at $INSTALL_DIR/rad-artifact but won't be used."
        warn "Either remove the shadowing binary, or put $INSTALL_DIR earlier on PATH:"
        warn "  export PATH=\"$INSTALL_DIR:\$PATH\""
      fi
    fi
  else
    warn "Skipped rad-artifact installation"
  fi
fi

# ── Done ──────────────────────────────────────────────────────────────
printf '\n\033[1;32m✓ Ready.\033[0m\n'
info "Run 'rad-artifact --help' to get started, or see https://app.radicle.xyz/nodes/iris.radicle.xyz/rad:z4VYyJ9KuwMNkXGQnmKuGPGKw3inv"
printf '\n'
