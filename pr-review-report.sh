#!/usr/bin/env bash
# pr-review-report.sh — report every open PR (and logged close-candidate) that needs a HUMAN decision.
# Buckets the org's open PRs by the action they're waiting on: merge / resolve-conflict / fix-or-judge,
# plus the close-candidates the cron logged. Everything is printed as full clickable URLs.
#
# Usage:   ./pr-review-report.sh            # all buckets
#          ./pr-review-report.sh --ready    # only the merge-ready bucket
#          ORG=rainlanguage PR_ASSIGNEE=thedavidmeister ./pr-review-report.sh
# Config is read from ./cron.env if present (ORG, PR_ASSIGNEE, CLOSE_CANDIDATES).
set -uo pipefail

DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
# shellcheck disable=SC1091
[ -f "$DIR/cron.env" ] && . "$DIR/cron.env" 2>/dev/null || true
ORG="${ORG:-rainlanguage}"
AUTHOR="${PR_ASSIGNEE:-thedavidmeister}"
CLOSE_CANDIDATES="${CLOSE_CANDIDATES:-$DIR/close-candidates.jsonl}"
ONLY="${1:-}"

# Need gh + jq; if missing, re-exec under nix (so it works in the cron's bare env too).
if ! command -v gh >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
  if command -v nix >/dev/null 2>&1; then exec nix shell nixpkgs#gh nixpkgs#jq --command "$0" "$@"; fi
  echo "error: need gh + jq on PATH (or nix available to provision them)" >&2; exit 1
fi

# --- per-PR classifier: prints  repo<TAB>num<TAB>mergeable<TAB>ci<TAB>draft<TAB>url ---
classify_one() {
  local repo="$1" num="$2" org="$3" j url mergeable draft fail pend tot
  j=$(gh pr view "$num" -R "$org/$repo" --json url,mergeable,isDraft,statusCheckRollup 2>/dev/null) || { printf '%s\t%s\t?\t?\t?\t-\n' "$repo" "$num"; return; }
  url=$(printf '%s' "$j" | jq -r '.url')
  mergeable=$(printf '%s' "$j" | jq -r '.mergeable')
  draft=$(printf '%s' "$j" | jq -r 'if .isDraft then "DRAFT" else "-" end')
  fail=$(printf '%s' "$j" | jq '[.statusCheckRollup[]?|select(.conclusion=="FAILURE" or .conclusion=="TIMED_OUT" or .conclusion=="CANCELLED" or .conclusion=="ACTION_REQUIRED" or .conclusion=="STARTUP_FAILURE" or .state=="FAILURE" or .state=="ERROR")]|length')
  pend=$(printf '%s' "$j" | jq '[.statusCheckRollup[]?|select(.status=="IN_PROGRESS" or .status=="QUEUED" or .status=="PENDING" or .state=="PENDING")]|length')
  tot=$(printf '%s' "$j" | jq '[.statusCheckRollup[]?]|length')
  local ci
  if   [ "${fail:-0}" -gt 0 ]; then ci="RED"
  elif [ "${pend:-0}" -gt 0 ]; then ci="PENDING"
  elif [ "${tot:-0}" -eq 0 ];  then ci="NOCHECKS"
  else ci="GREEN"; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$repo" "$num" "$mergeable" "$ci" "$draft" "$url"
}
export -f classify_one

echo "PR review report — $ORG, author $AUTHOR — $(date -u +%FT%TZ)"
echo "================================================================"

TMP=$(mktemp)
gh search prs --owner "$ORG" --author "$AUTHOR" --state open --limit 300 --json repository,number \
  --jq '.[]|.repository.name+" "+(.number|tostring)' 2>/dev/null \
  | xargs -P12 -n2 bash -c 'classify_one "$1" "$2" "'"$ORG"'"' _ > "$TMP" 2>/dev/null
total=$(wc -l < "$TMP")

section() { # title  awk-filter
  local title="$1" filt="$2" n
  n=$(awk -F'\t' "$filt" "$TMP" | wc -l)
  echo; echo "$title  ($n)"
  awk -F'\t' "$filt {print \"  \"\$6}" "$TMP" | sort
}

[ -z "$ONLY" -o "$ONLY" = "--ready" ] && \
  section "✅ READY TO MERGE — your merge approval" '$3=="MERGEABLE" && ($4=="GREEN"||$4=="NOCHECKS") && $5!="DRAFT"'
[ -z "$ONLY" ] && section "⚠️  CONFLICTING — rebase or close" '$3=="CONFLICTING"'
[ -z "$ONLY" ] && section "🔴 RED CI — needs a fix or a judgment call" '$4=="RED" && $5!="DRAFT"'
[ -z "$ONLY" ] && section "🟡 PENDING / mergeability unknown — not ready yet" '($4=="PENDING" || ($3=="UNKNOWN" && $4!="RED")) && $5!="DRAFT"'
[ -z "$ONLY" ] && section "📝 DRAFTS — intentionally not ready" '$5=="DRAFT"'

# --- close-candidates the cron logged (issues + PRs it thinks should close) ---
if [ -z "$ONLY" ] && [ -s "$CLOSE_CANDIDATES" ]; then
  echo; echo "🗑️  CLOSE-CANDIDATES — your close decision (cron logged, never closed)"
  jq -r 'select(type=="object") | "  "+(.url // (.repo+"#"+((.issue//.pr)|tostring)))+"  — "+((.reason//.note//"")[0:90])' "$CLOSE_CANDIDATES" 2>/dev/null | sort -u
fi

echo; echo "----------------------------------------------------------------"
echo "totals: $total open PRs by $AUTHOR"
rm -f "$TMP"
