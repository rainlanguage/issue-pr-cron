---
paths:
  - "pr-review-report-rs/**"
---

# The transition function

- **A guard lives in the tool, tested once, not re-asserted in prose.** Verdict
  vocabulary, mandatory in-range cost, well-formed refs and the human-sacred
  refusal belong in `validate_call` / `verdict_plan`; a rule a prompt merely
  states is a rule that drifts.
- **A refusal reports EVERY unmet entry at once and prints what would satisfy
  it.** The vetter cannot escalate to a human the way a producer can, so a
  correct verdict must never be more than ONE corrected call from being
  recorded.
- **A claim checked against absent input is not checked.** Where the evidence a
  guard reads is missing — a changed-file set whose diff carries no
  `diff --git` header — refuse outright. A guard that silently stops firing is
  the failure it exists to prevent, one level up.
- **A verdict accounts for every file the PR changes**, and the claim is an
  ARGUMENT the tool verifies rather than a rule the prompt states: each
  hand-written `covered` entry carries a new-side line anchor checked against
  the PR's own diff, the same move `Reviewed <sha>:` makes for the head. Where
  an anchor cannot exist — generated, vendored, lockfile, binary, or a file the
  diff shows no new-side line for — the name alone stands, because a gate
  nothing can pass is not a gate.
- **A profile is a SURFACE filter, not a permission.** `tools/list` returns one
  role's tools, so neither role pays preamble for the other's schemas and
  neither can name the other's transitions. Widening a profile widens the
  machine.
