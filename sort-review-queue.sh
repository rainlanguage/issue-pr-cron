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

# Packaged as a flake output: `gh` and the binary come from the flake's locked nixpkgs, so the
# gh-hunting re-exec and the unpinned `cargo build --release` fallback are both gone (#76 items
# 2 and 5). $0 is a read-only nix store path now, so the install dir comes from $CRON_DIR,
# defaulting to $PWD.
cd "${CRON_DIR:-$PWD}" || exit 1

exec pr-review-report queue "${1:-20}"
