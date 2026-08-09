#!/usr/bin/env bash
# design-doctor.sh — the FSM doctor pass for the design lane (#241): route every ai:design PR
# with NO live trusted design question back to ai:needs-work, with the trusted work order
# (re-flag or proceed) posted at the current head. Detection is next_design's own classifier;
# the whole pass is ONE tested subcommand (`pr-review-report design-doctor`) and this wrapper
# only adds what a bare cron invocation cannot: the install-dir env, the org scope from
# cron.env, stamped logging, and a flock so overlapping ticks never stack.
# Installed on a daily cron; see crontab (README "Schedule & controls").
# Packaged as a flake output (`packages.design-doctor`); `gh` and the binary come from the
# flake's locked nixpkgs. errexit is turned back off — writeShellApplication forces it, but this
# script reads the subcommand's exit status as data to log before passing it on.
set +o errexit

# --- locate the install dir + bare-cron env (mirrors refresh-human-queue.sh) ---
# $0 is a read-only nix store path, so the install dir comes from the crontab's $CRON_DIR,
# defaulting to the working directory for an interactive run from the checkout.
DIR="${CRON_DIR:-$PWD}"
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"; export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME

# Every line is stamped, same format as the other cron logs, so design-doctor.log can answer
# "when did this last run / fail". Everything goes to stderr, one stream, so the crontab's
# `>> …log 2>&1` preserves the order.
log() { echo "$(date -u +%FT%TZ) design-doctor: $*" >&2; }

cd "$DIR" || { log "install dir '$DIR' is not usable — set CRON_DIR to the checkout"; exit 1; }

# --- deployment config (defaults here; override in ./cron.env) ---
# A hard cap, as both model runners carry (MAXTIME=3h, REVIEW_MAXTIME=2h): an unattended writer
# with no cap can hold its flock for ever, and every later tick then logs "a previous tick still
# holds the lock" while nothing drains. This pass is a bounded number of `gh` calls, so its cap is
# small — long enough for a large backlog on a slow API, short enough that a wedged tick is gone
# before the next one.
DOCTOR_MAXTIME="30m"                   # hard cap per tick

# Org scope + fleet: single source is cron.env (same as the producer/vetter/refresher). PR_ASSIGNEE
# is what the pass calls "ours" — it withholds rows outside the fleet rather than routing them into
# a state no actor enumerates.
# shellcheck disable=SC1091
[ -f cron.env ] && . ./cron.env
: "${ORGS:=rainlanguage cyclofinance S01-Issuer}"; export ORGS
export PR_ASSIGNEE

# --- kill switch ---
# The producer's own flag, not a third one. The README documents "Pause: touch DISABLED" directly
# beneath this cron's line, so an operator halting the pipeline believes everything is stopped —
# and this pass STRIPS LABELS and POSTS TRUSTED COMMENTS across every org in ORGS. A writer that
# ignores the pause is the one runner whose ticks a halted operator cannot undo.
if [ -f "$DIR/DISABLED" ]; then
  log "SKIP: DISABLED flag present"
  exit 0
fi

# flock so overlapping ticks never stack.
exec 9>"$DIR/.design-doctor.lock"
flock -n 9 || { log "skipped: a previous tick still holds the lock"; exit 0; }

log "tick start (cap $DOCTOR_MAXTIME)"
timeout "$DOCTOR_MAXTIME" pr-review-report design-doctor "$@"
rc=$?
[ "$rc" -eq 124 ] && log "TIMED OUT after $DOCTOR_MAXTIME — the tick was cut off; rows it had not reached are re-enumerated next tick"
log "tick end (rc=$rc)"
exit "$rc"
