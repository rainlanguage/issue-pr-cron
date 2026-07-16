#!/usr/bin/env bash
# Weekly-budget PACE GATE for the producer/vetter crons.
#
# Usage IS machine-readable: /api/oauth/usage is the endpoint Claude Code's own
# `/usage` renders from, authenticated with the OAuth credential in
# ~/.claude/.credentials.json. This gate reads it directly, so there is no human
# checkpoint to go stale — an earlier version of this file asserted usage could not
# be read and made the operator paste percentages in by hand, which silently paused
# both crons for 22 consecutive ticks when a reading went stale at 80%.
#
# The response's `seven_day` block carries the weekly bar the crons burn:
#   {"seven_day": {"utilization": 1.0, "resets_at": "2026-07-19T11:59:59Z"}}
# `utilization` is the percentage used and `resets_at` is authoritative — neither is
# inferred, so this gate never has to guess when the week rolls.
#
# Two independent checks, in order:
#   1. CEILING — pause at/over USAGE_CEILING_PCT, whatever the pace.
#   2. PACE    — pause when usage runs more than USAGE_SLACK_PCT ahead of a linear
#      burn toward resets_at.
#
# Reads (from cron.env, all optional):
#   USAGE_CEILING_PCT   float, default 90 — hard ceiling regardless of pace
#   USAGE_SLACK_PCT     float, default 5  — points over linear pace before pausing
#   USAGE_USED_PCT      float — FALLBACK only, used when the endpoint is unreachable
#   USAGE_RESET_AT      ISO-8601 UTC — FALLBACK only, paired with USAGE_USED_PCT
#
# The fallback keeps the crons paced if the endpoint changes or the token expires;
# it is not the normal path. With neither the endpoint nor a fallback, the gate is
# INERT and the crons run: this gate exists to pace spending, not to be a new way
# for the pipeline to stall.
#
# Exit 0  = OK to run (prints a one-line reason).
# Exit 10 = PAUSE this tick; the caller logs the reason and skips (exit 0).
set -u
DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
set +u
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"
set -u

CREDS="${CLAUDE_CREDENTIALS:-$HOME/.claude/.credentials.json}"
USAGE_URL="${USAGE_URL:-https://api.anthropic.com/api/oauth/usage}"

# Fetch into a variable, never a file: the response is account data and the token
# must not reach the process table (curl reads the header from stdin via @-).
usage_json=""
if [ -r "$CREDS" ]; then
  _tok="$(python3 -c 'import json,sys; print((json.load(open(sys.argv[1])).get("claudeAiOauth") or {}).get("accessToken",""))' "$CREDS" 2>/dev/null || true)"
  if [ -n "$_tok" ]; then
    usage_json="$(printf 'header "Authorization: Bearer %s"\n' "$_tok" |
      curl -sS --max-time 20 --config - \
        -H 'anthropic-beta: oauth-2025-04-20' \
        -H 'Content-Type: application/json' \
        "$USAGE_URL" 2>/dev/null || true)"
  fi
  unset _tok
fi

python3 - "$usage_json" "${USAGE_SLACK_PCT:-5}" "${USAGE_CEILING_PCT:-90}" \
  "${USAGE_USED_PCT:-}" "${USAGE_RESET_AT:-}" <<'PY'
import datetime, json, sys

raw = sys.argv[1]
slack = float(sys.argv[2])
ceiling = float(sys.argv[3])
fallback_pct = sys.argv[4]
fallback_reset = sys.argv[5]


def parse(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


now = datetime.datetime.now(datetime.timezone.utc)
used = reset = None
source = "endpoint"

try:
    week = (json.loads(raw) or {}).get("seven_day") or {}
    used = float(week["utilization"])
    reset = parse(week["resets_at"])
except (ValueError, KeyError, TypeError):
    used = reset = None

if used is None and fallback_pct:
    # The endpoint is unreachable — pace on the operator's last reading instead.
    try:
        used = float(fallback_pct)
        reset = parse(fallback_reset) if fallback_reset else None
        source = "fallback reading"
    except ValueError:
        used = None

if used is None:
    print("OK: usage endpoint unreachable and no fallback reading set — gate inert")
    sys.exit(0)

if used >= ceiling:
    print(f"PAUSE: {used:.0f}% of the weekly budget used ({source}) — at/over the {ceiling:.0f}% ceiling")
    sys.exit(10)

if reset is None:
    print(f"OK: {used:.0f}% used ({source}), under the {ceiling:.0f}% ceiling — no reset known, pacing off")
    sys.exit(0)

if now >= reset:
    print(f"OK: reset {reset:%Y-%m-%dT%H:%M:%SZ} has passed ({source}) — new week")
    sys.exit(0)

WEEK = datetime.timedelta(days=7)
frac = (now - (reset - WEEK)).total_seconds() / WEEK.total_seconds()
linear = max(0.0, min(100.0, frac * 100.0))  # where usage "should" be by now at a steady burn

if used - linear > slack:
    print(f"PAUSE: {used:.0f}% used vs {linear:.0f}% linear-by-now toward reset "
          f"{reset:%Y-%m-%dT%H:%M:%SZ} ({source}) — >{slack:.0f}% ahead of pace")
    sys.exit(10)

print(f"OK: {used:.0f}% used vs {linear:.0f}% linear-by-now toward reset "
      f"{reset:%Y-%m-%dT%H:%M:%SZ} ({source}) — within {slack:.0f}% slack")
PY
