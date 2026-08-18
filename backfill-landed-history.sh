#!/usr/bin/env bash
# Backfill (and HEAL) landed-history.jsonl from the git history of human-queue.json, so the
# rain-org-health tokens-per-landed-item band has a real denominator series from day one rather
# than an empty feed.
#
# Walks every consecutive snapshot pair oldest -> newest and runs the SAME
# `pr-review-report landed-history-lines` the live refresh appends with, so the historical rewrite
# and the ongoing append can never produce different rows for the same departure. observedAt = THIS
# RUN's own start time: the backfill is the observer, and its API checks all ran now. The pair's
# historical commit date is NOT used — an item can leave the view open and merge later, and a row
# stamped with the pair date would claim a tick observed a landing that had not yet happened
# (observedAt earlier than ts). ts inside each row is GitHub's own mergedAt/closedAt, which is
# retroactively exact however late this runs.
#
# APPENDS rather than rewriting from scratch: `--existing` skips every (kind,repo,number) already
# recorded, which is what makes this script the healer for live-tick API misses — rerun it and
# only the gaps are re-asked. The known limit it cannot heal: an item that entered AND left the
# queue entirely between two snapshot commits was never observed at all (absence, not a zero; the
# subcommand's own docs state the same).
#
# Packaged as a flake output, so `pr-review-report` and `git` are already on PATH from the flake's
# locked nixpkgs (same reasoning as backfill-human-queue-history.sh).
set -euo pipefail

# $0 is a read-only nix store path now; the repo comes from $CRON_DIR, defaulting to $PWD.
DIR="${CRON_DIR:-$PWD}"
out="$DIR/landed-history.jsonl"
touch "$out"

prev="$(mktemp)"
cur="$(mktemp)"
trap 'rm -f "$prev" "$cur"' EXIT
have_prev=0
misses=0
# One stamp for the whole walk: every row this run emits was observed BY this run.
observed="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# --reverse => oldest first; %H = commit sha (the pair boundary; its date is deliberately unused —
# see the observedAt note in the header).
# Process substitution, not a pipe: the loop mutates $prev/$cur/$misses, and a pipe would run it
# in a subshell where the trap's temp paths and the final miss count stop being the real ones.
while read -r sha; do
  if ! git -C "$DIR" show "$sha:human-queue.json" >"$cur" 2>/dev/null || [ ! -s "$cur" ]; then
    # A commit whose snapshot cannot be read contributes no pair boundary; the next readable
    # snapshot diffs against the last readable one, so nothing is silently treated as vanished.
    continue
  fi
  if [ "$have_prev" -eq 1 ]; then
    # Exit 3 = the emitted rows are complete minus the items stderr names; keep walking — the
    # rerun of this very script is the retry. Buffered, then appended: the subcommand READS
    # $out for its dedup keys, so appending in the same pipeline would have it reading a file
    # it is mid-writing (SC2094) — and a buffered append can never leave a partial row.
    rows="$(pr-review-report landed-history-lines "$prev" "$cur" \
      --observed-at "$observed" --existing "$out")" || misses=$((misses + 1))
    # An explicit if, not `&&`: under errexit a bare false-returning list ends the walk.
    if [ -n "$rows" ]; then
      printf '%s\n' "$rows" >>"$out"
    fi
  fi
  mv "$cur" "$prev"
  cur="$(mktemp)"
  have_prev=1
done < <(git -C "$DIR" log --reverse --format='%H' -- human-queue.json)

echo "landed-history.jsonl now holds $(wc -l <"$out") rows ($misses pair(s) had unresolved items; rerun to heal)"
