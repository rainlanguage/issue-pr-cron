#!/usr/bin/env bash
# Cron-scoped PreToolUse hook (RAINIX_CRON_HOOK=1, set by the cron runners). Closes
# the `git -C <dir> …` spellings that EVADE the history-rewrite guards: the deny-list
# and block-history-rewrite.sh both anchor on a BARE `git reset` / `git push`, so
# inserting `-C <dir>` between `git` and the verb slips past them.
#
# KEY: it does not merely block — it TELLS the agent the ALLOWED action to take
# instead, so a blocked agent doesn't guess and reach for something worse. Scoped to
# the cron via the env var, so interactive/other sessions are untouched.
#
# DEPLOY: copy to the box's claude hooks dir and add it as a PreToolUse "Bash" hook
# in the user settings.json (alongside the other block-*.sh hooks).
#
# Exit codes: 0 -> allow; 2 -> block (stderr surfaces to the model).

[ -n "${RAINIX_CRON_HOOK:-}" ] || exit 0

cmd=$(python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("tool_input",{}).get("command",""))
except Exception: pass' 2>/dev/null)
[ -z "$cmd" ] && exit 0

# (1) git -C <dir> reset --hard  — bypasses the bare-`git reset --hard` guards.
if printf '%s' "$cmd" | grep -qE 'git[[:space:]]+-C[[:space:]]+[^|;&]*reset[[:space:]]+--hard'; then
  printf 'BLOCKED — `git -C <dir> reset --hard` bypasses the history-rewrite guard:\n  %s\n\nWHAT TO DO INSTEAD: these per-issue / per-PR clones are throwaway, so to bring one to a CLEAN copy of a branch use checkout-force + clean (identical clean state, no forbidden reset):\n  git -C <dir> fetch origin && git -C <dir> checkout -f -B <branch> origin/<branch> && git -C <dir> clean -fdx\nNever use `git reset --hard` in ANY form.\n' "$cmd" >&2
  exit 2
fi

# (2) git -C <dir> push <force-variant>  — bypasses the bare-`git push` force guards.
if printf '%s' "$cmd" | grep -qE 'git[[:space:]]+-C[[:space:]]+[^|;&]*push[^|;&]*(--force(-with-lease|-if-includes)?|[[:space:]]-f([[:space:]]|$)|[[:space:]][+][[:alnum:]_/.-]+)'; then
  printf 'BLOCKED — `git -C <dir> push <force>` bypasses the history-rewrite guard:\n  %s\n\nWHAT TO DO INSTEAD: push ONLY a plain fast-forward of a NEW commit:\n  git -C <dir> push     (no --force / -f / --force-with-lease / +refspec)\nIf the push is rejected as non-fast-forward, STOP and leave the PR for a human to resolve — never force-push a branch that is under review.\n' "$cmd" >&2
  exit 2
fi

exit 0
