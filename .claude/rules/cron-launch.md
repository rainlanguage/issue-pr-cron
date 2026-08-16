---
paths:
  - "review-run.sh"
  - "campaign-run.sh"
  - "*-mcp.json"
  - "*-settings.json"
  - "hooks/**"
---

# How the crons are launched

- **The vetter's MCP surface REPLACES its Bash**, which is why `review-run.sh`
  passes `--strict-mcp-config`: a non-FSM operation must be unrepresentable
  rather than merely denied, since a Bash deny-list is prefix-matched and
  bypassable. Dropping the flag, or adding a fallback prompt or settings file,
  silently restores every loose transition. There is no run-time selection
  between surfaces.
- **The producer's server is ADDITIVE** — no `--strict-mcp-config` — because
  what it gains is a clone lifecycle it could not otherwise perform; it keeps
  its Bash.
- **Dispatch adds no transition.** Deep source reading happens in a sub-agent
  briefed through `--agents`, so its context dies with it instead of being
  re-read on every turn of the main loop. What comes back is EVIDENCE: the
  sub-agent's `tools` list names no write, the session deny-list reaches inside
  it, and recording the verdict stays a main-loop move.
- **A hook is not an excuse to leave a transition loose.** `hooks/` holds what
  has not been converted yet; a converted guard is a `pr-review-report`
  subcommand that Claude Code runs as a PreToolUse hook, so it is tested and
  covered by the nix build like every other transition.
