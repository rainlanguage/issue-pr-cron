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

# --- locate the INSTALL dir (cron.env, prompts, logs, ledgers) ---
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

# --- weekly-budget pace gate: skip this tick when usage is over the ceiling or inside the BAU
# headroom band under the linear burn toward the reset — the crons hold ~USAGE_HEADROOM_PCT points
# BEHIND pace so interactive work keeps standing budget (#158). `usage-gate` reads
# /api/oauth/usage itself; exit 10 means PAUSE (record one skip row, exit 0). It is INERT when it
# cannot read usage and no fallback is set — it prints OK and we run. Any OTHER non-zero exit is a
# config REFUSAL (the retired USAGE_SLACK_PCT still set: exit 2, reason on stderr, captured into
# the log): the tick must not run on config the gate refused to read, so propagate the failure — a
# refusal is neither a run nor a pause, and it writes NO row. ---
_ug="$(pr-review-report usage-gate 2>&1)"; _ugrc=$?
echo "$(date -u +%FT%TZ) usage-gate: $_ug" >> "$LOG"
if [ "$_ugrc" -eq 10 ]; then
  # A paused tick still writes its metrics/runs.jsonl row (#160): the dashboard reads runs from
  # that file, and a pause that wrote nothing rendered as a dead stretch indistinguishable from a
  # broken cron. Same shape as the preflight abort below — an empty trace, so the record's shape
  # still comes from `run-metrics` and no second place knows what a runs.jsonl line looks like.
  # The row records the GATE's exit 10 (as the preflight row records preflight's 12) plus the
  # gate's own line, verbatim; this script still exits 0 because a pause is not a failure. The row
  # reaches origin/main via the hourly refresh-human-queue cron, which stages this file — the one
  # committer still awake during a pause.
  TS="$(date -u +%Y%m%dT%H%M%SZ)"
  RUNLOG="$RUNDIR/$TS.jsonl"
  mkdir -p "$RUNDIR" "$DIR/metrics"
  : > "$RUNLOG"
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role producer --model "$MODEL" --exit-code 10 \
    --skipped usage-gate --skip-reason "$_ug" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
  exit 0
fi
if [ "$_ugrc" -ne 0 ]; then
  echo "$(date -u +%FT%TZ) campaign run ABORTED: usage-gate refused its config (exit $_ugrc) — fix cron.env" >> "$LOG"
  exit "$_ugrc"
fi

# --- single-run lock (non-blocking: skip this tick if a prior run is still going) ---
exec 9>"$LOCK"
if ! flock -n 9; then
  echo "$(date -u +%FT%TZ) SKIP: previous run still holding the lock" >> "$LOG"
  exit 0
fi

# clones live here; per-run traces here. The metrics dir must exist BEFORE the run, not just after
# it: `run-timings` appends this run's boot/ttl records from inside the live pipe, long before the
# end-of-run `run-metrics` line.
mkdir -p "$WORK_DIR" "$RUNDIR" "$DIR/metrics"
cd "$WORK_DIR" || exit 1

# The FSM MCP server reads both clone roots from the environment, never from a tool argument — a
# model-supplied root would make its path guard vacuous. WORK_DIR is where clones belong; INSTALL_DIR
# is swept too because it collected `vet-*` clones for months (review-run.sh did not substitute
# {{WORK_DIR}} into the vetter prompt, so the vetter checked out into its cwd).
export WORK_DIR
export INSTALL_DIR="$DIR"

# rotate per-run traces (keep newest $KEEP_RUNS .jsonl + their .err sidecars)
find "$RUNDIR" -maxdepth 1 -name "*.jsonl" -printf "%T@ %p\n" 2>/dev/null | sort -rn | cut -d" " -f2- | tail -n +$((KEEP_RUNS + 1)) | while read -r old; do rm -f "$old" "${old%.jsonl}.err"; done
# per-run infra records (#108): kept a week so the last few runs' raw records can be read by hand,
# then dropped — their contents are already folded into metrics/runs.jsonl, which is the durable copy.
find "$DIR/metrics" -maxdepth 1 -name ".infra-*.jsonl" -mtime +7 -delete 2>/dev/null
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUNLOG="$RUNDIR/$TS.jsonl"
ERRLOG="$RUNDIR/$TS.err"

# --- the run's infra record (#108) -------------------------------------------------------------
# Where `infra-down` writes and where `run-metrics` / `run-infra` read it back. Named for THIS run's
# id, so one run's finding can never be read as another's — the staleness bug a fixed path would
# have had, and the reason the model is never asked to pass a path it could get wrong.
# It replaces the `ai:blocked-infra` label: ending the run leaves NO state on GitHub, only this line.
INFRAREC="$DIR/metrics/.infra-$TS.jsonl"
export RUN_INFRA_FILE="$INFRAREC"
rm -f "$INFRAREC"

# --- harness read-time dependencies: resolved BEFORE a token is spent -------------------------
# Same check, same reason, as review-run.sh. The producer drains the `audit`-labelled backlog
# first, and those issues cite `audit/protofire/*.pdf` as their source (raindex#2619 names the file
# outright) — a producer that cannot open the report it is implementing against writes a PR body
# asserting something it never read. A missing dependency ends the run rather than degrading it.
#
# The two CAPABILITY flags are the run's environment preconditions, asserted here rather than by the
# model. `gh auth status` and a `nix develop …#sol-shell -c forge --version` opened every producer
# run byte-identically and carried no decision content — the model read two answers it could do
# nothing about and then started work anyway. Asserted here they ABORT: no tokens, one run-metrics
# row naming what was unsatisfied, `ToolingFailure`, which is neither a success nor a skip. The gate
# is also stricter than the read it replaces, because it checks the token's SCOPES rather than
# eyeballing the word "Logged".
_pf="$(pr-review-report preflight --gh-auth --sol-shell)"; _pfrc=$?
printf '%s\n' "$_pf" | sed 's/^/  /' >> "$LOG"
if [ "$_pfrc" -ne 0 ]; then
  _missing="$(printf '%s\n' "$_pf" | sed -n 's/^missing=//p')"
  echo "$(date -u +%FT%TZ) campaign run ABORT: harness dependencies unsatisfied: $_missing" >> "$LOG"
  : > "$RUNLOG"
  mkdir -p "$DIR/metrics"
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role producer --model "$MODEL" --exit-code "$_pfrc" \
    --preflight-missing "$_missing" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
  exit "$_pfrc"
fi

# --- per-run scratch: the ONE place this run may write throwaway files ------------------------
# WHY IT EXISTS: with no designated path, every run invented its own at the root of a clone root
# and nothing ever reclaimed them — `covered.json`, `covered-set.json`, `covered_set.json`,
# `covered-keys.json`, `covered-map.json`, `covered-issues.json`, `covered_issues.json`: seven
# names for one notion, six weeks deep, and `.gitignore`'s `/*` kept the pile invisible (#106).
#
# WHY IT NEEDS AN ALLOW RULE, NOT JUST `--add-dir`: verified against claude 2.1.220. A bash output
# REDIRECTION is a `create` operation, and working-directory membership — which is all that
# `--add-dir` and `permissions.additionalDirectories` confer — short-circuits to allow for `read`
# or in `acceptEdits` mode ONLY. Under `--permission-mode default` a `create` falls through to an
# EDIT-KIND ALLOW RULE, so `--add-dir $SCRATCH_DIR` on its own still leaves `... > $SCRATCH_DIR/x`
# refused — and refused by a message that lists the directory as allowed, which is precisely what
# sent the producer hunting for a legal path that did not exist. The `--allowedTools` grant below
# is what makes the redirect work; it covers BOTH `--add-dir` roots, for the reason given there,
# and this dir is inside one of them. Its `//` prefix is load-bearing: `Edit(/abs/**)` is silently
# inert; only `Edit(//abs/**)` matches an absolute path.
#
# PER-RUN, NOT PERSISTENT: nothing accumulates, and no run can read another run's half-written
# state and mistake it for its own. Nothing is lost by deleting it — every tool call and its full
# output is already in this run's trace ($RUNLOG), which is the post-mortem source and is retained
# $KEEP_RUNS deep. A persistent dir would have to be taught to the gc, and the gc only recognises
# directories containing `.git` (it sweeps work CLONES), so it would never have reclaimed this.
#
# It lives UNDER $WORK_DIR so the model can Read/Glob it back with no further configuration — the
# whole point is writing something down and re-reading it instead of paying for the call twice —
# and so scratch shares the disk already sized for clones. gc ignores it (no `.git` inside).
# $TS alone is NOT unique: the flock is per-INSTALL-dir while WORK_DIR defaults to $HOME/code, so
# two installs sharing a WORK_DIR run concurrently by design and can start within the same second.
# $$ makes the path unique per PROCESS, which is what actually distinguishes them.
SCRATCH_DIR="$WORK_DIR/scratch/$TS-$$"
# A run killed outright (SIGKILL, box reboot) never reaches its own cleanup, so reclaim on the way
# IN as well as on the way out. AGE-BOUNDED rather than "everything that is not mine", for the same
# concurrency reason: an unbounded sweep would delete a live sibling run's scratch out from under
# it. `-mmin +1440` and not `-mtime +1` — the latter truncates to whole days, so it would leave a
# 25-hour-old directory in place until it was 48 hours old. A day is far past MAXTIME either way.
find "$WORK_DIR/scratch" -mindepth 1 -maxdepth 1 -type d -mmin +1440 -exec rm -rf {} + 2>/dev/null
# errexit is off in this runner, so an unchecked mkdir would fall through on a full disk or a
# permission fault and hand the model an authorised path that does not exist — every redirect into
# it failing, which is exactly the state before #106, minus the clue. Same reading as the `cd
# "$WORK_DIR" || exit 1` above: if the run cannot have its work area, it does not start.
if ! mkdir -p "$SCRATCH_DIR"; then
  echo "$(date -u +%FT%TZ) campaign run ABORT: cannot create scratch dir '$SCRATCH_DIR'" >> "$LOG"
  exit 1
fi
export SCRATCH_DIR

# Reclaim this run's scratch HOWEVER the run ends. This is what keeps the footprint one run's
# worth rather than a growing pile, and it is a TRAP because a statement at the foot of the script
# runs only when the script REACHES that line: a killed run left
# `scratch/20260728T132223Z-3993293/` behind (#118) while the comment beside the statement called
# the delete unconditional.
# EXIT alone is the whole grant needed. Measured on bash 5.1 and on the flake's bash, with the
# signal delivered to the script while it sits in the model pipeline: INT, TERM and HUP each run
# the EXIT trap and reclaim, so explicit INT/TERM/HUP traps add nothing but a changed exit status.
# SIGKILL does not, and cannot be made to by anyone; nor does a box reboot. Those two are what the
# age-bounded sweep above reclaims, on the next run's way in.
# Deleting on EXIT rather than in sequence moves the delete PAST the `run-metrics` line and past
# the `run-infra` read-back that can `exit 12`. Neither reads the scratch dir — `run-metrics` reads
# $RUNLOG and $INFRAREC, `run-infra` reads $INFRAREC — so the reordering changes when the delete
# happens, never what survives it.
# Installed HERE, after the mkdir succeeded: the pre-model exits above (DISABLED, usage-gate, lock,
# preflight, and the mkdir failure itself) have no scratch dir to reclaim and must not acquire one.
trap '[ -n "${SCRATCH_DIR:-}" ] && rm -rf "$SCRATCH_DIR"' EXIT

# substitute deployment values into the (path-free) prompt template at runtime
PROMPT="$(sed -e "s#{{WORK_DIR}}#$WORK_DIR#g" \
              -e "s#{{ASSIGNEE}}#$PR_ASSIGNEE#g" \
              -e "s#{{OWNER_FLAGS}}#$OWNER_FLAGS#g" \
              -e "s#{{ORGS}}#$ORGS_HUMAN#g" \
              -e "s#{{INSTALL_DIR}}#$DIR#g" \
              -e "s#{{SCRATCH_DIR}}#$SCRATCH_DIR#g" \
              "$DIR/campaign-prompt.txt")"

# --- the STANDING BRIEF every dispatched worker starts with (#200) -----------------------------
# A dispatched sub-agent starts with no prompt, so the run's standing rules reach it only if
# something puts them there. Until now the only channel was the dispatch prompt the main loop
# improvised per item, and 36% of those bytes (60,891 of 181,867 across the 40 dispatches in the
# retained traces) were the same standing rules retyped — held in the MAIN LOOP's context and
# re-read on every one of its remaining turns, which is where they are dearest ($2.50 measured).
# The rules the main loop did NOT retype are the ones it never thought to: 142 of the 362 GitHub
# reads dispatched agents made were a per-turn `gh pr checks` poll and 72 were a re-read of a
# subject already in the agent's own context — $15.73, 68% of the sub-agent cold-start bucket.
#
# `--agents` is the harness's own channel for this: the JSON defines a subagent TYPE whose prompt
# the harness loads directly into each dispatched agent, so the main loop pays none of those bytes
# and cannot paraphrase the rules away. Verified against claude 2.1.221 that a `--agents`-defined
# type is dispatchable and inherits the full surface these settings grant — Bash, ToolSearch and
# the `mcp__fsm__*` producer profile all resolve inside one.
#
# Built with jq rather than committed as JSON so the brief stays PLAIN TEXT: it is prompt prose a
# human edits and a conformance test greps, and a hand-escaped JSON string literal is neither.
# A brief that cannot be built ABORTS the run: dispatch would still work, which is the problem —
# the workers would silently go back to improvised briefing and nothing in the trace would say so.
# `-s` and not `-f`: an EMPTY brief passes `-f` and then builds perfectly valid JSON carrying an
# empty prompt, which registers the type and briefs nobody — the silent degradation this guard
# exists to prevent, one file-truncation away.
if [ ! -s "$DIR/campaign-worker-prompt.txt" ]; then
  echo "$(date -u +%FT%TZ) campaign run ABORT: no campaign-worker-prompt.txt in '$DIR'" >> "$LOG"
  exit 1
fi
AGENTS_JSON="$(jq -nc --rawfile brief "$DIR/campaign-worker-prompt.txt" \
  '{"pr-worker":{"description":"Producer worker: does ONE dispatched item end to end and reports its outcome.","prompt":$brief}}')"
if [ -z "$AGENTS_JSON" ]; then
  echo "$(date -u +%FT%TZ) campaign run ABORT: could not build the worker brief from campaign-worker-prompt.txt" >> "$LOG"
  exit 1
fi

{
  echo "================================================================="
  echo "$(date -u +%FT%TZ) campaign run START (model=$MODEL, host=$(uname -n)) trace=$RUNLOG"
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
# `--allowedTools` grants the redirect form to EVERY directory `--add-dir` names, and to nothing
# else. One rule per root, because the two roots are siblings under $HOME and no single glob spans
# them without also spanning the rest of $HOME. The scratch dir needs no rule of its own: it is
# `$WORK_DIR/scratch/…`, so the WORK_DIR rule subsumes it.
# WHY BOTH ROOTS, rather than scratch alone: `--add-dir` and an edit-kind allow rule are the two
# halves of one grant (see the block above), and a directory given only the first half refuses
# redirects with a message NAMING IT AS ALLOWED. Scratch-only left every one of the hundreds of
# work clones under $WORK_DIR in that state, which is what made a render harness — components
# written into the clone so the real one can be mounted against controlled data, the org's
# known-good way to produce the before/after screenshots the vetter requires — unbuildable by the
# ordinary means (#118).
# WHY $DIR IS GRANTED TOO, and not deliberately refused: refusing it would be a boundary only if
# the install dir were otherwise unwritable, and it is not. campaign-settings.json allows `Write`
# and `Edit` with no path constraint, and the Bash allow-list carries `cp`, `mv`, `tee`, `touch`,
# `sed` and `mkdir` — every one of which lands a byte anywhere this process can write. That is how
# 78 stray files reached the install dir with the redirect form already refused. So the choice is
# not "writable vs not"; it is "one write form refused, with a self-contradicting message, while a
# dozen others are not" versus consistency. What keeps the install dir clean is the prompt's rule
# plus the scratch dir that gives it somewhere legal to go — a guard at the level that actually
# governs. A permission rule cannot express this one, and pretending it does costs a turn per
# refusal and buys nothing.
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
    --agents "$AGENTS_JSON" \
    --permission-mode default \
    --verbose --output-format stream-json \
    --add-dir "$WORK_DIR" \
    --add-dir "$DIR" \
    --allowedTools "Edit(//$WORK_DIR/**),Edit(//$DIR/**)" \
    2>"$ERRLOG" \
    | tee "$RUNLOG" \
    | { pr-review-report run-timings --out "$DIR/metrics/runs.jsonl" --trace "$RUNLOG" \
          --run-id "$TS" --role producer --model "$USED_MODEL" 2>/dev/null || cat ; } \
    | { pr-review-report distill-trace 2>/dev/null || cat >/dev/null ; } >> "$LOG"
  rc=${PIPESTATUS[0]}
  # Advance to the next model ONLY on a usage/quota limit; any other outcome is final. The verdict
  # is a TYPE computed from the trace's result events (see `classify_trace`), not a grep over the
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
# Appends one enriched JSON line to metrics/runs.jsonl (committed+pushed to main by the hourly
# refresh-human-queue cron, never from here — this cron does not push). Best-effort: never fail
# the run on it.
# `run-metrics` emits the whole enriched record itself now, including `outcome` — which it derives
# with the same typed classifier the fallback loop uses, so the metrics line and the fallback
# decision can never disagree about whether a run was quota-limited.
# This is the run's `stage: final` line. Its `stage: boot` / `stage: ttl` lines were already
# appended mid-run by `run-timings` above — reaching HERE at all is what a killed run cannot do.
if [ -s "$RUNLOG" ]; then
  pr-review-report run-metrics "$RUNLOG" \
    --run-id "$TS" --role producer --model "$USED_MODEL" --exit-code "$rc" \
    --infra "$INFRAREC" \
    >> "$DIR/metrics/runs.jsonl" 2>/dev/null || true
fi

# The scratch dir is reclaimed by the EXIT trap installed where the dir is created — including on
# the `exit 12` below. A failed run's scratch is no more informative than a successful one's: the
# trace records the content of both identically.

# --- did the run end because the infrastructure was down? (#108) ------------------------------
# Every other exit in this script is PRE-MODEL: disabled, usage-gated, locked, preflight. So nothing
# the model LEARNED could ever end its run — it could only label the victims and carry on queueing
# work behind the same dead infrastructure, which is what run 20260728T111645Z did. `run-infra` reads
# the finding the model recorded mid-flight and exits 12. That code is the preflight abort's, and it
# means the same thing: the run could not usefully proceed, the code is fine, retry next tick. The
# fact is already on the metrics line above, so the record survives the exit, and the record file
# itself is per-run and rotates with the traces.
_ri="$(pr-review-report run-infra "$INFRAREC")"; _rirc=$?
printf '%s\n' "$_ri" | sed 's/^/  /' >> "$LOG"
if [ "$_rirc" -eq 12 ]; then
  echo "$(date -u +%FT%TZ) campaign run ENDED EARLY: infrastructure down (see infraReason in metrics/runs.jsonl)" >> "$LOG"
  exit 12
fi
exit 0
