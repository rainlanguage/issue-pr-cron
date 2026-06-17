#!/usr/bin/env bash
# Durable local runner for the MERGE cron — the final pipeline stage. Drives HUMAN-APPROVED PRs
# (effective source=human, verdict=ready in review-verdicts.jsonl) to merge. Sibling to campaign-run.sh /
# review-run.sh. SAFETY: defaults to DRY-RUN (reports, does not merge) until MERGE_DRY_RUN=0 in cron.env.
#
# Controls (run from the install dir):
#   DISABLE:  touch merge-DISABLED
#   WATCH:    tail -f merge.log
#   RUN NOW:  ./merge-run.sh
#   GO LIVE:  set MERGE_DRY_RUN=0 in cron.env  (otherwise it only reports what it WOULD merge)

set -uo pipefail

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

# --- environment: cron's env is bare. Same HOME/USER/PATH/nix fix as the other runners. ---
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"
export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
# Flag this as a cron run so the block-nix-wrap-gh PreToolUse hook enforces bare gh
# (gh is on PATH below) and closes the deny-list nix-wrap bypass — cron-scoped only.
export RAINIX_CRON_HOOK=1
export PATH="$HOME/.nix-profile/bin:$HOME/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
# shellcheck disable=SC1091
set +u
[ -f "$HOME/.nix-profile/etc/profile.d/nix.sh" ] && . "$HOME/.nix-profile/etc/profile.d/nix.sh"
set -u

# --- deployment config (defaults; override in ./cron.env) ---
PR_ASSIGNEE=""
MERGE_MODEL="claude-sonnet-4-6"
MERGE_MAXTIME="1h"
MERGE_KEEP_RUNS=20
MERGE_DRY_RUN=1                     # SAFE DEFAULT: report only. Set 0 in cron.env to actually merge.
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"

LOG="$DIR/merge.log"
LOCK="$DIR/merge.lock"
RUNDIR="$DIR/merge-runs"
REVIEW_VERDICTS="$DIR/review-verdicts.jsonl"

if [ -f "$DIR/merge-DISABLED" ]; then
  echo "$(date -u +%FT%TZ) SKIP: merge-DISABLED flag present" >> "$LOG"; exit 0
fi

exec 9>"$LOCK"
if ! flock -n 9; then
  echo "$(date -u +%FT%TZ) SKIP: previous merge run still holding the lock" >> "$LOG"; exit 0
fi

mkdir -p "$RUNDIR"
touch "$REVIEW_VERDICTS"
cd "$DIR" || exit 1

ls -1t "$RUNDIR"/*.jsonl 2>/dev/null | tail -n +$((MERGE_KEEP_RUNS+1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err"; done
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"

PROMPT="$(sed -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              -e "s#{{REVIEW_VERDICTS}}#$REVIEW_VERDICTS#g" \
              -e "s#{{DRY_RUN}}#$MERGE_DRY_RUN#g" \
              "$DIR/merge-prompt.txt")"

echo "=================================================================" >> "$LOG"
echo "$(date -u +%FT%TZ) merge run START (model=$MERGE_MODEL, dry_run=$MERGE_DRY_RUN, host=$(hostname)) trace=$RUNLOG" >> "$LOG"

timeout "$MERGE_MAXTIME" nix shell nixpkgs#gh nixpkgs#jq --command claude --print "$PROMPT" \
  --model "$MERGE_MODEL" \
  --settings "$DIR/merge-settings.json" \
  --permission-mode default \
  --verbose --output-format stream-json \
  --add-dir "$DIR" \
  2>"$ERRLOG" \
  | tee "$RUNLOG" \
  | { nix shell nixpkgs#jq --command jq --unbuffered -rc '
        if .type=="assistant" then
          (.message.content[]?
            | if .type=="tool_use" then "  ▸ "+.name+"  "+(((.input.command // .input.description // (.input|tostring)))|tostring|gsub("\n";" ")|.[0:200])
              elif .type=="text" then "  · "+((.text|gsub("\n";" "))|.[0:200])
              else empty end)
        elif .type=="result" then "  ⟹ "+(((.subtype//"done"))|ascii_upcase)+": "+(((.result//"")|gsub("\n";" "))|.[0:800])
        else empty end
      ' 2>/dev/null || cat >/dev/null ; } >> "$LOG"
rc=${PIPESTATUS[0]}

if [ ! -s "$RUNLOG" ] && [ -s "$ERRLOG" ]; then
  echo "  !! no event stream — likely auth/startup failure; stderr:" >> "$LOG"
  tail -5 "$ERRLOG" | sed 's/^/    /' >> "$LOG"
fi

echo "$(date -u +%FT%TZ) merge run END (exit=$rc, dry_run=$MERGE_DRY_RUN, trace=$RUNLOG)" >> "$LOG"
exit 0
