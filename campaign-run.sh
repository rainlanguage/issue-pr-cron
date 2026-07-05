#!/usr/bin/env bash
# Durable local runner for an autonomous GitHub issue→PR cron.
# Installed via crontab (every 4h). The live engine after the interactive session closes.
#
# Controls (run from the install dir — wherever this script lives):
#   DISABLE:  touch DISABLED          (or `crontab -e` and delete the line)
#   WATCH:    tail -f campaign.log     (distilled trail)
#             tail -f "$(ls -t runs/*.jsonl | head -1)"   (full live trace)
#   RUN NOW:  ./campaign-run.sh
#
# Deployment-specific values live in ./cron.env (gitignored; copy from cron.env.example).
# Guardrails: curated allowlist (campaign-settings.json) + the prompt forbids merge/deploy/
# force-push/issue-close. Concurrency: flock -n so ticks never stack; timeout caps a hung run.

set -uo pipefail

# --- self-locate: the install dir is wherever this script lives (no hardcoded paths) ---
DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

# --- environment: cron starts bare. Derive HOME for the invoking user, then nix + PATH. ---
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"
export HOME
# cron's env lacks USER/LOGNAME; nix.sh and some tools reference them, and under `set -u`
# an unbound USER aborts the run before anything logs. Derive them explicitly.
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
# Flag this as a cron run so the block-nix-wrap-gh PreToolUse hook enforces bare gh
# (gh is on PATH below) and closes the deny-list nix-wrap bypass — cron-scoped only.
export RAINIX_CRON_HOOK=1
export PATH="$HOME/.nix-profile/bin:$HOME/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
# nix.sh is third-party and references unbound vars; relax `set -u` only around it.
set +u
# shellcheck disable=SC1091
[ -f "$HOME/.nix-profile/etc/profile.d/nix.sh" ] && . "$HOME/.nix-profile/etc/profile.d/nix.sh"
set -u

# --- deployment config (defaults here; override in ./cron.env) ---
WORK_DIR="$HOME/code"          # where issue clones are made
PR_ASSIGNEE=""                 # GitHub handle to assign opened PRs to (set in cron.env)
MODEL="claude-fable-5"      # org default per 2026-07-04 directive: max-capability model for both crons
MAXTIME="3h"                   # hard cap per run
KEEP_RUNS=20                   # retained per-run traces
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"

LOG="$DIR/campaign.log"
LOCK="$DIR/campaign.lock"
RUNDIR="$DIR/runs"
CLOSE_CANDIDATES="$DIR/close-candidates.jsonl"
DESIGN_CANDIDATES="$DIR/design-candidates.jsonl"
REVIEW_VERDICTS="$DIR/review-verdicts.jsonl"

# --- kill switch ---
if [ -f "$DIR/DISABLED" ]; then
  echo "$(date -u +%FT%TZ) SKIP: DISABLED flag present" >> "$LOG"
  exit 0
fi

# --- single-run lock (non-blocking: skip this tick if a prior run is still going) ---
exec 9>"$LOCK"
if ! flock -n 9; then
  echo "$(date -u +%FT%TZ) SKIP: previous run still holding the lock" >> "$LOG"
  exit 0
fi

# clones live here; per-run traces here
mkdir -p "$WORK_DIR" "$RUNDIR"
cd "$WORK_DIR" || exit 1

# rotate per-run traces (keep newest $KEEP_RUNS .jsonl + their .err sidecars)
find "$RUNDIR" -maxdepth 1 -name "*.jsonl" -printf "%T@ %p\n" 2>/dev/null | sort -rn | cut -d" " -f2- | tail -n +$((KEEP_RUNS + 1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err"; done
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"

# substitute deployment values into the (path-free) prompt template at runtime
PROMPT="$(sed -e "s#{{WORK_DIR}}#$WORK_DIR#g" \
              -e "s#{{CLOSE_CANDIDATES}}#$CLOSE_CANDIDATES#g" \
              -e "s#{{DESIGN_CANDIDATES}}#$DESIGN_CANDIDATES#g" \
              -e "s#{{REVIEW_VERDICTS}}#$REVIEW_VERDICTS#g" \
              -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              "$DIR/campaign-prompt.txt")"

{
  echo "================================================================="
  echo "$(date -u +%FT%TZ) campaign run START (model=$MODEL, host=$(hostname)) trace=$RUNLOG"
} >> "$LOG"

# Run claude with gh + jq ON PATH (via nix shell) so the model invokes them DIRECTLY:
#   - bare `gh ...` is subject to campaign-settings.json's deny-list (nix-wrapped gh bypasses it),
#   - bare `jq` means dedup is one jq pass, not the byte-grep pathology that stalls runs,
#   - no nix git-hooks WARNING banner leaking into close-candidates.jsonl.
# Stream every event as JSON. tee keeps the full trace even if the jq distiller is missing/errors.
timeout "$MAXTIME" nix shell nixpkgs#gh nixpkgs#jq "path:$DIR#pr-review-report" --command claude --print "$PROMPT" \
  --model "$MODEL" \
  --settings "$DIR/campaign-settings.json" \
  --permission-mode default \
  --verbose --output-format stream-json \
  --add-dir "$WORK_DIR" \
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

# surface a startup/auth failure (no stdout events) directly into the main log
if [ ! -s "$RUNLOG" ] && [ -s "$ERRLOG" ]; then
  echo "  !! no event stream — likely auth/startup failure; stderr:" >> "$LOG"
  tail -5 "$ERRLOG" | sed 's/^/    /' >> "$LOG"
fi

echo "$(date -u +%FT%TZ) campaign run END (exit=$rc, trace=$RUNLOG, err=$ERRLOG)" >> "$LOG"
exit 0
