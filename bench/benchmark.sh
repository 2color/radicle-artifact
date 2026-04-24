#!/usr/bin/env bash
# Benchmark compute_content_id over a deterministic corpus.
# Run from the worktree root. Emits "duration: <ms> ms" on stdout.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"

# Keep the target dir inside the worktree so different worktrees don't clobber
# each other's build cache.
export CARGO_TARGET_DIR="$REPO/bench/target"

cargo run --manifest-path "$REPO/Cargo.toml" -p cc-bench --release --quiet
