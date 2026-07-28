#!/usr/bin/env bash
# Durable local runner for the AI PR-VETTING cron (the "AI review" stage of the merge pipeline).
# Sibling to campaign-run.sh. It reviews open PRs and records ONE verdict per PR — an `ai:<verdict>`
# label plus a sha-bound `🤖 ai:vetter` comment — which is its ONLY GitHub write. The vetter runs on
# the FSM MCP surface (see below): the write is a tool, not a command it could vary.
#
# Controls (run from the install dir):
#   DISABLE:  touch review-DISABLED        (independent of the producer cron's DISABLED)
#   WATCH:    tail -f review.log
#   RUN NOW:  ./review-run.sh
# Deployment values come from ./cron.env (PR_ASSIGNEE, optional REVIEW_MODEL/REVIEW_MAXTIME/REVIEW_KEEP_RUNS).

# Packaged as a flake output (`packages.review-run`), so nix builds PATH from the flake's locked
# nixpkgs. writeShellApplication forces `set -euo pipefail`; errexit is turned back OFF because
# this runner checks `rc` explicitly and uses `grep -q … &&` as a conditional — both abort under
# errexit. Same reasoning as campaign-run.sh.
set +o errexit

# --- locate the INSTALL dir (cron.env, prompts, logs). $0 is a read-only nix store path
# now, so the install dir comes from the crontab's $CRON_DIR, defaulting to the working directory
# for an interactive `nix run .#review-run` from the checkout. ---
DIR="${CRON_DIR:-$PWD}"
if [ ! -f "$DIR/review-prompt.txt" ]; then
  echo "review-run: no review-prompt.txt in '$DIR' — set CRON_DIR to the install dir" >&2
  exit 1
fi

# --- environment: cron's env is bare. Derive HOME/USER (same fix as campaign-run.sh). ---
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"
export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME
# Cron-run flag for the block-nix-wrap-gh / block-cron-git-bypass PreToolUse hooks, which are
# scoped to Bash. The vetter's surface has no Bash, so nothing fires; it is set so the hooks
# cover any Bash the session were ever granted.
export RAINIX_CRON_HOOK=1
# `claude` is not a nixpkgs package (npm installer -> ~/.local/bin); it is the one tool the flake
# cannot provide. Appended, so nix-provided tools — `gh` above all — still take precedence.
# ~/.nix-profile/bin is deliberately not re-added (#76 item 3).
export PATH="$PATH:$HOME/.local/bin"

# --- deployment config (defaults; override in ./cron.env) ---
PR_ASSIGNEE=""
REVIEW_MODEL="claude-fable-5"   # org default per 2026-07-04 directive; override via cron.env if needed
FALLBACK_MODELS=""              # ordered fallback models tried on a REVIEW_MODEL quota/429 (set in cron.env)
REVIEW_MAXTIME="2h"
WORK_DIR="$HOME/code"          # where the audit lens checks PRs out (review-prompt {{WORK_DIR}})
REVIEW_KEEP_RUNS=2000          # ~1.8MB/trace → ~4GB/~11mo at 6/day; sole re-derivation source for future metrics (see campaign-run.sh KEEP_RUNS)
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env"
# The gate runs in-process in pr-review-report, so its config must reach the binary as ENV.
# cron.env is sourced above, which makes these shell-local; export them explicitly. Exporting a
# name that cron.env never set is a no-op — bash does not put unset names in the child's env — so
# an unset var stays absent rather than arriving as an empty string.
export USAGE_CEILING_PCT USAGE_SLACK_PCT USAGE_USED_PCT USAGE_RESET_AT USAGE_URL CLAUDE_CREDENTIALS


# --- org scope: single source = cron.env ORGS; derive owner-flags + prose, export for pr-review-report ---
: "${ORGS:=rainlanguage cyclofinance}"
export ORGS
OWNER_FLAGS=""; for _o in $ORGS; do OWNER_FLAGS="$OWNER_FLAGS --owner $_o"; done
OWNER_FLAGS="${OWNER_FLAGS# }"
ORGS_HUMAN="$(printf '%s' "$ORGS" | sed -E 's/[[:space:]]+/, /g')"

LOG="$DIR/review.log"
LOCK="$DIR/review.lock"
RUNDIR="$DIR/review-runs"

# --- kill switch (independent of the producer cron) ---
if [ -f "$DIR/review-DISABLED" ]; then
  echo "$(date -u +%FT%TZ) SKIP: review-DISABLED flag present" >> "$LOG"; exit 0
fi

# --- weekly-budget pace gate: skip this tick when usage is over the ceiling or running ahead of a
# linear burn toward the reset. `usage-gate` reads /api/oauth/usage itself; exit 10 means PAUSE.
# It is INERT when it cannot read usage and no fallback is set — it prints OK and we run. ---
_ug="$(pr-review-report usage-gate)"; _ugrc=$?
echo "$(date -u +%FT%TZ) usage-gate: $_ug" >> "$LOG"
[ "$_ugrc" -eq 10 ] && exit 0

# --- single-run lock (non-blocking) ---
exec 9>"$LOCK"
if ! flock -n 9; then
  echo "$(date -u +%FT%TZ) SKIP: previous review run still holding the lock" >> "$LOG"; exit 0
fi

mkdir -p "$RUNDIR"
# The metrics dir must exist BEFORE the run, not just after it: `run-timings` appends this run's
# boot/ttl records from inside the live pipe, long before the end-of-run `run-metrics` line.
mkdir -p "$DIR/metrics"
cd "$DIR" || exit 1

# rotate per-run traces
find "$RUNDIR" -maxdepth 1 -name "*.jsonl" -printf "%T@ %p\n" 2>/dev/null | sort -rn | cut -d" " -f2- | tail -n +$((REVIEW_KEEP_RUNS + 1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err"; done
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"

# --- tool surface: the FSM MCP server, and nothing else (issue #52) ---------------------------
# The vetter runs against the FSM MCP server in pr-review-report: its whole tool surface is
# `mcp__fsm__{unvetted,pr_context,pr_checkout,record_verdict}` (+ Read/Grep/Glob/Skill/ToolSearch)
# with NO Bash at all, so a non-FSM operation is unrepresentable rather than merely denied — a Bash
# deny-list is prefix-matched and bypassable (`nix shell … --command`).
# `--strict-mcp-config` keeps every other MCP configuration on the box out of the run.
#
# Tool schemas are PRESENTED, not deferred (#78). By default Claude Code defers MCP schemas and the
# vetter spends its first turn on a `ToolSearch` selecting its own eight `mcp__fsm__*` tools by name
# — a round trip to rediscover a fixed allowlist. `ENABLE_TOOL_SEARCH=false` puts the harness in
# standard mode, where the whole surface rides in the preamble. That trade is right HERE and only
# here: the vetter's surface is thirteen tools, deliberately tiny, so the preamble it costs is
# smaller than the turn it saves. The producer keeps deferral — it has Bash and a far larger surface.
#
# Verified 2026-07-27 against claude 2.1.220: unset, the first tool_use of a one-shot run is
# `ToolSearch`; with this export, the first tool_use is the MCP tool itself.
#
# `ToolSearch` stays ALLOWED in review-settings.json on purpose. This export is an optimisation, and
# its failure mode must be the old behaviour, not a dead vetter: if a future harness ignores the
# variable and defers anyway, a vetter that cannot call `ToolSearch` sees its own tools as
# nonexistent and records nothing, silently — that is the #63 failure. Allowed-and-unused costs one
# schema; disallowed-and-needed costs a whole run.
export ENABLE_TOOL_SEARCH=false
PROMPT_FILE="$DIR/review-prompt.txt"
SETTINGS_FILE="$DIR/review-settings.json"
MCP_ARGS=(--mcp-config "$DIR/review-mcp.json" --strict-mcp-config)

# The vetter's audit lens checks PRs out under WORK_DIR (prompt {{WORK_DIR}}; the MCP `pr_checkout`
# tool reads the env var), so it must exist and be exported. INSTALL_DIR is the SECOND clone root the
# FSM server knows about: for months the {{WORK_DIR}} substitution below was missing here, the vetter
# improvised a checkout path, and `vet-*` clones piled up in the install dir — where a WORK_DIR-only
# sweep never looked. Both roots come from the environment, so no tool argument can name its own.
mkdir -p "$WORK_DIR"
export WORK_DIR
export INSTALL_DIR="$DIR"

# substitute deployment values into the prompt template
PROMPT="$(sed -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              -e "s#{{OWNER_FLAGS}}#$OWNER_FLAGS#g" \
              -e "s#{{ORGS}}#$ORGS_HUMAN#g" \
              -e "s#{{WORK_DIR}}#$WORK_DIR#g" \
              "$PROMPT_FILE")"

{
  echo "================================================================="
  echo "$(date -u +%FT%TZ) review run START (model=$REVIEW_MODEL, host=$(uname -n)) trace=$RUNLOG"
} >> "$LOG"

# `gh` is on PATH as a bare executable, put there by nix from the flake's runtimeInputs, for the MCP
# SERVER — it shells out to gh for every GitHub read and for its one write. The vetter model itself
# has no Bash and never invokes it. No `jq` here: the vetter's only jq uses were the trace distiller
# and the metrics merge, both now `pr-review-report` subcommands.
# Model fallback: try $REVIEW_MODEL, then each $FALLBACK_MODELS in order, advancing ONLY on a
# quota/usage limit so one model's exhausted quota can't stall vetting. Any other outcome (success,
# auth/startup failure, real error) is final.
USED_MODEL="$REVIEW_MODEL"
rc=1
for USED_MODEL in $REVIEW_MODEL $FALLBACK_MODELS; do
  echo "$(date -u +%FT%TZ)   model attempt: $USED_MODEL" >> "$LOG"
  timeout "$REVIEW_MAXTIME" claude --print "$PROMPT" \
    --model "$USED_MODEL" \
    --settings "$SETTINGS_FILE" \
    "${MCP_ARGS[@]}" \
    --permission-mode default \
    --verbose --output-format stream-json \
    --add-dir "$DIR" \
    --add-dir "$WORK_DIR" \
    2>"$ERRLOG" \
    | tee "$RUNLOG" \
    | { pr-review-report run-timings --out "$DIR/metrics/runs.jsonl" --trace "$RUNLOG" \
          --run-id "$TS" --role vetter --model "$USED_MODEL" 2>/dev/null || cat ; } \
    | { pr-review-report distill-trace 2>/dev/null || cat >/dev/null ; } >> "$LOG"
  rc=${PIPESTATUS[0]}
  # Typed verdict from the trace's result events, not a grep over the trace bytes (see
  # `classify_trace`): the old regex also matched a 429 quoted inside an unrelated tool result.
  if [ "$(pr-review-report trace-outcome "$RUNLOG" --exit-code "$rc")" = "session-limit" ]; then
    echo "  !! model $USED_MODEL is quota-limited — falling back to next model" >> "$LOG"
    continue
  fi
  break
done

if [ ! -s "$RUNLOG" ] && [ -s "$ERRLOG" ]; then
  echo "  !! no event stream — likely auth/startup failure; stderr:" >> "$LOG"
  tail -5 "$ERRLOG" | sed 's/^/    /' >> "$LOG"
fi

echo "$(date -u +%FT%TZ) review run END (exit=$rc, trace=$RUNLOG)" >> "$LOG"

# `run-metrics` emits the whole enriched record, deriving `outcome` with the same typed classifier
# the fallback loop uses, so the two can never disagree about whether a run was quota-limited.
# This is the run's `stage: final` line. Its `stage: boot` / `stage: ttl` lines were already
# appended mid-run by `run-timings` above — reaching HERE at all is what a killed run cannot do.
if [ -s "$RUNLOG" ]; then
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role vetter --model "$USED_MODEL" --exit-code "$rc" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
fi
exit 0
