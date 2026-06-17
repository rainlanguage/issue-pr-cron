#!/usr/bin/env bash
# pr-review-report.sh — report every open PR (and logged close-candidate) that needs a HUMAN decision,
# RESPECTING reviews already done: it overlays (a) recorded review verdicts in review-verdicts.jsonl and
# (b) GitHub's own review state (APPROVED / CHANGES_REQUESTED) on top of the CI/mergeability signal, so a
# reviewed-as-reject/dup/relink PR is NOT shown as "ready to merge". Everything prints as full clickable URLs.
#
# Usage:   ./pr-review-report.sh            # all buckets
#          ./pr-review-report.sh --ready    # only the reviewed-&-ready-to-merge bucket
# review-verdicts.jsonl lines: {"repo","pr","verdict":"ready|relink|reject|close","note"}  (edit freely).
# Config from ./cron.env if present (ORG, PR_ASSIGNEE, CLOSE_CANDIDATES, REVIEW_VERDICTS).
set -uo pipefail

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env" 2>/dev/null || true
ORG="${ORG:-rainlanguage}"
AUTHOR="${PR_ASSIGNEE:-thedavidmeister}"
CLOSE_CANDIDATES="${CLOSE_CANDIDATES:-$DIR/close-candidates.jsonl}"
REVIEW_VERDICTS="${REVIEW_VERDICTS:-$DIR/review-verdicts.jsonl}"
ONLY="${1:-}"

if ! command -v gh >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
  if command -v nix >/dev/null 2>&1; then exec nix shell nixpkgs#gh nixpkgs#jq --command "$0" "$@"; fi
  echo "error: need gh + jq on PATH (or nix available)" >&2; exit 1
fi

# per-PR: repo<TAB>num<TAB>mergeable<TAB>ci<TAB>draft<TAB>reviewDecision<TAB>url
classify_one() {
  local repo="$1" num="$2" org="$3" j url merg draft rev fail pend tot ci
  j=$(gh pr view "$num" -R "$org/$repo" --json url,mergeable,isDraft,reviewDecision,statusCheckRollup 2>/dev/null) \
    || { printf '%s\t%s\t?\t?\t?\t?\t-\n' "$repo" "$num"; return; }
  url=$(printf '%s' "$j" | jq -r '.url')
  merg=$(printf '%s' "$j" | jq -r '.mergeable')
  draft=$(printf '%s' "$j" | jq -r 'if .isDraft then "DRAFT" else "-" end')
  rev=$(printf '%s' "$j" | jq -r 'if (.reviewDecision // "") == "" then "-" else .reviewDecision end')
  fail=$(printf '%s' "$j" | jq '[.statusCheckRollup[]?|select(.conclusion=="FAILURE" or .conclusion=="TIMED_OUT" or .conclusion=="CANCELLED" or .conclusion=="ACTION_REQUIRED" or .conclusion=="STARTUP_FAILURE" or .state=="FAILURE" or .state=="ERROR")]|length')
  pend=$(printf '%s' "$j" | jq '[.statusCheckRollup[]?|select(.status=="IN_PROGRESS" or .status=="QUEUED" or .status=="PENDING" or .state=="PENDING")]|length')
  tot=$(printf '%s' "$j" | jq '[.statusCheckRollup[]?]|length')
  if   [ "${fail:-0}" -gt 0 ]; then ci="RED"
  elif [ "${pend:-0}" -gt 0 ]; then ci="PENDING"
  elif [ "${tot:-0}" -eq 0 ];  then ci="NOCHECKS"
  else ci="GREEN"; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$repo" "$num" "$merg" "$ci" "$draft" "$rev" "$url"
}
export -f classify_one

# verdict lookup: VKEY["repo/num"]=verdict
declare -A VERD SRC
if [ -s "$REVIEW_VERDICTS" ]; then
  while IFS=$'\t' read -r k v s; do VERD["$k"]="$v"; SRC["$k"]="${s:-ai-campaign}"; done < <(jq -r 'select(type=="object")|.repo+"/"+(.pr|tostring)+"\t"+.verdict+"\t"+(.source//"ai-campaign")' "$REVIEW_VERDICTS" 2>/dev/null)
fi

# bucket(verdict, reviewDecision, mergeable, ci, draft) -> bucket key
# pipeline:  unreviewed -> AI-vetted -> you approve -> merge
bucket() {
  local v="$1" src="$2" rev="$3" merg="$4" ci="$5" draft="$6"
  # AI/human flagged a disposition -> that IS the action, regardless of CI.
  case "$v" in close) echo CLOSE; return;; reject) echo REJECT; return;; relink) echo RELINK; return;; esac
  [ "$rev" = CHANGES_REQUESTED ] && { echo REJECT; return; }
  [ "$draft" = DRAFT ] && { echo DRAFT; return; }
  # A REAL red is the PRODUCER's work to drive green -> it is NOT a terminal "blocked"
  # state and NOT a human action. Surface it independently of review status: the red has
  # to be fixed before vet/approve/merge matter again.
  [ "$ci" = RED ] && { echo PRODUCER_FIX; return; }
  [ "$merg" = CONFLICTING ] && { echo CONFLICTING; return; }
  [ "$ci" = PENDING ] && { echo PENDING; return; }
  # green / nochecks + mergeable -> bucket by review state
  local approved=0 aivet=0
  # human approval = a GitHub APPROVED review, or a verdict you set with source=human
  { [ "$rev" = APPROVED ] || { [ "$v" = ready ] && [ "$src" = human ]; }; } && approved=1
  # AI-vetted = the review campaign passed it (source=ai-campaign), not yet human-approved
  { [ "$approved" = 0 ] && [ "$v" = ready ] && [ "$src" = ai-campaign ]; } && aivet=1
  if [ "$merg" = MERGEABLE ] && { [ "$ci" = GREEN ] || [ "$ci" = NOCHECKS ]; }; then
    [ "$approved" = 1 ] && { echo APPROVED; return; }
    [ "$aivet" = 1 ]   && { echo AIVET; return; }
    echo UNREVIEWED; return
  fi
  echo PENDING   # mergeability still resolving (mergeable=UNKNOWN)
}

echo "PR review report — $ORG, author $AUTHOR — $(date -u +%FT%TZ)"
echo "pipeline:  🟦 unreviewed  →  🤖 AI-vetted  →  ✅ you approve  →  merge"
echo "(AI verdicts: review-verdicts.jsonl · your sign-off: a GitHub approval, or a verdict with source=human)"
echo "================================================================"

TMP=$(mktemp); BKT=$(mktemp)
gh search prs --owner "$ORG" --author "$AUTHOR" --state open --limit 300 --json repository,number \
  --jq '.[]|.repository.name+" "+(.number|tostring)' 2>/dev/null \
  | xargs -P12 -n2 bash -c 'classify_one "$1" "$2" "'"$ORG"'"' _ > "$TMP" 2>/dev/null

while IFS=$'\t' read -r repo num merg ci draft rev url; do
  [ -z "$repo" ] && continue
  v="${VERD[$repo/$num]:-}"
  s="${SRC[$repo/$num]:-ai-campaign}"
  b=$(bucket "$v" "$s" "$rev" "$merg" "$ci" "$draft")
  printf '%s\t%s\t%s\n' "$b" "$url" "$v" >> "$BKT"
done < "$TMP"

emit() { # bucket-key  title
  local n; n=$(awk -F'\t' -v b="$1" '$1==b' "$BKT" | wc -l)
  [ "$n" -eq 0 ] && return
  echo; echo "$2  ($n)"
  awk -F'\t' -v b="$1" '$1==b {print "  "$2}' "$BKT" | sort
}

emit APPROVED        "✅ APPROVED BY YOU — ready to merge (GitHub approval / verdict you set)"
[ "$ONLY" = "--ready" ] && { rm -f "$TMP" "$BKT"; exit 0; }
emit AIVET           "🤖 AI-VETTED — awaiting YOUR approval (passed the automated review; NOT human-reviewed yet)"
emit PRODUCER_FIX    "🔴 RED — NEEDS A PRODUCER FIX (CI failing; the producer cron diagnoses it and pushes a fix to drive it green — producer work, NOT 'blocked', NOT your action)"
emit RELINK          "🔧 AI-flagged — relink Closes→Refs before merge (else it auto-closes a live issue)"
emit REJECT          "❌ AI-flagged / you requested changes — rework or close"
emit CLOSE           "🗑️  AI-flagged — close (duplicate / superseded)"
emit UNREVIEWED      "🟦 NOT YET REVIEWED — green + mergeable, awaiting AI review + your approval"
emit CONFLICTING     "⚠️  CONFLICTING — needs a rebase onto current main (producer work)"
emit PENDING         "🟡 PENDING — CI / mergeability still resolving (no action; just wait)"
emit DRAFT           "📝 DRAFTS — intentionally not ready"

if [ -s "$CLOSE_CANDIDATES" ]; then
  echo; echo "🗑️  ISSUE CLOSE-CANDIDATES — cron logged already-fixed/invalid issues (never closed)"
  jq -r --arg org "$ORG" 'select(type=="object" and (.issue!=null)) | "  "+(.url // ("https://github.com/"+(if (.repo|test("/")) then .repo else $org+"/"+.repo end)+"/issues/"+(.issue|tostring)))+"  — "+((.reason//.note//"")[0:80])' "$CLOSE_CANDIDATES" 2>/dev/null | sort -u
fi

echo; echo "----------------------------------------------------------------"
echo "totals: $(wc -l < "$TMP") open PRs by $AUTHOR  ·  buckets:"
awk -F'\t' '{print $1}' "$BKT" | sort | uniq -c | sort -rn | sed 's/^/   /'
rm -f "$TMP" "$BKT"
