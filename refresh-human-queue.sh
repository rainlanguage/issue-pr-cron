#!/usr/bin/env bash
# Regenerate the FSM-conformance snapshot (human-queue.json) and commit it to main so the
# rain-org-health dashboard can fetch it at runtime from the raw URL — no site redeploy for data.
# The snapshot itself is OVERWRITE (point-in-time); alongside it we APPEND one rollup line per
# changed refresh to human-queue-history.jsonl ({ts, counts}, mirroring metrics/runs.jsonl) so the
# dashboard can render per-state inventory over time (Theory-of-Constraints flow panel;
# rain-org-health#32). This tick is also what publishes metrics/runs.jsonl itself (#160): the
# model runners append rows but never push, and during a usage-gate pause they are the ONLY thing
# writing (one skip row per gated tick) — this cron is data-only, never usage-gated, and already
# commits straight to main every hour, which makes it the one committer still awake during a
# pause. Data-only, safe unattended. Installed on a cron; see crontab.
# Packaged as a flake output (`packages.refresh-human-queue`); nix builds PATH from the flake's
# locked nixpkgs. errexit is turned back off — writeShellApplication forces it, but this script
# reads exit status as data (`git diff --quiet` says whether the snapshot moved, a rejected push
# is a state to recover from) and decides what to do with each one explicitly.
set +o errexit

# --- locate the install dir + bare-cron env (mirrors campaign-run.sh) ---
# $0 is a read-only nix store path now, so the install dir comes from the crontab's $CRON_DIR,
# defaulting to the working directory for an interactive run from the checkout.
DIR="${CRON_DIR:-$PWD}"
: "${HOME:=$(getent passwd "$(id -un)" | cut -d: -f6)}"; export HOME
: "${USER:=$(id -un)}"; export USER
: "${LOGNAME:=$USER}"; export LOGNAME

# Every line this script emits is stamped, in the same format campaign.log and review.log use
# (`$(date -u +%FT%TZ) …`). The lines used to be bare text, so refresh-human-queue.log could not
# answer "when did this fail?" or "when did it last work?" — a two-day-old failure read exactly
# like a current one. Success is logged too, for the same reason: the trail is the answer.
# Everything goes to stderr, one stream, so the crontab's `>> …log 2>&1` preserves the order.
log() { echo "$(date -u +%FT%TZ) refresh-human-queue: $*" >&2; }

# git, with its output captured so a failure is reported as ONE stamped line instead of leaking
# git's own unstamped multi-line message into the log. Only for the mutating/network steps:
# `diff --quiet` and the rev-* queries use their exit status as data, not as failure.
git_q() {
  local out rc
  out="$(git -C "$DIR" "$@" 2>&1)"; rc=$?
  [ "$rc" -eq 0 ] || log "git $* failed (rc=$rc): $(printf '%s' "$out" | tr '\n' ' ')"
  return "$rc"
}

cd "$DIR" || { log "install dir '$DIR' is not usable — set CRON_DIR to the checkout"; exit 1; }

# Org scope + assignee: single source is cron.env (same as the producer/vetter).
# shellcheck disable=SC1091
[ -f cron.env ] && . ./cron.env
: "${ORGS:=rainlanguage cyclofinance S01-Issuer}"; export ORGS
export PR_ASSIGNEE

# `pr-review-report` comes from the flake, so it is the binary built from THIS commit. It used to
# be `$DIR/result/bin/pr-review-report` — a gitignored symlink from whenever someone last ran
# `nix build`, checked only for being executable, never for being current. A stale `result` ran
# happily and that is what made the counts keys flap (#76 item 6).

# flock so overlapping ticks never stack. Everything below — the sync included — runs under it,
# so two ticks can never drive the same working tree's git state at once.
exec 9>"$DIR/.refresh-human-queue.lock"
flock -n 9 || { log "skipped: a previous tick still holds the lock"; exit 0; }

# --- sync with the remote BEFORE generating -----------------------------------------------
# The snapshot is committed straight to main, so a tick that starts behind origin/main can only
# produce a non-fast-forward push, and every later tick repeats it from the same stale HEAD.
# The repair has to happen HERE, before the working tree is dirty: once the new snapshot is
# written, git refuses to fast-forward over the local modification ("your local changes would be
# overwritten"), which is exactly why the previous `pull --ff-only` — placed after the write, and
# with its error discarded — never actually recovered anything.
#
# Fast-forward only. A diverged branch means unpushed local work, which is a real error to report,
# not something to merge over, reset away or force past.
BRANCH="$(git -C "$DIR" rev-parse --abbrev-ref HEAD 2>/dev/null)"
REMOTE="$(git -C "$DIR" config --get "branch.$BRANCH.remote")"
UPSTREAM="$(git -C "$DIR" rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null)"
if [ -z "$REMOTE" ] || [ -z "$UPSTREAM" ]; then
  log "ERROR: branch '$BRANCH' has no upstream — the snapshot cannot be published from here"
  exit 1
fi

# One helper, used again by the push retry below: how far apart HEAD and the upstream are.
sync_counts() { git -C "$DIR" rev-list --left-right --count "$UPSTREAM...HEAD" 2>/dev/null; }

git_q fetch --quiet "$REMOTE" || { log "ERROR: fetch failed; not generating a snapshot this tick"; exit 1; }
counts="$(sync_counts)"
if [ -z "$counts" ]; then
  log "ERROR: cannot compare HEAD with $UPSTREAM"
  exit 1
fi
behind="${counts%%[[:space:]]*}"; ahead="${counts##*[[:space:]]}"
if [ "$behind" -gt 0 ] && [ "$ahead" -gt 0 ]; then
  log "ERROR: $BRANCH has diverged from $UPSTREAM ($ahead unpushed local, $behind remote) — refusing to merge, reset or force; a human must reconcile (git -C $DIR log --oneline $UPSTREAM...HEAD)"
  exit 1
fi
if [ "$behind" -gt 0 ]; then
  git_q merge --ff-only --quiet "$UPSTREAM" || { log "ERROR: fast-forward to $UPSTREAM failed; not generating a snapshot this tick"; exit 1; }
  log "fast-forwarded $behind commit(s) to $UPSTREAM"
fi
[ "$ahead" -gt 0 ] && log "$ahead unpushed commit(s) carried in from an earlier tick; this push publishes them too"

# Regenerate into a temp file; only replace on a non-empty success (never commit a truncated snapshot).
# The generator's stderr is kept and replayed as one stamped line rather than discarded: "gh auth /
# API?" was a guess the log could never confirm, and an unredirected stderr would otherwise land in
# refresh-human-queue.log unstamped.
tmp="$(mktemp)"; tmperr="$(mktemp)"
pr-review-report human-queue --json >"$tmp" 2>"$tmperr"; gen_rc=$?
if [ "$gen_rc" -eq 0 ] && [ -s "$tmp" ]; then
  mv "$tmp" "$DIR/human-queue.json"
  rm -f "$tmperr"
else
  log "generation failed (rc=$gen_rc): $(tr '\n' ' ' <"$tmperr")— keeping the previous snapshot"
  rm -f "$tmp" "$tmperr"
  exit 1
fi

# Commit + push only on a real change — to the snapshot OR to the run-metrics ledger (#160).
# metrics/runs.jsonl rides this tick because the runners that append it never push, and a
# usage-gate pause suspends the very runs whose completion used to be the occasion for committing
# it — while the pause path itself appends one skip row per gated tick. Gating the publish on the
# snapshot alone would hold those rows hostage to unrelated queue churn; either file moving is a
# reason to publish both.
snapshot_changed=1
git -C "$DIR" diff --quiet -- human-queue.json && snapshot_changed=0
metrics_changed=1
git -C "$DIR" diff --quiet -- metrics/runs.jsonl && metrics_changed=0
if [ "$snapshot_changed" -eq 0 ] && [ "$metrics_changed" -eq 0 ]; then
  log "snapshot and run metrics unchanged at $(git -C "$DIR" rev-parse --short HEAD); nothing to publish"
  exit 0
fi

# Append one rollup line {ts, counts} to the append-only history so the dashboard can
# render per-state inventory over time (Theory-of-Constraints flow panel;
# rain-org-health#32). One line per CHANGED snapshot, mirroring metrics/runs.jsonl — so it stays
# gated on the SNAPSHOT having moved: a metrics-only tick appends no history line, or an idle
# queue would grow one identical rollup per skip row.
if [ "$snapshot_changed" -eq 1 ]; then
  # counts come straight from the tool-generated snapshot (the tool stays the single
  # source of truth); ts is this refresh's real UTC time (never synthesized downstream).
  # `queue-history-line` is the same code path the backfill uses, so the live append and the
  # historical rewrite can never produce different line shapes for the same snapshot.
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  histerr="$(mktemp)"
  hist="$(pr-review-report queue-history-line "$DIR/human-queue.json" --ts "$ts" 2>"$histerr")"; hist_rc=$?
  if [ "$hist_rc" -eq 0 ] && [ -n "$hist" ]; then
    printf '%s\n' "$hist" >>"$DIR/human-queue-history.jsonl"
  else
    # The snapshot is what the dashboard reads; a missing history point costs one plot marker, so
    # this is reported rather than fatal. Buffering the line (instead of appending the pipe straight
    # into the file) is what keeps a failure from writing a partial record into an append-only file.
    log "history line failed (rc=$hist_rc): $(tr '\n' ' ' <"$histerr")— publishing the snapshot without it"
  fi
  rm -f "$histerr"
fi

# The commit message names what actually moved: metrics-only ticks keep the `chore(metrics):`
# prefix the file's hand-committed history already uses.
if [ "$snapshot_changed" -eq 1 ] && [ "$metrics_changed" -eq 1 ]; then
  msg="chore(dashboard): refresh human-queue.json snapshot + run metrics"
elif [ "$snapshot_changed" -eq 1 ]; then
  msg="chore(dashboard): refresh human-queue.json snapshot"
else
  msg="chore(metrics): publish accrued run metrics"
fi
git_q add human-queue.json human-queue-history.jsonl metrics/runs.jsonl || exit 1
git_q -c commit.gpgsign=false commit --no-verify -m "$msg" --quiet || exit 1
mine="$(git -C "$DIR" rev-parse HEAD)"

# The remote can still move between the fetch above and this push — a PR merging mid-tick. Replay
# THIS TICK'S snapshot commit onto the new tip and push again, rather than leaving it unpushed and
# the branch diverged (which is what turned one lost race into a permanent wedge before).
#
# The guard is exact rather than a guess about commit subjects: `$mine` is the commit made three
# lines up, and the replay only runs while HEAD is still exactly that commit and it is the only one
# ahead of the upstream. So the rebase moves one known commit and nothing else — no merge is
# fabricated, no remote commit is discarded, nothing is forced. A conflict (the remote touched the
# same files) aborts and is reported; it is not resolved in either side's favour.
for attempt in 1 2 3; do
  if git_q push --quiet; then
    log "pushed $(git -C "$DIR" rev-parse --short HEAD) to $UPSTREAM"
    exit 0
  fi
  git_q fetch --quiet "$REMOTE" || { log "ERROR: fetch after a rejected push failed; the snapshot commit is unpushed"; exit 1; }
  counts="$(sync_counts)"
  behind="${counts%%[[:space:]]*}"; ahead="${counts##*[[:space:]]}"
  if [ "$ahead" != "1" ] || [ "$(git -C "$DIR" rev-parse HEAD)" != "$mine" ]; then
    log "ERROR: not replaying — $BRANCH is $ahead ahead / $behind behind $UPSTREAM and no longer just this tick's snapshot commit; a human must reconcile (git -C $DIR log --oneline $UPSTREAM...HEAD)"
    exit 1
  fi
  if [ "$behind" -gt 0 ]; then
    if ! git_q -c commit.gpgsign=false rebase --quiet "$UPSTREAM"; then
      git_q rebase --abort
      log "ERROR: the snapshot commit does not replay onto $UPSTREAM (the remote changed the same files); a human must reconcile (git -C $DIR log --oneline $UPSTREAM...HEAD)"
      exit 1
    fi
    mine="$(git -C "$DIR" rev-parse HEAD)"
    log "attempt $attempt: replayed the snapshot commit onto $UPSTREAM ($behind new remote commit(s)); retrying the push"
  fi
done
log "ERROR: push still rejected after $attempt attempts; the snapshot commit is committed but unpushed and $BRANCH is diverged — a human must reconcile (git -C $DIR log --oneline $UPSTREAM...HEAD)"
exit 1
