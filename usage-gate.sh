#!/usr/bin/env bash
# Weekly-budget PACE GATE for the producer/vetter crons.
#
# The Claude subscription plan's weekly/session usage is NOT machine-readable — it
# is not exposed by any `claude` CLI flag, API endpoint, or local file (the Admin
# usage API is org-only; the "session/weekly limit" errors are claude.ai product
# features signaled only reactively). So the ground truth is a HUMAN-PROVIDED
# checkpoint the operator reads from `/usage` and drops into cron.env; this gate
# paces the crons against it, linearly toward the weekly reset, and PAUSES them
# when our known usage is running ahead of that pace.
#
# Reads (from cron.env, sourced below):
#   USAGE_USED_PCT      integer/float — % of the weekly budget used, per the operator's last reading
#   USAGE_RESET_AT      ISO-8601 UTC  — when the weekly budget resets to 0 (e.g. 2026-07-19T00:00:00Z)
#   USAGE_CHECKPOINT_AT ISO-8601 UTC  — when that reading was taken (recorded for transparency)
#   USAGE_SLACK_PCT     float, default 5 — allow usage to run this many points ahead of linear before pausing
#
# INERT until USAGE_USED_PCT + USAGE_RESET_AT are both set — the crons run normally.
# Exit 0  = OK to run (prints a one-line reason).
# Exit 10 = PAUSE this tick (over pace); the caller logs the reason and skips (exit 0).
#
# To re-pace: the operator gives a fresh reading and updates USAGE_USED_PCT (+ the
# timestamps) in cron.env. The gate re-opens automatically once linear pace catches
# up to the checkpoint, or immediately once USAGE_RESET_AT passes (new week).
set -u
DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
set +u
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"
set -u

[ -n "${USAGE_USED_PCT:-}" ] && [ -n "${USAGE_RESET_AT:-}" ] || exit 0  # inert until set

python3 - "$USAGE_USED_PCT" "$USAGE_RESET_AT" "${USAGE_SLACK_PCT:-5}" "${USAGE_CHECKPOINT_AT:-}" <<'PY'
import sys, datetime

used = float(sys.argv[1])
reset_s = sys.argv[2]
slack = float(sys.argv[3])
checkpoint = sys.argv[4] or "?"

def parse(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))

now = datetime.datetime.now(datetime.timezone.utc)
reset = parse(reset_s)
week = datetime.timedelta(days=7)
week_start = reset - week

# Once the reset passes, the week has rolled over — the old checkpoint is stale, so run.
if now >= reset:
    print(f"OK: reset {reset_s} has passed — new week, checkpoint stale"); sys.exit(0)

frac = (now - week_start).total_seconds() / week.total_seconds()
linear = max(0.0, min(100.0, frac * 100.0))  # where usage "should" be by now at a steady pace

if used - linear > slack:
    print(f"PAUSE: {used:.0f}% used (read {checkpoint}) vs {linear:.0f}% linear-by-now "
          f"toward reset {reset_s} — >{slack:.0f}% ahead of pace")
    sys.exit(10)

print(f"OK: {used:.0f}% used vs {linear:.0f}% linear-by-now (within {slack:.0f}% slack)")
PY
