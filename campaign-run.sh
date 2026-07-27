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
FALLBACK_MODELS=""             # ordered fallback models tried on a MODEL quota/429 (set in cron.env) — keeps the pipeline moving
MAXTIME="3h"                   # hard cap per run
KEEP_RUNS=2000                 # retained per-run traces (~1.8MB each → ~4GB/~11mo at 6/day; traces are the sole re-derivation source for future metrics and are NOT the disk hog — clones+nix store are, gc'd nightly)
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"

# --- org scope: single source = cron.env ORGS; derive owner-flags + prose, export for pr-review-report ---
: "${ORGS:=rainlanguage cyclofinance}"
export ORGS
OWNER_FLAGS=""; for _o in $ORGS; do OWNER_FLAGS="$OWNER_FLAGS --owner $_o"; done
OWNER_FLAGS="${OWNER_FLAGS# }"
ORGS_HUMAN="$(printf '%s' "$ORGS" | sed -E 's/[[:space:]]+/, /g')"

LOG="$DIR/campaign.log"
LOCK="$DIR/campaign.lock"
RUNDIR="$DIR/runs"
# close/design candidates are GitHub-native now (ai:close-candidate label via
# `pr-review-report flag-close-candidate`; design = human:design + awaiting-ruling comment).
# The local ledgers -- close-candidates.jsonl, design-candidates.jsonl and
# review-verdicts.jsonl -- are retired. GitHub is the source of truth.

# --- kill switch ---
if [ -f "$DIR/DISABLED" ]; then
  echo "$(date -u +%FT%TZ) SKIP: DISABLED flag present" >> "$LOG"
  exit 0
fi

# --- weekly-budget pace gate: skip this tick when usage is over the ceiling or
# running ahead of a linear burn toward the reset. Reads /api/oauth/usage
# itself — see usage-gate.sh ---
if [ -x "$DIR/usage-gate.sh" ]; then
  _ug="$("$DIR/usage-gate.sh")"; _ugrc=$?
  echo "$(date -u +%FT%TZ) usage-gate: $_ug" >> "$LOG"
  [ "$_ugrc" -eq 10 ] && exit 0
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

# The FSM MCP server reads both clone roots from the environment, never from a tool argument — a
# model-supplied root would make its path guard vacuous. WORK_DIR is where clones belong; INSTALL_DIR
# is swept too because it collected `vet-*` clones for months (review-run.sh did not substitute
# {{WORK_DIR}} into the vetter prompt, so the vetter checked out into its cwd).
export WORK_DIR
export INSTALL_DIR="$DIR"

# rotate per-run traces (keep newest $KEEP_RUNS .jsonl + their .err sidecars)
find "$RUNDIR" -maxdepth 1 -name "*.jsonl" -printf "%T@ %p\n" 2>/dev/null | sort -rn | cut -d" " -f2- | tail -n +$((KEEP_RUNS + 1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err"; done
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"

# substitute deployment values into the (path-free) prompt template at runtime
PROMPT="$(sed -e "s#{{WORK_DIR}}#$WORK_DIR#g" \
              -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              -e "s#{{OWNER_FLAGS}}#$OWNER_FLAGS#g" \
              -e "s#{{ORGS}}#$ORGS_HUMAN#g" \
              -e "s#{{INSTALL_DIR}}#$DIR#g" \
              "$DIR/campaign-prompt.txt")"

{
  echo "================================================================="
  echo "$(date -u +%FT%TZ) campaign run START (model=$MODEL, host=$(hostname)) trace=$RUNLOG"
} >> "$LOG"

# Run claude with gh + jq ON PATH (via nix shell) so the model invokes them DIRECTLY:
#   - bare `gh ...` is subject to campaign-settings.json's deny-list (nix-wrapped gh bypasses it),
#   - bare `jq` means dedup is one jq pass, not the byte-grep pathology that stalls runs,
#   - no nix git-hooks WARNING banner leaking into close-candidates.jsonl.
# `--mcp-config campaign-mcp.json` adds the FSM server's PRODUCER profile: clone_create /
# clone_release / clone_list / clone_gc. Work-clone lifecycle is a TOOL rather than shell because the
# `Bash(rm -rf /:*)` deny rule is prefix-matched and so also denied `rm -rf $WORK_DIR/<clone>` — the
# very deletion campaign-prompt mandated (#56). NO `--strict-mcp-config` here, unlike the vetter: the
# producer keeps its Bash and whatever servers its skill plugins bring, and this server is ADDITIVE.
# Stream every event as JSON. tee keeps the full trace even if the jq distiller is missing/errors.
# Model fallback: try $MODEL, then each $FALLBACK_MODELS in order, advancing to the next ONLY when a
# model is quota-limited (HTTP 429 / "reached your … limit"). Any other outcome (success, a nix/auth
# startup failure, or a real error) stops the loop — so one model's exhausted quota can't stall the
# pipeline, yet we never thrash through models on a failure that isn't about quota.
USED_MODEL="$MODEL"
rc=1
for USED_MODEL in $MODEL $FALLBACK_MODELS; do
  echo "$(date -u +%FT%TZ)   model attempt: $USED_MODEL" >> "$LOG"
  timeout "$MAXTIME" nix shell nixpkgs#gh nixpkgs#jq "path:$DIR#pr-review-report" --command claude --print "$PROMPT" \
    --model "$USED_MODEL" \
    --settings "$DIR/campaign-settings.json" \
    --mcp-config "$DIR/campaign-mcp.json" \
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
  # Advance to the next model ONLY on a usage/quota limit (429); any other outcome is final.
  if grep -qiE '"api_error_status": ?429|reached your [^"]*limit|usage limit|session limit' "$RUNLOG" "$ERRLOG" 2>/dev/null; then
    echo "  !! model $USED_MODEL is quota-limited (429) — falling back to next model" >> "$LOG"
    continue
  fi
  break
done

# surface a startup/auth failure (no stdout events) directly into the main log
if [ ! -s "$RUNLOG" ] && [ -s "$ERRLOG" ]; then
  echo "  !! no event stream — likely auth/startup failure; stderr:" >> "$LOG"
  tail -5 "$ERRLOG" | sed 's/^/    /' >> "$LOG"
fi

echo "$(date -u +%FT%TZ) campaign run END (exit=$rc, trace=$RUNLOG, err=$ERRLOG)" >> "$LOG"

# Persist per-run metrics BEFORE the next run's rotation deletes this trace.
# Appends one enriched JSON line to metrics/runs.jsonl (committed periodically,
# never from here — the cron does not push). Best-effort: never fail the run on it.
if [ -s "$RUNLOG" ]; then
  outcome="ok"; [ "$rc" -ne 0 ] && outcome="error"
  grep -qi "session limit\|usage limit" "$RUNLOG" "$ERRLOG" 2>/dev/null && outcome="session-limit"
  mkdir -p "$DIR/metrics"
  # shellcheck disable=SC2016  # $ts/$model/$rc below are jq --arg vars, not shell expansion
  nix run "path:$DIR#pr-review-report" -- run-metrics "$RUNLOG" 2>/dev/null \
    | nix shell nixpkgs#jq --command jq -c --arg ts "$TS" --arg model "$USED_MODEL" --arg oc "$outcome" --argjson rc "$rc" \
      '. + {runId:$ts, role:"producer", model:$model, exitCode:$rc, outcome:$oc}' \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
fi
exit 0
