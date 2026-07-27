#!/usr/bin/env bash
# Regenerate the FSM-conformance snapshot (human-queue.json) and commit it to main so the
# rain-org-health dashboard can fetch it at runtime from the raw URL — no site redeploy for data.
# The snapshot itself is OVERWRITE (point-in-time); alongside it we APPEND one rollup line per
# changed refresh to human-queue-history.jsonl ({ts, counts}, mirroring metrics/runs.jsonl) so the
# dashboard can render per-state inventory over time (Theory-of-Constraints flow panel;
# rain-org-health#32). Data-only, safe unattended. Installed on a cron; see crontab.
# Packaged as a flake output (`packages.refresh-human-queue`); nix builds PATH from the flake's
# locked nixpkgs. errexit is turned back off — writeShellApplication forces it, but this script
# uses `git diff --quiet && exit 0` as a conditional and tolerates best-effort steps.
set +o errexit

# --- locate the install dir + bare-cron env (mirrors campaign-run.sh) ---
# $0 is a read-only nix store path now, so the install dir comes from the crontab's $CRON_DIR,
# defaulting to the working directory for an interactive run from the checkout.
DIR="${CRON_DIR:-$PWD}"
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"; export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
cd "$DIR" || exit 1

# Org scope + assignee: single source is cron.env (same as the producer/vetter).
# shellcheck disable=SC1091
[ -f cron.env ] && . ./cron.env
: "${ORGS:=rainlanguage cyclofinance S01-Issuer}"; export ORGS
export PR_ASSIGNEE

# `pr-review-report` comes from the flake, so it is the binary built from THIS commit. It used to
# be `$DIR/result/bin/pr-review-report` — a gitignored symlink from whenever someone last ran
# `nix build`, checked only for being executable, never for being current. A stale `result` ran
# happily and that is what made the counts keys flap (#76 item 6).

# flock so overlapping ticks never stack.
exec 9>"$DIR/.refresh-human-queue.lock"
flock -n 9 || exit 0

# Regenerate into a temp file; only replace on a non-empty success (never commit a truncated snapshot).
tmp="$(mktemp)"
if pr-review-report human-queue --json >"$tmp" 2>/dev/null && [ -s "$tmp" ]; then
  mv "$tmp" "$DIR/human-queue.json"
else
  rm -f "$tmp"
  echo "refresh-human-queue: generation failed (gh auth / API?), keeping previous snapshot" >&2
  exit 1
fi

# Commit + push only on a real change. Pull first so the push fast-forwards.
git -C "$DIR" diff --quiet -- human-queue.json && exit 0

# Append one rollup line {ts, counts} to the append-only history so the dashboard can
# render per-state inventory over time (Theory-of-Constraints flow panel;
# rain-org-health#32). One line per CHANGED snapshot, mirroring metrics/runs.jsonl.
# counts come straight from the tool-generated snapshot (the tool stays the single
# source of truth); ts is this refresh's real UTC time (never synthesized downstream).
# `queue-history-line` is the same code path the backfill uses, so the live append and the
# historical rewrite can never produce different line shapes for the same snapshot.
ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
pr-review-report queue-history-line "$DIR/human-queue.json" --ts "$ts" \
  >>"$DIR/human-queue-history.jsonl"

git -C "$DIR" pull --ff-only --quiet 2>/dev/null || true
git -C "$DIR" add human-queue.json human-queue-history.jsonl
git -C "$DIR" -c commit.gpgsign=false commit --no-verify -m "chore(dashboard): refresh human-queue.json snapshot" --quiet
git -C "$DIR" push --quiet 2>/dev/null || echo "refresh-human-queue: push failed (main moved?); next tick retries" >&2
