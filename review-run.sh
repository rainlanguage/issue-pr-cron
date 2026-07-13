#!/usr/bin/env bash
# Durable local runner for the AI PR-VETTING cron (the "AI review" stage of the merge pipeline).
# Sibling to campaign-run.sh. It reviews open PRs and appends verdicts to review-verdicts.jsonl;
# it NEVER mutates GitHub (read-only gh + write only the local ledger — enforced by review-settings.json).
#
# Controls (run from the install dir):
#   DISABLE:  touch review-DISABLED        (independent of the producer cron's DISABLED)
#   WATCH:    tail -f review.log
#   RUN NOW:  ./review-run.sh
# Deployment values come from ./cron.env (PR_ASSIGNEE, optional REVIEW_MODEL/REVIEW_MAXTIME/REVIEW_KEEP_RUNS).

set -uo pipefail

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

# --- environment: cron's env is bare. Derive HOME/USER, then nix + PATH (same fix as campaign-run.sh). ---
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"
export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
# Flag this as a cron run so the block-nix-wrap-gh PreToolUse hook enforces bare gh
# (gh is on PATH below) and closes the deny-list nix-wrap bypass — cron-scoped only.
export RAINIX_CRON_HOOK=1
export PATH="$HOME/.nix-profile/bin:$HOME/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
set +u
# shellcheck disable=SC1091
[ -f "$HOME/.nix-profile/etc/profile.d/nix.sh" ] && . "$HOME/.nix-profile/etc/profile.d/nix.sh"
set -u

# --- deployment config (defaults; override in ./cron.env) ---
PR_ASSIGNEE=""
REVIEW_MODEL="claude-fable-5"   # org default per 2026-07-04 directive; override via cron.env if needed
FALLBACK_MODELS=""              # ordered fallback models tried on a REVIEW_MODEL quota/429 (set in cron.env)
REVIEW_MAXTIME="2h"
REVIEW_KEEP_RUNS=2000          # ~1.8MB/trace → ~4GB/~11mo at 6/day; sole re-derivation source for future metrics (see campaign-run.sh KEEP_RUNS)
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"

# --- org scope: single source = cron.env ORGS; derive owner-flags + prose, export for pr-review-report ---
: "${ORGS:=rainlanguage cyclofinance}"
export ORGS
OWNER_FLAGS=""; for _o in $ORGS; do OWNER_FLAGS="$OWNER_FLAGS --owner $_o"; done
OWNER_FLAGS="${OWNER_FLAGS# }"
ORGS_HUMAN="$(printf '%s' "$ORGS" | sed -E 's/[[:space:]]+/, /g')"

LOG="$DIR/review.log"
LOCK="$DIR/review.lock"
RUNDIR="$DIR/review-runs"
REVIEW_VERDICTS="$DIR/review-verdicts.jsonl"

# --- kill switch (independent of the producer cron) ---
if [ -f "$DIR/review-DISABLED" ]; then
  echo "$(date -u +%FT%TZ) SKIP: review-DISABLED flag present" >> "$LOG"; exit 0
fi

# --- weekly-budget pace gate (shared with the producer cron; inert until USAGE_* set in
# cron.env — see usage-gate.sh) ---
if [ -x "$DIR/usage-gate.sh" ]; then
  _ug="$("$DIR/usage-gate.sh")"; _ugrc=$?
  echo "$(date -u +%FT%TZ) usage-gate: $_ug" >> "$LOG"
  [ "$_ugrc" -eq 10 ] && exit 0
fi

# --- single-run lock (non-blocking) ---
exec 9>"$LOCK"
if ! flock -n 9; then
  echo "$(date -u +%FT%TZ) SKIP: previous review run still holding the lock" >> "$LOG"; exit 0
fi

mkdir -p "$RUNDIR"
touch "$REVIEW_VERDICTS"
cd "$DIR" || exit 1

# rotate per-run traces
find "$RUNDIR" -maxdepth 1 -name "*.jsonl" -printf "%T@ %p\n" 2>/dev/null | sort -rn | cut -d" " -f2- | tail -n +$((REVIEW_KEEP_RUNS + 1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err"; done
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"

# substitute deployment values into the prompt template
PROMPT="$(sed -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              -e "s#{{REVIEW_VERDICTS}}#$REVIEW_VERDICTS#g" \
              -e "s#{{OWNER_FLAGS}}#$OWNER_FLAGS#g" \
              -e "s#{{ORGS}}#$ORGS_HUMAN#g" \
              "$DIR/review-prompt.txt")"

{
  echo "================================================================="
  echo "$(date -u +%FT%TZ) review run START (model=$REVIEW_MODEL, host=$(hostname)) trace=$RUNLOG"
} >> "$LOG"

# gh + jq on PATH (via nix shell) so the model uses BARE gh -> the read-only deny-list applies.
# Model fallback: try $REVIEW_MODEL, then each $FALLBACK_MODELS in order, advancing ONLY on a
# quota/usage limit (HTTP 429) so one model's exhausted quota can't stall vetting. Any other outcome
# (success, nix/auth startup failure, real error) is final.
USED_MODEL="$REVIEW_MODEL"
rc=1
for USED_MODEL in $REVIEW_MODEL $FALLBACK_MODELS; do
  echo "$(date -u +%FT%TZ)   model attempt: $USED_MODEL" >> "$LOG"
  timeout "$REVIEW_MAXTIME" nix shell nixpkgs#gh nixpkgs#jq "path:$DIR#pr-review-report" --command claude --print "$PROMPT" \
    --model "$USED_MODEL" \
    --settings "$DIR/review-settings.json" \
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
  if grep -qiE '"api_error_status": ?429|reached your [^"]*limit|usage limit|session limit' "$RUNLOG" "$ERRLOG" 2>/dev/null; then
    echo "  !! model $USED_MODEL is quota-limited (429) — falling back to next model" >> "$LOG"
    continue
  fi
  break
done

if [ ! -s "$RUNLOG" ] && [ -s "$ERRLOG" ]; then
  echo "  !! no event stream — likely auth/startup failure; stderr:" >> "$LOG"
  tail -5 "$ERRLOG" | sed 's/^/    /' >> "$LOG"
fi

echo "$(date -u +%FT%TZ) review run END (exit=$rc, verdicts now=$(wc -l < "$REVIEW_VERDICTS" 2>/dev/null), trace=$RUNLOG)" >> "$LOG"

if [ -s "$RUNLOG" ]; then
  outcome="ok"; [ "$rc" -ne 0 ] && outcome="error"
  grep -qi "session limit\|usage limit" "$RUNLOG" 2>/dev/null && outcome="session-limit"
  mkdir -p "$DIR/metrics"
  # shellcheck disable=SC2016  # $ts/$model/$rc below are jq --arg vars, not shell expansion
  nix run "path:$DIR#pr-review-report" -- run-metrics "$RUNLOG" 2>/dev/null \
    | nix shell nixpkgs#jq --command jq -c --arg ts "$TS" --arg model "$USED_MODEL" --arg oc "$outcome" --argjson rc "$rc" \
      '. + {runId:$ts, role:"vetter", model:$model, exitCode:$rc, outcome:$oc}' \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
fi
exit 0
