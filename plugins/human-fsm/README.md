# human-fsm — slash commands for the human's side of the FSM

The [issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron) is a
finite state machine, and `pr-review-report` is its only transition function.
Every actor's hand-off is a labelled transition through that binary — including
the human's. This plugin is the layer above: the commands a human types so both
halves of a decision — the read it rests on and the ruling it becomes — are
**one invocation** each, rather than a hand-written JSON-RPC frame, a Python
filter over the response, and two raw `gh` calls.

| Command                                      | The call it invokes                                                                            |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `/nr [1-3]`                                  | `next_ready` (MCP) — the next `ai:ready` PR's whole merge decision. A **read**; writes nothing |
| `/close-candidate <owner/repo#n> uphold "…"` | `human-close` — rule, retire `ai:close-candidate`, close. Issue **or** PR, resolved by lookup  |
| `/close-candidate <owner/repo#n> reject "…"` | `record-close-candidate-verdict … reject` — drop the flag, back to the producer (issue-only)   |
| `/reject <owner/repo#n> "…"`                 | `human-rule` / `human-rule-issue` — `human:reject`, pinned to the head sha or the issue        |
| `/design <owner/repo#n> "…"`                 | `human-rule` / `human-rule-issue` — `human:design`                                             |
| `/keep-open <owner/repo#n> "…"`              | `human-rule-issue … keep-open` — the sacred "never re-flag this" (issue-only)                  |

Names collide across plugins; `/human-fsm:close-candidate` disambiguates.

## The read and the writes

`/nr` is the read that precedes a ruling; the rest are the rulings. They differ
in how they reach the binary, and the difference is the point.

The rulings shell out to a `pr-review-report` subcommand. `/nr` calls the **MCP
tool** `next_ready`, served by the `fsm` server this plugin ships in its own
manifest — so the command's `allowed-tools` is that one tool and **nothing
else**. It cannot fall back to `gh`, cannot assemble the answer itself, and
cannot quietly answer from memory: either the tool answered or the command
failed loudly. That is the guarantee a merge decision needs, because the way
this goes wrong is not a refusal, it is a plausible answer nobody can trace.

Same reason the tool exists at all. Six reads — verdict note, head sha, base
branch, CI, CodeRabbit coverage, deploy gate — done by hand, in an order the
reader has to remember, is a decision whose inputs are not auditable. One typed
result is one artefact in the trace, with the same fields in the same order for
every caller, whether that is a human, this agent, or the next one.

## What lives here and what does not

**Nothing here writes GitHub state.** Every guard — the ruling vocabulary, the
mandatory note, the provenance anchor, the stranded-flag refusal, the
subject-type check, terminal-is-moot, idempotence, re-ruling-supersedes — is
enforced in `pr-review-report`, where it is unit- and mutation-tested. A command
that re-derived a transition, or reached for raw `gh` to do what a subcommand
already does, would be a loose transition: unenforced, untested, and free to
drift from what the crons do.

So a refusal is the answer, not an obstacle. Each one names the moves that are
legal from the state the subject is actually in, one command each.

## The subject is always `owner/repo#n`

One label name covers two separately-sized populations —
`lanes.vetter-verdicts.ai:close-candidate` counts **PRs**,
`closeCandidateIssues` counts **issues**. A bare number would eventually rule on
the wrong one. Where a transition can act on either, the binary resolves the
type by lookup; where it cannot, it refuses by naming which subject you actually
referenced and handing over the command for it.

## Requirements

`pr-review-report` on `PATH`:

```
nix profile install github:rainlanguage/issue-pr-cron#pr-review-report
```

or substitute the absolute path from
`nix build --no-link --print-out-paths <install-dir>#pr-review-report`.

The `fsm` MCP server is spawned as that same bare command and inherits the
session's environment, so a `pr-review-report` that is not on `PATH` shows up as
a server that fails to connect and a `/nr` with no tool to call. Nothing secret
belongs in the manifest — the server reads the same `gh` auth the subcommands
do.

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install human-fsm@issue-pr-cron
```
