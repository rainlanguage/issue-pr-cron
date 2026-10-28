#!/usr/bin/env bash
# Regenerate the FSM-conformance snapshot (human-queue.json) and publish it, with the other
# dashboard data files, to the `dashboard-data` branch, so the rain-org-health dashboard can fetch
# them at runtime from their raw URLs — no site redeploy for data. `main` is ruleset-protected
# (PR + checks), so data never goes there: this tick builds one commit on top of the branch's
# current tip, replacing exactly the data paths with the install dir's files, and pushes it as a
# plain fast-forward. The install dir's files are the source of truth; the branch is their
# published copy.
# The snapshot is OVERWRITE (point-in-time); alongside it we APPEND one rollup line per changed
# refresh to human-queue-history.jsonl ({ts, counts[, ages]}) for the dashboard's per-state
# inventory over time (rain-org-health#32), and landed-history.jsonl rows for the
# tokens-per-landed-item denominator. This tick also publishes metrics/runs.jsonl (#160) and
# fsm-touches.jsonl: the runners append them in place but never push, and during a usage-gate
# pause this data-only, never-gated cron is the one publisher still awake.
# Packaged as a flake output (`packages.refresh-human-queue`); nix builds PATH from the flake's
# locked nixpkgs. errexit is turned back off — writeShellApplication forces it, but this script
# reads exit status as data (a rejected push is a state to recover from) and decides what to do
# with each one explicitly.
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
# the rev-* and ls-remote queries use their exit status as data, not as failure.
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

# The branch the dashboard reads, and every file published to it. A path the install dir does
# not hold yet (fsm-touches.jsonl is born with the first transition) is neither published nor
# removed from the branch.
DATA_BRANCH="dashboard-data"
DATA_PATHS=(human-queue.json human-queue-history.jsonl landed-history.jsonl metrics/runs.jsonl fsm-touches.jsonl)

# --- keep the install's code current ------------------------------------------------------
# The flake this cron runs from is the checkout's HEAD, so `main` is fast-forwarded here. Nothing
# is ever committed to it by this script, so a failure here is reported and fails the tick, but
# only AFTER the data is published: a stuck code sync must not freeze the dashboard.
# Fast-forward only. A diverged branch means unpushed local work, which is a real error to report,
# not something to merge over, reset away or force past.
BRANCH="$(git -C "$DIR" rev-parse --abbrev-ref HEAD 2>/dev/null)"
REMOTE="$(git -C "$DIR" config --get "branch.$BRANCH.remote")"
UPSTREAM="$(git -C "$DIR" rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null)"
if [ -z "$REMOTE" ] || [ -z "$UPSTREAM" ]; then
  log "ERROR: branch '$BRANCH' has no upstream — there is no remote to publish to"
  exit 1
fi

git_q fetch --quiet "$REMOTE" || { log "ERROR: fetch failed; not generating a snapshot this tick"; exit 1; }
sync_rc=0
counts="$(git -C "$DIR" rev-list --left-right --count "$UPSTREAM...HEAD" 2>/dev/null)"
behind="${counts%%[[:space:]]*}"; ahead="${counts##*[[:space:]]}"
if [ -z "$counts" ]; then
  log "ERROR: cannot compare HEAD with $UPSTREAM"
  sync_rc=1
elif [ "$behind" -gt 0 ] && [ "$ahead" -gt 0 ]; then
  log "ERROR: $BRANCH has diverged from $UPSTREAM ($ahead unpushed local, $behind remote) — refusing to merge, reset or force; a human must reconcile (git -C $DIR log --oneline $UPSTREAM...HEAD)"
  sync_rc=1
elif [ "$behind" -gt 0 ]; then
  if git_q merge --ff-only --quiet "$UPSTREAM"; then
    log "fast-forwarded $behind commit(s) to $UPSTREAM"
  else
    log "ERROR: fast-forward to $UPSTREAM failed"
    sync_rc=1
  fi
fi

# --- the published tip --------------------------------------------------------------------
# Prints the branch's tip, or nothing when the branch does not exist yet (the first publish
# creates it). ls-remote's exit 2 is "no such ref"; any other failure is the network's.
DATA_REF="refs/remotes/$REMOTE/$DATA_BRANCH"
data_tip() {
  git -C "$DIR" ls-remote --exit-code --heads "$REMOTE" "$DATA_BRANCH" >/dev/null 2>&1
  case $? in
    0) git_q fetch --quiet "$REMOTE" "+refs/heads/$DATA_BRANCH:$DATA_REF" || return 1
       git -C "$DIR" rev-parse --verify --quiet "$DATA_REF^{commit}" ;;
    2) return 0 ;;
    *) log "ERROR: cannot read $DATA_BRANCH from $REMOTE"; return 1 ;;
  esac
}
tip="$(data_tip)" || exit 1

# A fresh install (a new box, or a checkout that lacks the files) would otherwise publish its
# empty or brand-new feeds over the branch's whole history. So every data path the install dir
# lacks (absent or empty) that the tip carries is restored from the tip first: before the snapshot
# is compared, before anything is appended. A path the install already holds is never touched.
restore_from_tip() {
  local p blob tmpf
  for p in "${DATA_PATHS[@]}"; do
    [ -s "$DIR/$p" ] && continue
    blob="$(git -C "$DIR" rev-parse --verify --quiet "$tip:$p" 2>/dev/null)" || continue
    if ! tmpf="$(mktemp 2>/dev/null)" || ! git -C "$DIR" cat-file blob "$blob" >"$tmpf" 2>/dev/null \
      || ! mkdir -p "$(dirname "$DIR/$p")" || ! mv "$tmpf" "$DIR/$p"; then
      rm -f "$tmpf"
      log "ERROR: cannot restore $p from $DATA_BRANCH ${tip:0:7}; not publishing over its history"
      return 1
    fi
    log "restored $p from $DATA_BRANCH ${tip:0:7}: the install dir lacked it"
  done
}
[ -z "$tip" ] || restore_from_tip || exit 1

# Regenerate into a temp file; only replace on a non-empty success (never publish a truncated
# snapshot). The generator's stderr is replayed as one stamped line rather than discarded.
tmp="$(mktemp)"; tmperr="$(mktemp)"
pr-review-report human-queue --json >"$tmp" 2>"$tmperr"; gen_rc=$?
if [ "$gen_rc" -ne 0 ] || [ ! -s "$tmp" ]; then
  log "generation failed (rc=$gen_rc): $(tr '\n' ' ' <"$tmperr")— keeping the previous snapshot"
  rm -f "$tmp" "$tmperr"
  exit 1
fi
rm -f "$tmperr"

# `moved` compares against the install dir's PREVIOUS snapshot, not the branch: the history line
# and the landed diff are one per changed snapshot, so a tick replayed after a failed push must
# not append them a second time for the same move.
prevsnap="$(mktemp)"
snapshot_moved=1
if [ -s "$DIR/human-queue.json" ]; then
  cp "$DIR/human-queue.json" "$prevsnap"
  cmp -s "$tmp" "$prevsnap" && snapshot_moved=0
fi
mv "$tmp" "$DIR/human-queue.json"

if [ "$snapshot_moved" -eq 1 ]; then
  # counts come straight from the tool-generated snapshot; ts is this refresh's real UTC time.
  # `queue-history-line` is the same code path the backfill uses. Buffered, so a failure can
  # never write a partial record into an append-only file; a missing line is reported, not fatal.
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  histerr="$(mktemp)"
  hist="$(pr-review-report queue-history-line "$DIR/human-queue.json" --ts "$ts" 2>"$histerr")"; hist_rc=$?
  if [ "$hist_rc" -eq 0 ] && [ -n "$hist" ]; then
    printf '%s\n' "$hist" >>"$DIR/human-queue-history.jsonl"
  else
    log "history line failed (rc=$hist_rc): $(tr '\n' ' ' <"$histerr")— publishing the snapshot without it"
  fi
  rm -f "$histerr"

  # FSM items present in the previous snapshot and gone from the fresh one, verified against
  # GitHub as actually landed. `--existing` makes a replayed diff append nothing twice. rc 3 =
  # the rows are complete minus the items stderr names; backfill-landed-history heals them.
  landederr="$(mktemp)"
  if [ -s "$prevsnap" ]; then
    landed="$(pr-review-report landed-history-lines "$prevsnap" "$DIR/human-queue.json" \
      --observed-at "$ts" --existing "$DIR/landed-history.jsonl" 2>"$landederr")"; landed_rc=$?
    [ -n "$landed" ] && printf '%s\n' "$landed" >>"$DIR/landed-history.jsonl"
    [ "$landed_rc" -ne 0 ] && log "landed-history incomplete (rc=$landed_rc): $(tr '\n' ' ' <"$landederr")— publishing what resolved"
  else
    log "no previous snapshot in the install dir — skipping the landed-history diff this tick"
  fi
  rm -f "$landederr"
fi
rm -f "$prevsnap"

publish_paths=()
for p in "${DATA_PATHS[@]}"; do
  [ -e "$DIR/$p" ] && publish_paths+=("$p")
done

# The present data paths whose content differs from the branch at $1 (every one when $1 is empty).
unpublished() {
  local p mine theirs
  for p in "${publish_paths[@]}"; do
    mine="$(git -C "$DIR" hash-object -- "$p")" || return 1
    theirs=""
    [ -n "$1" ] && theirs="$(git -C "$DIR" rev-parse --verify --quiet "$1:$p" 2>/dev/null)"
    [ "$mine" = "$theirs" ] || printf '%s\n' "$p"
  done
}

moved="$(unpublished "$tip")" || { log "ERROR: cannot hash the data files"; exit 1; }
if [ -z "$moved" ]; then
  log "dashboard data unchanged at $DATA_BRANCH ${tip:0:7}; nothing to publish"
  exit "$sync_rc"
fi

# The commit message names what actually moved.
has() { printf '%s\n' "$moved" | grep -qx "$1"; }
if has human-queue.json && has metrics/runs.jsonl; then
  msg="chore(dashboard): refresh human-queue.json snapshot + run metrics"
elif has human-queue.json; then
  msg="chore(dashboard): refresh human-queue.json snapshot"
elif has metrics/runs.jsonl; then
  msg="chore(metrics): publish accrued run metrics"
elif has fsm-touches.jsonl; then
  msg="chore(metrics): publish accrued fsm touch records"
else
  msg="chore(dashboard): publish accrued dashboard data"
fi

# One commit on top of $1 ('' = the branch's first commit): $1's tree with every present data path
# replaced by the install dir's file. Built in a private index, so the install's own index and
# working tree are never touched. Prints the commit.
build_commit() {
  local work rc out
  work="$(mktemp -d)" || return 1
  out="$(
    export GIT_INDEX_FILE="$work/index"
    if [ -n "$1" ]; then git -C "$DIR" read-tree "$1"; else git -C "$DIR" read-tree --empty; fi || exit 1
    for p in "${publish_paths[@]}"; do
      blob="$(git -C "$DIR" hash-object -w -- "$p")" || exit 1
      git -C "$DIR" update-index --add --cacheinfo "100644,$blob,$p" || exit 1
    done
    tree="$(git -C "$DIR" write-tree)" || exit 1
    parent=()
    [ -n "$1" ] && parent=(-p "$1")
    git -C "$DIR" -c commit.gpgsign=false commit-tree "$tree" "${parent[@]}" -m "$msg"
  )" 2>"$work/err"; rc=$?
  [ "$rc" -eq 0 ] || log "building the $DATA_BRANCH commit failed (rc=$rc): $(tr '\n' ' ' <"$work/err")"
  rm -rf "$work"
  [ "$rc" -eq 0 ] && printf '%s\n' "$out"
  return "$rc"
}

# The feeds are append-only, so a publish may only lengthen them: each present feed must hold the
# copy at $1 as a byte-prefix (or $1 lacks it). Otherwise the publish would shorten or rewrite the
# branch's history, so it names the feed and fails. The snapshot is overwrite and is not checked.
APPEND_ONLY=(human-queue-history.jsonl landed-history.jsonl metrics/runs.jsonl fsm-touches.jsonl)
feeds_extend() {
  local p blob size
  for p in "${APPEND_ONLY[@]}"; do
    [ -e "$DIR/$p" ] || continue
    blob="$(git -C "$DIR" rev-parse --verify --quiet "$1:$p" 2>/dev/null)" || continue
    if ! size="$(git -C "$DIR" cat-file -s "$blob" 2>/dev/null)" \
      || ! git -C "$DIR" cat-file blob "$blob" 2>/dev/null | cmp -s -n "$size" - "$DIR/$p"; then
      log "ERROR: $p does not extend its copy on $DATA_BRANCH ${1:0:7}; not publishing over its history — a human must reconcile"
      return 1
    fi
  done
}

# The branch can move between the fetch and the push. The install dir is the source of truth, so
# a rejected push is answered by rebuilding the same files onto the new tip — never by forcing.
for attempt in 1 2 3; do
  feeds_extend "$tip" || exit 1
  commit="$(build_commit "$tip")" || exit 1
  if git_q push --quiet "$REMOTE" "$commit:refs/heads/$DATA_BRANCH"; then
    log "published ${commit:0:7} to $DATA_BRANCH ($(printf '%s' "$moved" | tr '\n' ' '))"
    exit "$sync_rc"
  fi
  tip="$(data_tip)" || { log "ERROR: re-reading $DATA_BRANCH after a rejected push failed; the data stays unpublished until the next tick"; exit 1; }
  moved="$(unpublished "$tip")" || exit 1
  if [ -z "$moved" ]; then
    log "$DATA_BRANCH already carries this data at ${tip:0:7}"
    exit "$sync_rc"
  fi
  log "attempt $attempt: $DATA_BRANCH moved to ${tip:0:7}; rebuilding onto it"
done
log "ERROR: push to $DATA_BRANCH still rejected after $attempt attempts; the data stays unpublished until the next tick"
exit 1
