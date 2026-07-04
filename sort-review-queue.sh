#!/usr/bin/env bash
# Print the human review queue sorted by verification cost (cheapest first).
#
# Queue = every OPEN, non-draft PR whose effective (last-wins-by-position)
# verdict in review-verdicts.jsonl is verdict=ready AND source=ai-campaign
# (human-decided PRs are already dispositioned and excluded).
#
# Cost = integer 0-1000 (vibes; see review-prompt.txt rubric). Sourced from the
# verdict line's own `cost` field when present (vetter stamps it at vetting
# time), else from the review-costs.jsonl backfill sidecar (repo+pr keyed,
# sha-matching entry preferred). PRs with no score sort last at cost 1001.
#
# Usage: ./sort-review-queue.sh [N]   (default: top 20; 0 = all)

set -euo pipefail
DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
TOP="${1:-20}"

OPEN_JSON="$(gh search prs --owner rainlanguage --owner cyclofinance --state open --limit 1000 \
  --json repository,number,isDraft,url 2>/dev/null)"

OPEN_JSON="$OPEN_JSON" TOP="$TOP" python3 - "$DIR" <<'EOF'
import json, os, sys

d = sys.argv[1]
top = int(os.environ.get('TOP', '20'))

verdicts = {}
with open(os.path.join(d, 'review-verdicts.jsonl')) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            v = json.loads(line)
        except json.JSONDecodeError:
            continue
        verdicts[(v.get('repo'), v.get('pr'))] = v

costs = {}
sidecar = os.path.join(d, 'review-costs.jsonl')
if os.path.exists(sidecar):
    with open(sidecar) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                c = json.loads(line)
            except json.JSONDecodeError:
                continue
            costs[(c.get('repo'), c.get('pr'))] = c

rows = []
for p in json.loads(os.environ['OPEN_JSON']):
    if p.get('isDraft'):
        continue
    repo = p['repository']['name']
    key = (repo, p['number'])
    v = verdicts.get(key)
    if not v or v.get('verdict') != 'ready' or v.get('source') != 'ai-campaign':
        continue
    cost, basis, stale = None, '', ''
    if isinstance(v.get('cost'), (int, float)):
        cost, basis = int(v['cost']), v.get('cost_basis', '')
    elif key in costs:
        c = costs[key]
        cost, basis = int(c['cost']), c.get('basis', '')
        if c.get('sha') and v.get('sha') and c['sha'] != v['sha']:
            stale = ' [cost from older head]'
    rows.append((cost if cost is not None else 1001, repo, p['number'], p.get('url', ''), basis + stale))

rows.sort(key=lambda r: (r[0], r[1], r[2]))
shown = rows if top == 0 else rows[:top]
print(f"review queue: {len(rows)} PRs (showing {len(shown)}, cheapest first)\n")
for cost, repo, num, url, basis in shown:
    c = 'unscored' if cost == 1001 else f"{cost:>4}"
    print(f"  {c}  {repo}#{num}  {basis}\n        {url}")
EOF
