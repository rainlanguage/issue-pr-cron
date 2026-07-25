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
# jq comes via `nix shell` like the producer/vetter runners (not on the bare cron PATH);
# re-exec self inside the shell once so the per-commit loop uses bare jq (one startup,
# not one per commit).
set -euo pipefail
command -v jq >/dev/null 2>&1 || exec nix shell nixpkgs#jq --command "$0" "$@"

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
out="$DIR/human-queue-history.jsonl"
: >"$out"

# --reverse => oldest first; %H = commit sha, %ad with iso-strict = ISO-8601 author date.
git -C "$DIR" log --reverse --date=iso-strict --format='%H %ad' -- human-queue.json |
  while read -r sha ts; do
    # shellcheck disable=SC2016  # $ts is a jq --arg var (single-quoted jq program), not shell
    git -C "$DIR" show "$sha:human-queue.json" 2>/dev/null |
      jq -c --arg ts "$ts" 'select(.counts != null) | {ts: $ts, counts: .counts}' \
        >>"$out" || true
  done

echo "backfilled $(wc -l <"$out") lines into $out"
