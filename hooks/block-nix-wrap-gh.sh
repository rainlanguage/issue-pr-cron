#!/usr/bin/env bash
# Claude Code PreToolUse hook for Bash. Closes the deny-list nix-wrap bypass in
# the rainlanguage cron context ONLY (the cron runners export RAINIX_CRON_HOOK=1).
#
# DEPLOY: copy to the box's claude hooks dir and add it as a PreToolUse "Bash"
# hook in the user settings.json (alongside the other block-*.sh hooks). The
# cron runners (campaign/review/merge-run.sh) export RAINIX_CRON_HOOK so this
# only ever fires for cron runs, never interactive sessions.
#
# WHY: the cron runner already wraps claude in `nix shell nixpkgs#gh nixpkgs#jq …`,
# so gh and jq are on PATH — the model MUST invoke BARE `gh`/`jq`. Re-wrapping gh
# in a nested `nix shell nixpkgs#gh --command gh …` (or `nix run nixpkgs#gh -- …`,
# or `nix shell nixpkgs#gh --command bash -c '… gh pr edit …'`) makes the command
# start with `nix`, so the `Bash(gh …)` deny-list (prefix-matched) never fires —
# letting a denied gh subcommand (pr edit/merge/close, issue close/comment/edit,
# workflow run, …) through. The prompt forbids this (step 7) but the model has
# done it anyway, so enforce it here where it cannot be bypassed. gh being on the
# cron PATH means there is NO legitimate reason for a cron tool-call to name
# `nixpkgs#gh`; blocking that token forces bare gh and re-arms the deny-list.
#
# Exit codes: 0 -> allow; 2 -> block (stderr surfaces to the model).

[ -n "${RAINIX_CRON_HOOK:-}" ] || exit 0

cmd=$(python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("tool_input",{}).get("command",""))
except Exception: pass' 2>/dev/null)
[ -z "$cmd" ] && exit 0

if printf '%s' "$cmd" | grep -qE 'nixpkgs#gh\b'; then
  printf 'blocked nix-wrapped gh: %s\n' "$cmd" >&2
  printf 'gh is ALREADY on PATH in this cron — invoke BARE `gh …` (and bare `jq`) so the deny-list applies. Wrapping gh in `nix shell/run nixpkgs#gh` bypasses the deny-list and is forbidden (prompt step 7).\n' >&2
  exit 2
fi

exit 0
