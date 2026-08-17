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
#   FORCE:    ./review-run.sh --force      (one run, past every POLICY stop — the usage-gate PAUSE
#                                          and review-DISABLED — streamed to stdout. Never past a
#                                          CORRECTNESS stop: the lock, or a gate config refusal.)
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
# an unset var stays absent rather than arriving as an empty string. USAGE_SLACK_PCT is RETIRED
# (#158) but still exported: the gate REFUSES it when set, and dropping it from this list would
# hide a stale cron.env from that guard instead of surfacing it.
export USAGE_CEILING_PCT USAGE_HEADROOM_PCT USAGE_SLACK_PCT USAGE_USED_PCT USAGE_RESET_AT USAGE_URL CLAUDE_CREDENTIALS


# --- org scope: single source = cron.env ORGS; derive owner-flags + prose, export for pr-review-report ---
: "${ORGS:=rainlanguage cyclofinance}"
export ORGS
OWNER_FLAGS=""; for _o in $ORGS; do OWNER_FLAGS="$OWNER_FLAGS --owner $_o"; done
OWNER_FLAGS="${OWNER_FLAGS# }"
ORGS_HUMAN="$(printf '%s' "$ORGS" | sed -E 's/[[:space:]]+/, /g')"

LOG="$DIR/review.log"
LOCK="$DIR/review.lock"
RUNDIR="$DIR/review-runs"

# --- one-off manual FORCE (#245) ---------------------------------------------------------------
# Identical in shape and in wording to campaign-run.sh's, because a force that works on the producer
# and silently does nothing on the vetter is worse than no force at all.
#
#   CRON_DIR=<install-dir> nix run git+file://<install-dir>#review-run -- --force
#
# The bare `--` is LOAD-BEARING: `nix run` parses everything up to it as its own flags and exits
# with `unrecognised flag '--force'` otherwise, so the runner never starts. Everything after it is
# handed to the packaged script as argv, verified against nix 2.18.1 through the `git+file:` form
# the crontab uses.
#
# An ARGUMENT and not a variable, deliberately: an argument belongs to one invocation and cannot be
# left switched on, where a force in cron.env would silently force every scheduled tick for ever.
# CRON_FORCE is refused below so that door is shut rather than merely unused.
#
# What `--force` overrides is ONE PROPERTY, not a list — the list is what got this wrong the first
# time (#245 shipped a force that refused the kill switch, and the first observation run it was
# built for printed `SKIP: DISABLED flag present` and did nothing):
#
#   --force overrides POLICY stops. It never overrides CORRECTNESS stops.
#
# A POLICY stop is the pipeline choosing not to spend right now. The human at the terminal owns that
# choice and is allowed to make it differently for one run: the usage-gate PAUSE (holding budget
# back from a tick nobody is watching is exactly right, and exactly wrong for a watched one), and the
# review-DISABLED kill switch (it exists to stop the CRON; the human typing --force is that switch's own
# owner deliberately overriding their own stop, which is not what the switch protects against).
#
# A CORRECTNESS stop is the run being unable to do its job properly no matter who asked. No argument
# reaches these:
#   * the flock — two runs of a role collide on the same clones and the same GitHub state. That is
#     not a policy choice about spending, it is two processes corrupting each other's work.
#   * a usage-gate config REFUSAL (any non-zero exit that is not 10) — the gate could not validate
#     its config, so the tick would run on config nobody checked. Forcing past a ceiling on purpose
#     and running on unvalidated config are not the same act.
#
# Every override is RECORDED: each one appends its kind and the stop's own line to the run's
# metrics/runs.jsonl row (see the FORCE stamp below), so the dashboard reads what was overridden
# rather than merely that something was.
FORCE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --force)
      FORCE=1
      shift
      ;;
    *)
      # Rejected rather than ignored: a typo'd flag that fell through would run UNFORCED and skip on
      # the very pause the human invoked it to run past, reporting nothing about why.
      echo "review-run: unknown argument '$1' — the only argument is --force" >&2
      exit 2
      ;;
  esac
done

# What this run actually OVERRODE, appended to as each policy stop is walked past. Two parallel
# arrays because a stop's kind and that stop's own line are one fact in two parts, exactly as
# `skipped`/`skipReason` are — and there can be more than one, since a single forced run can walk
# past both the kill switch and a gate pause. Empty is a real state and NOT the same as absent: a
# forced run that met no stop at all still has to say a human started it (see the FORCE stamp).
FORCED_KINDS=()
FORCED_REASONS=()

# --- where this run's trail goes ---------------------------------------------------------------
# A scheduled tick has no terminal, so its trail is appended to $LOG and nowhere else. A FORCED run
# is being WATCHED — live observability is the whole reason it exists — so the same bytes also
# reach stdout as they are produced. Every writer below goes through `_log`, so the two sinks cannot
# drift and `--force` is the only thing that decides which is in play.
if [ "$FORCE" -eq 1 ]; then
  _log() { tee -a "$LOG"; }
else
  _log() { cat >> "$LOG"; }
fi

# --- kill switch (independent of the producer cron) ---
# A POLICY stop, so --force overrides it (#245 as ruled: "force needs to force"). This file stops
# the CRON. The human typing --force is the switch's own owner, at a terminal, deliberately
# overriding their own stop for one run — which is not what the switch protects against, and a
# force that refused them would be friction and nothing else. A SCHEDULED tick still honours it
# exactly as it always has, which is the whole reason the file exists.
if [ -f "$DIR/review-DISABLED" ]; then
  if [ "$FORCE" -eq 1 ]; then
    echo "$(date -u +%FT%TZ) FORCED past the review-DISABLED kill switch (--force)" | _log
    FORCED_KINDS+=(disabled)
    FORCED_REASONS+=("review-DISABLED flag present")
  else
    echo "$(date -u +%FT%TZ) SKIP: review-DISABLED flag present" | _log
    exit 0
  fi
fi

# The stale-setting guard, in the posture `usage-gate` already takes toward the retired
# USAGE_SLACK_PCT (#158): a knob that cannot be honoured is REFUSED rather than ignored, so a
# cron.env carrying it surfaces instead of quietly doing nothing. CRON_FORCE has never been a knob —
# forcing is per-invocation — and this is what makes that structurally true instead of merely
# intended. Refused for a SCHEDULED tick too, which is the case that matters: a stale cron.env must
# stop the pipeline visibly rather than run it forced six times a day.
# It sits BELOW the kill switch, in the position the gate's own refusal has always held: a config
# refusal is subordinate to a deliberate human stop. Above it, a stale cron.env on a pipeline
# somebody turned off would exit 2 six times a day — recurring error noise about a setting that
# cannot affect anything while review-DISABLED is present.
if [ -n "${CRON_FORCE:-}" ]; then
  echo "$(date -u +%FT%TZ) review run REFUSED: CRON_FORCE is set (cron.env or the environment), but forcing is an ARGUMENT, not a setting — as a variable it would force every scheduled tick. Unset it and pass: nix run git+file://<install-dir>#review-run -- --force" | _log
  exit 2
fi

# --- weekly-budget pace gate: skip this tick when usage is over the ceiling or inside the BAU
# headroom band under the linear burn toward the reset — the crons hold ~USAGE_HEADROOM_PCT points
# BEHIND pace so interactive work keeps standing budget (#158). `usage-gate` reads
# /api/oauth/usage itself; exit 10 means PAUSE (record one skip row, exit 0). It FAILS CLOSED when
# it cannot read usage and no fallback is set (#273) — that is a PAUSE too, its reason naming the
# read failure so the skip row is diagnosable as an endpoint problem. Any OTHER non-zero exit is a
# config REFUSAL (the retired USAGE_SLACK_PCT still set: exit 2, reason on stderr, captured into
# the log): the tick must not run on config the gate refused to read, so propagate the failure — a
# refusal is neither a run nor a pause, and it writes NO row. ---
_ug="$(pr-review-report usage-gate 2>&1)"; _ugrc=$?
echo "$(date -u +%FT%TZ) usage-gate: $_ug" | _log
if [ "$_ugrc" -eq 10 ]; then
  if [ "$FORCE" -eq 1 ]; then
    # FORCED past the PAUSE (#245), and past NOTHING else. The gate still ran and its line is in the
    # log above, verbatim; the only difference is that this tick does not become a skip row. The
    # force lives INSIDE the exit-10 branch on purpose — a refusal (below) can never reach it.
    echo "$(date -u +%FT%TZ) FORCED past the usage-gate PAUSE (--force): $_ug" | _log
    FORCED_KINDS+=(usage-gate)
    FORCED_REASONS+=("$_ug")
  else
    # A paused tick still writes its metrics/runs.jsonl row (#160) — same shape and same reasoning
    # as campaign-run.sh: an empty trace so the record's shape still comes from `run-metrics`, the
    # GATE's exit 10 on the row, the gate's own line verbatim, and exit 0 because a pause is not a
    # failure. The hourly refresh-human-queue cron is what carries the row to origin/main during a
    # pause. A REFUSAL (exit 2, below) writes no row and aborts loudly.
    TS="$(date -u +%Y%m%dT%H%M%SZ)"
    RUNLOG="$RUNDIR/$TS.jsonl"
    mkdir -p "$RUNDIR" "$DIR/metrics"
    : > "$RUNLOG"
    pr-review-report run-metrics "$RUNLOG" \
      --run-id "$TS" --role vetter --model "$REVIEW_MODEL" --exit-code 10 \
      --skipped usage-gate --skip-reason "$_ug" \
      >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
    exit 0
  fi
elif [ "$_ugrc" -ne 0 ]; then
  echo "$(date -u +%FT%TZ) review run ABORTED: usage-gate refused its config (exit $_ugrc) — fix cron.env" | _log
  exit "$_ugrc"
fi

# --- the FORCE stamp every row this run writes carries (#245) ----------------------------------
# A forced run is not a paced tick, and a runs.jsonl row that cannot say so puts budget on the
# dashboard's run series against a schedule that was never followed. Built ONCE, here, after the
# last stop a force can walk past, so it carries what this run ACTUALLY overrode rather than what a
# force is allowed to override.
#
# `--forced-run` is the fact that a human started this, and it is passed whenever `--force` was —
# INCLUDING when nothing was in the way, which is the ordinary case once the crons are running
# again. Each `--forced/--force-reason` pair is one stop that was actually walked past. So an empty
# stamp still says "not scheduled", and a consumer reading the kinds learns exactly which stops
# yielded. Absent entirely for a scheduled tick, which is what keeps every other row byte-identical.
FORCED_FLAGS=()
if [ "$FORCE" -eq 1 ]; then
  FORCED_FLAGS=(--forced-run)
  for _i in "${!FORCED_KINDS[@]}"; do
    FORCED_FLAGS+=(--forced "${FORCED_KINDS[$_i]}" --force-reason "${FORCED_REASONS[$_i]}")
  done
fi

# --- single-run lock (non-blocking) ---
exec 9>"$LOCK"
if ! flock -n 9; then
  echo "$(date -u +%FT%TZ) SKIP: previous review run still holding the lock" | _log; exit 0
fi

mkdir -p "$RUNDIR"
# The metrics dir must exist BEFORE the run, not just after it: `run-timings` appends this run's
# boot/ttl records from inside the live pipe, long before the end-of-run `run-metrics` line.
mkdir -p "$DIR/metrics"
cd "$DIR" || exit 1

# rotate per-run traces, with each trace's siblings. The glob stays `*.jsonl` and the LENS ledger
# below is deliberately NOT named `.jsonl`: matching it here would count it as a second run and halve
# how many runs `REVIEW_KEEP_RUNS` actually keeps.
find "$RUNDIR" -maxdepth 1 -name "*.jsonl" -printf "%T@ %p\n" 2>/dev/null | sort -rn | cut -d" " -f2- | tail -n +$((REVIEW_KEEP_RUNS + 1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err" "${old%.jsonl}.lens"; done
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"
# The run's LENS LEDGER (#151): every `audit` skill invocation the harness announces, written from
# inside the live pipe by `run-timings` and read back by `record_verdict`, which REFUSES a verdict on a
# PR the ledger holds no invocation for. Named in TWO places for the same reason `$RUNLOG` is — the
# writer takes it as a flag (testable) and the reader takes it from the environment, because the MCP
# server's argv is fixed by review-mcp.json and cannot carry a per-run path.
LENSLOG="$RUNDIR/$TS.lens"
export RUN_LENS_LEDGER="$LENSLOG"

# --- harness read-time dependencies: resolved BEFORE a token is spent -------------------------
# The audit lens reads whatever the PR ships, and this org's audit evidence is PDF, which the
# harness renders by shelling out to poppler. A missing renderer does not crash the run: `Read`
# returns a typed error, the model vets what it can still see, records a verdict, and claude exits
# 0. That is how #85's run vetted `ready` a PR the previous run had sent back — the dependency
# moved the verdict. So the check runs here and a miss ENDS the run: no verdict at all beats a
# verdict from a lens that was blind without saying so.
_pf="$(pr-review-report preflight)"; _pfrc=$?
printf '%s\n' "$_pf" | sed 's/^/  /' | _log
if [ "$_pfrc" -ne 0 ]; then
  _missing="$(printf '%s\n' "$_pf" | sed -n 's/^missing=//p')"
  echo "$(date -u +%FT%TZ) review run ABORT: harness tools missing from PATH: $_missing" | _log
  # An empty trace, so the record's shape still comes from `run-metrics` — there is no second
  # place that knows what a runs.jsonl line looks like.
  : > "$RUNLOG"
  mkdir -p "$DIR/metrics"
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role vetter --model "$REVIEW_MODEL" --exit-code "$_pfrc" \
    --preflight-missing "$_missing" \
    "${FORCED_FLAGS[@]}" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
  exit "$_pfrc"
fi

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

# --- the RUN BUDGET the prompt states (#288) ---------------------------------------------------
# ONE definition — `RUN_ITEM_CAP` in pr-review-report — and it is the same constant the state-loads'
# `limit` range is computed from, so the budget the vetter is TOLD to spend and the page its own
# tool surface will hand it cannot disagree. They disagree silently when they can: `unvetted` and
# `unvetted_close_candidates` REFUSE an out-of-range `limit` rather than clamping it, so a vetter
# told to spend more items than the page can carry does not error, it just quietly does less.
#
# A value that is not a positive integer ABORTS: `{{ITEM_CAP}}` rendering empty leaves the vetter a
# RUN BUDGET sentence with no number in it, which nothing rejects and every run resolves its own
# way — the same silent-degradation class as the empty auditor brief below.
#
# "Positive" is decided by finding a NONZERO DIGIT, not by excluding the string `0`: `00` is all
# digits and is not `0`, so an exclusion list lets it through and renders "at most 00 WORK ITEMS",
# which is the zero budget this guard exists to refuse wearing two characters instead of one.
ITEM_CAP="$(pr-review-report item-cap 2>/dev/null)"
case "$ITEM_CAP" in
  '' | *[!0-9]*)
    echo "$(date -u +%FT%TZ) review run ABORT: \`pr-review-report item-cap\` gave no usable run budget (got '$ITEM_CAP') — the prompt's {{ITEM_CAP}} would render empty" | _log
    exit 1
    ;;
  *[1-9]*) ;;
  *)
    echo "$(date -u +%FT%TZ) review run ABORT: \`pr-review-report item-cap\` gave a ZERO run budget (got '$ITEM_CAP') — a run told to spend no items must not start" | _log
    exit 1
    ;;
esac

# substitute deployment values into the prompt template
PROMPT="$(sed -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              -e "s#{{OWNER_FLAGS}}#$OWNER_FLAGS#g" \
              -e "s#{{ORGS}}#$ORGS_HUMAN#g" \
              -e "s#{{WORK_DIR}}#$WORK_DIR#g" \
              -e "s#{{ITEM_CAP}}#$ITEM_CAP#g" \
              "$PROMPT_FILE")"

# --- the NON-INTERACTIVE pragma (claude-config hooks/noninteractive.py) ------------------------
# Same reason as the producer's: this is `claude --print`, nothing re-wakes it when a backgrounded
# command finishes (#249), and the two `background-*` PreToolUse hooks would rewrite every long
# build and every wait into exactly that. Stamped into the CONTEXT, because the hook is handed a
# `transcript_path` and reads it, and into BOTH the main prompt and the auditor brief so it is
# present whichever trace a dispatched agent's `transcript_path` names.
#
# The vetter builds nothing itself (it is read-only on the filesystem), so the build half of this
# is the producer's problem -- but a WAIT is not a build, and the poll-loop hook rewrites `sleep`,
# `until ... do` and `tail -f` for any caller.
NONINTERACTIVE_PRAGMA="CLAUDE-PRAGMA-NONINTERACTIVE-6b1f9d4e"
PROMPT="$PROMPT

$NONINTERACTIVE_PRAGMA"

# --- the STANDING BRIEF every dispatched AUDITOR starts with (#257) ----------------------------
# The vetter's audit lens is deep source reading, and until now every byte of it landed in the ONE
# main-loop context that is re-read on every turn. Measured on 20260810T091521Z: 72 tool calls,
# 223,804 cached tokens read PER CALL, a context peaking at 417,832, $27.97 — against the producer's
# $5.95 for the same 3-item budget over 116 calls, because the producer dispatches. 63% of that
# run's tool-result bytes were Read/Grep/Glob of one PR still being re-read while the next was
# judged. A sub-agent carries its own context and that context dies with it, so the reads go there.
#
# Same channel and same reasoning as campaign-run.sh's `--agents` block: a dispatched sub-agent
# starts with no prompt, so the standing rules reach it only if something puts them there, and the
# harness loads a `--agents` type's prompt straight into the agent — which is why the main loop pays
# none of those bytes and cannot paraphrase them away. Built with jq rather than committed as JSON
# so the brief stays PLAIN TEXT a human edits and a conformance test greps.
#
# `tools` is what keeps this inside the vetter's role. The auditor gets the READ half of the
# surface and nothing else: no `record_verdict`, no `record_close_candidate_verdict`, no
# `clone_release`. Verified against claude 2.1.226 that the key is honoured — a probe defined with
# `"tools":["Read","Glob"]` reported exactly those two — and that the session deny-list reaches
# inside a sub-agent as well ("Bash is disabled for this session, in subagents as well as here"),
# so review-settings.json's Bash/Write/Edit/NotebookEdit denials cover the auditor unchanged.
# `mcp__fsm__pr_checkout` is granted because the audit lens follows callees into DEPENDENCY repos
# and an auditor that cannot reach that source is a NARROWER lens wearing a cheaper price;
# `clone_release` is deliberately withheld, so an auditor can never dispose of the tree the
# main loop's verdict is about (the nightly `vet-*` age sweep is what reclaims the rest).
#
# The guard is CONTENT, not existence, for campaign-run.sh's reason: an empty or whitespace-only
# brief builds perfectly valid JSON carrying an empty prompt, which registers the type and briefs
# nobody — the silent degradation, one truncated file away. `-f` would pass it and so would `-s`.
if ! grep -q '[^[:space:]]' "$DIR/review-auditor-prompt.txt" 2>/dev/null; then
  echo "$(date -u +%FT%TZ) review run ABORT: no review-auditor-prompt.txt in '$DIR'" | _log
  exit 1
fi
AUDITOR_JSON="$(jq -nc --rawfile brief "$DIR/review-auditor-prompt.txt" \
  --arg pragma "$NONINTERACTIVE_PRAGMA" \
  '{"pr-auditor":{"description":"Vetter auditor: runs the audit lens over ONE PR and reports findings, recording nothing.","prompt":($brief + "\n\n" + $pragma),"tools":["Read","Glob","Grep","Skill","ToolSearch","mcp__fsm__pr_checkout"]}}')"
if [ -z "$AUDITOR_JSON" ]; then
  echo "$(date -u +%FT%TZ) review run ABORT: could not build the auditor brief from review-auditor-prompt.txt" | _log
  exit 1
fi

{
  echo "================================================================="
  echo "$(date -u +%FT%TZ) review run START (model=$REVIEW_MODEL, host=$(uname -n)) trace=$RUNLOG"
} | _log

# `gh` is on PATH as a bare executable, put there by nix from the flake's runtimeInputs, for the MCP
# SERVER — it shells out to gh for every GitHub read and for its one write. The vetter model itself
# has no Bash and never invokes it. `jq` is on PATH for THIS SCRIPT and only for it — the auditor
# `--agents` block above is its one use, exactly as it is on the producer side. The vetter MODEL
# still cannot reach it: with `Bash` denied there is nothing for it to invoke jq from, which is why
# the binary can ride in the closure without stating a capability the role does not have.
# Model fallback: try $REVIEW_MODEL, then each $FALLBACK_MODELS in order, advancing ONLY on a
# quota/usage limit so one model's exhausted quota can't stall vetting. Any other outcome (success,
# auth/startup failure, real error) is final.
# NO `--allowedTools` HERE, and that omission is the answer, not an oversight (#118). The producer
# passes an `Edit(//…)` rule beside its `--add-dir` flags because a bash output redirection is a
# `create` that working-directory membership alone does not authorise — see campaign-run.sh for the
# mechanism. The vetter has no such gap to close: review-settings.json DENIES `Bash`, `Write`,
# `Edit` and `NotebookEdit`, deny beats allow, and a session with no Bash tool cannot express a
# redirection at all. So an edit-kind allow rule here would be inert, and would falsely advertise a
# vetter that writes. The vetter is a READER — `pr_checkout` (the MCP tool, running server-side)
# makes the checkout and `record_verdict` is its one write, both outside the model's tool surface —
# and these two `--add-dir` flags confer exactly the read membership Read/Glob/Grep need over the
# install dir and the checkouts. If Bash is ever granted here, that is the moment to decide about a
# redirect grant; the `vetter has no write grant` CI job fails until someone does.
USED_MODEL="$REVIEW_MODEL"
rc=1
for USED_MODEL in $REVIEW_MODEL $FALLBACK_MODELS; do
  echo "$(date -u +%FT%TZ)   model attempt: $USED_MODEL" | _log
  timeout "$REVIEW_MAXTIME" claude --print "$PROMPT" \
    --model "$USED_MODEL" \
    --settings "$SETTINGS_FILE" \
    --agents "$AUDITOR_JSON" \
    "${MCP_ARGS[@]}" \
    --permission-mode default \
    --verbose --output-format stream-json \
    --add-dir "$DIR" \
    --add-dir "$WORK_DIR" \
    2>"$ERRLOG" \
    | tee "$RUNLOG" \
    | { pr-review-report run-timings --out "$DIR/metrics/runs.jsonl" --trace "$RUNLOG" \
          --lens "$LENSLOG" \
          --run-id "$TS" --role vetter --model "$USED_MODEL" 2>/dev/null || cat ; } \
    | { pr-review-report distill-trace 2>/dev/null || cat >/dev/null ; } | _log
  rc=${PIPESTATUS[0]}
  # Typed verdict from the trace's result events, not a grep over the trace bytes (see
  # `classify_trace`): the old regex also matched a 429 quoted inside an unrelated tool result.
  if [ "$(pr-review-report trace-outcome "$RUNLOG" --exit-code "$rc")" = "session-limit" ]; then
    echo "  !! model $USED_MODEL is quota-limited — falling back to next model" | _log
    continue
  fi
  break
done

if [ ! -s "$RUNLOG" ] && [ -s "$ERRLOG" ]; then
  echo "  !! no event stream — likely auth/startup failure; stderr:" | _log
  tail -5 "$ERRLOG" | sed 's/^/    /' | _log
fi

echo "$(date -u +%FT%TZ) review run END (exit=$rc, trace=$RUNLOG)" | _log

# `run-metrics` emits the whole enriched record, deriving `outcome` with the same typed classifier
# the fallback loop uses, so the two can never disagree about whether a run was quota-limited.
# This is the run's `stage: final` line. Its `stage: boot` / `stage: ttl` lines were already
# appended mid-run by `run-timings` above — reaching HERE at all is what a killed run cannot do.
if [ -s "$RUNLOG" ]; then
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role vetter --model "$USED_MODEL" --exit-code "$rc" \
    "${FORCED_FLAGS[@]}" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
fi
exit 0
