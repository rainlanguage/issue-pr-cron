#!/usr/bin/env bash
# One-time backfill of human-queue-history.jsonl from the git history of human-queue.json,
# so the rain-org-health Theory-of-Constraints panel (rain-org-health#32) has per-state
# inventory sparklines on day one rather than an empty feed.
#
# Emits one {ts, counts} line per commit that touched the snapshot: ts = that commit's
# AUTHOR date (real time — never synthesized), counts = the snapshot's .counts at that
# commit. Ordered oldest -> newest so the ongoing refresh appends extend it in order.
# Idempotent: rewrites the file from scratch each run. Matches the line shape that
# refresh-human-queue.sh appends going forward.
#
# Packaged as a flake output, so `pr-review-report` and `git` are already on PATH from the
# flake's locked nixpkgs — the old `command -v jq || exec nix shell …` self-re-exec
# existed only because a bare script could not assume its tools, and is gone.
set -euo pipefail

# $0 is a read-only nix store path now; the repo comes from $CRON_DIR, defaulting to $PWD.
DIR="${CRON_DIR:-$PWD}"
out="$DIR/human-queue-history.jsonl"
: >"$out"

# --reverse => oldest first; %H = commit sha, %ad with iso-strict = ISO-8601 author date.
git -C "$DIR" log --reverse --date=iso-strict --format='%H %ad' -- human-queue.json |
  while read -r sha ts; do
    git -C "$DIR" show "$sha:human-queue.json" 2>/dev/null |
      pr-review-report queue-history-line --ts "$ts" >>"$out" || true
  done

echo "backfilled $(wc -l <"$out") lines into $out"
