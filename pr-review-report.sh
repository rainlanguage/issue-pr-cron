#!/usr/bin/env bash
# pr-review-report.sh — report every open PR (and logged close-candidate) that needs a HUMAN
# decision, reading verdict state from GitHub labels and its own review
# state on top of the CI/mergeability signal. Delegates to the Rust implementation in
# pr-review-report-rs/. Everything prints as full clickable URLs.
#
# Usage:   ./pr-review-report.sh            # all buckets
#          ./pr-review-report.sh --ready    # only the reviewed-&-ready-to-merge bucket
# Config from ./cron.env (ORG, PR_ASSIGNEE), read by the binary.
set -uo pipefail

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
cd "$DIR" || exit 1            # the binary reads cron.env + ledgers from this dir
BIN="$DIR/pr-review-report-rs/target/release/pr-review-report"

# gh is required at runtime; re-exec under nix if it isn't already on PATH.
if ! command -v gh >/dev/null 2>&1 && command -v nix >/dev/null 2>&1; then
  exec nix shell nixpkgs#gh --command "$0" "$@"
fi

if [ ! -x "$BIN" ]; then
  echo "pr-review-report: Rust binary not built; building it now…" >&2
  ( cd "$DIR/pr-review-report-rs" \
      && nix shell nixpkgs#cargo nixpkgs#rustc --command cargo build --release >&2 ) \
    || { echo "pr-review-report: build failed; run: (cd pr-review-report-rs && cargo build --release)" >&2; exit 1; }
fi

exec "$BIN" "$@"
