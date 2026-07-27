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

# This script is packaged as a flake output (`packages.campaign-run`), so nix builds its PATH
# from the flake's locked nixpkgs. writeShellApplication forces `set -euo pipefail`; errexit is
# turned back OFF here because this runner is written without it — it checks `rc` explicitly and
# uses `grep -q … &&` as a conditional, both of which would abort the run under errexit.
set +o errexit

# --- self-locate: the INSTALL dir (cron.env, prompts, logs, ledgers) ---
# Not derived from $0 any more: $0 is now a path in the nix store, which is read-only and holds
# none of the run's state. $CRON_DIR is set by the crontab line; it falls back to the working
# directory, which is what an interactive `nix run .#campaign-run` from the checkout gets.
DIR="${CRON_DIR:-$PWD}"
if [ ! -f "$DIR/campaign-prompt.txt" ]; then
  echo "campaign-run: no campaign-prompt.txt in '$DIR' — set CRON_DIR to the install dir" >&2
  exit 1
fi

# --- environment: cron starts bare. Derive HOME for the invoking user. ---
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"
export HOME
# cron's env lacks USER/LOGNAME; some tools reference them, and under `set -u` an unbound USER
# aborts the run before anything logs. Derive them explicitly.
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
# Flag this as a cron run so the block-nix-wrap-gh PreToolUse hook enforces bare gh and closes
# the deny-list nix-wrap bypass — cron-scoped only.
export RAINIX_CRON_HOOK=1
# `claude` is installed by its own npm-based installer into ~/.local/bin and is not a nixpkgs
# package, so it is the one tool the flake cannot put on PATH. APPENDED, so every tool nix does
# provide — gh above all, which campaign-settings.json's deny-list is written against — still
# wins. ~/.nix-profile/bin is deliberately NOT re-added: it put whatever a human last installed
# ahead of the pinned closure (#76 item 3).
export PATH="$PATH:$HOME/.local/bin"

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

# gh, jq and pr-review-report are on PATH as BARE executables, put there by nix from the flake's
# runtimeInputs, so the model invokes them DIRECTLY:
#   - bare `gh ...` is subject to campaign-settings.json's deny-list (a nix-wrapped gh bypasses it),
#   - bare `jq` means dedup is one jq pass, not the byte-grep pathology that stalls runs.
# They are the flake's LOCKED nixpkgs. The old `nix shell` composed the runtime
# at run time from the global registry (channels.nixos.org/nixpkgs-unstable), which floats
# independently of flake.lock — that is how the cron came to run jq 1.8.2 while the lock pinned
# 1.8.1, with no commit to point at.
# `--mcp-config campaign-mcp.json` adds the FSM server's PRODUCER profile: clone_create /
# clone_release / clone_list / clone_gc. Work-clone lifecycle is a TOOL rather than shell because the
# `Bash(rm -rf /:*)` deny rule is prefix-matched and so also denied `rm -rf $WORK_DIR/<clone>` — the
# very deletion campaign-prompt mandated (#56). NO `--strict-mcp-config` here, unlike the vetter: the
# producer keeps its Bash and whatever servers its skill plugins bring, and this server is ADDITIVE.
# Stream every event as JSON. tee keeps the full trace even if the distiller is missing/errors.
# Model fallback: try $MODEL, then each $FALLBACK_MODELS in order, advancing to the next ONLY when a
# model is quota-limited. Any other outcome (success, an auth/startup failure, or a real error) stops
# the loop — so one model's exhausted quota can't stall the pipeline, yet we never thrash through
# models on a failure that isn't about quota.
USED_MODEL="$MODEL"
rc=1
for USED_MODEL in $MODEL $FALLBACK_MODELS; do
  echo "$(date -u +%FT%TZ)   model attempt: $USED_MODEL" >> "$LOG"
  timeout "$MAXTIME" claude --print "$PROMPT" \
    --model "$USED_MODEL" \
    --settings "$DIR/campaign-settings.json" \
    --mcp-config "$DIR/campaign-mcp.json" \
    --permission-mode default \
    --verbose --output-format stream-json \
    --add-dir "$WORK_DIR" \
    --add-dir "$DIR" \
    2>"$ERRLOG" \
    | tee "$RUNLOG" \
    | { pr-review-report distill-trace 2>/dev/null || cat >/dev/null ; } >> "$LOG"
  rc=${PIPESTATUS[0]}
  # Advance to the next model ONLY on a usage/quota limit; any other outcome is final. The verdict
  # is a TYPE computed from the trace's result events (see `classify_run`), not a grep over the
  # trace bytes — the old regex also matched a 429 quoted inside an unrelated tool result, which
  # could skip a model that was never quota-limited at all.
  if [ "$(pr-review-report trace-outcome "$RUNLOG" --exit-code "$rc")" = "session-limit" ]; then
    echo "  !! model $USED_MODEL is quota-limited — falling back to next model" >> "$LOG"
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
# `run-metrics` emits the whole enriched record itself now, including `outcome` — which it derives
# with the same typed classifier the fallback loop uses, so the metrics line and the fallback
# decision can never disagree about whether a run was quota-limited.
if [ -s "$RUNLOG" ]; then
  mkdir -p "$DIR/metrics"
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role producer --model "$USED_MODEL" --exit-code "$rc" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
fi
exit 0
