# human-fsm — slash commands for the human's FSM transitions

The [issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron) is a finite
state machine, and `pr-review-report` is its only transition function. Every actor's
hand-off is a labelled transition through that binary — including the human's. This
plugin is the layer above: the commands a human types so a ruling is **one invocation**
rather than a hand-written JSON-RPC frame, a Python filter over the response, and two
raw `gh` calls.

| Command                                       | The transition it invokes                                                                    |
| --------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `/close-candidate <owner/repo#n> uphold "…"`  | `human-close` — rule, retire `ai:close-candidate`, close. Issue **or** PR, resolved by lookup |
| `/close-candidate <owner/repo#n> reject "…"`  | `record-close-candidate-verdict … reject` — drop the flag, back to the producer (issue-only) |
| `/reject <owner/repo#n> "…"`                  | `human-rule` / `human-rule-issue` — `human:reject`, pinned to the head sha or the issue      |
| `/design <owner/repo#n> "…"`                  | `human-rule` / `human-rule-issue` — `human:design`                                           |
| `/keep-open <owner/repo#n> "…"`               | `human-rule-issue … keep-open` — the sacred "never re-flag this" (issue-only)                |

Names collide across plugins; `/human-fsm:close-candidate` disambiguates.

## What lives here and what does not

**Nothing here writes GitHub state.** Every guard — the ruling vocabulary, the
mandatory note, the provenance anchor, the stranded-flag refusal, the subject-type
check, terminal-is-moot, idempotence, re-ruling-supersedes — is enforced in
`pr-review-report`, where it is unit- and mutation-tested. A command that re-derived a
transition, or reached for raw `gh` to do what a subcommand already does, would be a
loose transition: unenforced, untested, and free to drift from what the crons do.

So a refusal is the answer, not an obstacle. Each one names the moves that are legal
from the state the subject is actually in, one command each.

## The subject is always `owner/repo#n`

One label name covers two separately-sized populations —
`lanes.vetter-verdicts.ai:close-candidate` counts **PRs**,
`closeCandidateIssues` counts **issues**. A bare number would eventually rule on the
wrong one. Where a transition can act on either, the binary resolves the type by
lookup; where it cannot, it refuses by naming which subject you actually referenced and
handing over the command for it.

## Requirements

`pr-review-report` on `PATH`:

```
nix profile install github:rainlanguage/issue-pr-cron#pr-review-report
```

or substitute the absolute path from
`nix build --no-link --print-out-paths <install-dir>#pr-review-report`.

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install human-fsm@issue-pr-cron
```
