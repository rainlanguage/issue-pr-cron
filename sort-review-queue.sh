#!/usr/bin/env bash
# Print the human review queue sorted by verification cost (cheapest first).
# Thin wrapper: delegates to `pr-review-report queue` in pr-review-report-rs/,
# the single owner of ledger parsing (last-wins-by-position over
# GitHub labels) — one parser for the report AND the queue.
#
# Queue = every OPEN, non-draft PR whose effective verdict is ready/ai-campaign.
# Cost from the verdict line's `cost`, else review-costs.jsonl (sha mismatch
# flagged), else unscored (sorts last).
#
# Usage: ./sort-review-queue.sh [N]   (default: top 20; 0 = all)
set -uo pipefail

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
cd "$DIR" || exit 1
BIN="$DIR/pr-review-report-rs/target/release/pr-review-report"

# gh is required at runtime; re-exec under nix if it isn't already on PATH.
if ! command -v gh >/dev/null 2>&1 && command -v nix >/dev/null 2>&1; then
  exec nix shell nixpkgs#gh --command "$0" "$@"
fi

if [ ! -x "$BIN" ]; then
  echo "sort-review-queue: Rust binary not built; building it now…" >&2
  ( cd "$DIR/pr-review-report-rs" \
      && nix shell nixpkgs#cargo nixpkgs#rustc --command cargo build --release >&2 ) \
    || { echo "sort-review-queue: build failed; run: (cd pr-review-report-rs && cargo build --release)" >&2; exit 1; }
fi

exec "$BIN" --queue "${1:-20}"
