#!/usr/bin/env bash
# Regenerate the FSM-conformance snapshot (human-queue.json) and commit it to main so the
# rain-org-health dashboard can fetch it at runtime from the raw URL — no site redeploy for data.
# The snapshot itself is OVERWRITE (point-in-time); alongside it we APPEND one rollup line per
# changed refresh to human-queue-history.jsonl ({ts, counts}, mirroring metrics/runs.jsonl) so the
# dashboard can render per-state inventory over time (Theory-of-Constraints flow panel;
# rain-org-health#32). Data-only, safe unattended. Installed on a cron; see crontab.
set -uo pipefail

# --- self-locate + bare-cron env (mirrors campaign-run.sh) ---
DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"; export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
export PATH="$HOME/.nix-profile/bin:$HOME/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
cd "$DIR" || exit 1

# Org scope + assignee: single source is cron.env (same as the producer/vetter).
# shellcheck disable=SC1091
[ -f cron.env ] && . ./cron.env
: "${ORGS:=rainlanguage cyclofinance S01-Issuer}"; export ORGS
export PR_ASSIGNEE

BIN="$DIR/result/bin/pr-review-report"
[ -x "$BIN" ] || { echo "refresh-human-queue: no binary at $BIN (run: nix build .#pr-review-report)" >&2; exit 1; }

# flock so overlapping ticks never stack.
exec 9>"$DIR/.refresh-human-queue.lock"
flock -n 9 || exit 0

# Regenerate into a temp file; only replace on a non-empty success (never commit a truncated snapshot).
tmp="$(mktemp)"
if "$BIN" human-queue --json >"$tmp" 2>/dev/null && [ -s "$tmp" ]; then
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
# jq comes via `nix shell` like the producer/vetter runners (it is not on the bare cron PATH).
ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# shellcheck disable=SC2016  # $ts is a jq --arg var (single-quoted jq program), not shell
nix shell nixpkgs#jq --command jq -c --arg ts "$ts" '{ts: $ts, counts: .counts}' \
  "$DIR/human-queue.json" >>"$DIR/human-queue-history.jsonl"

git -C "$DIR" pull --ff-only --quiet 2>/dev/null || true
git -C "$DIR" add human-queue.json human-queue-history.jsonl
git -C "$DIR" -c commit.gpgsign=false commit --no-verify -m "chore(dashboard): refresh human-queue.json snapshot" --quiet
git -C "$DIR" push --quiet 2>/dev/null || echo "refresh-human-queue: push failed (main moved?); next tick retries" >&2
