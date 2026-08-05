# human-fsm — slash commands for the human's side of the FSM

The [issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron) is a
finite state machine, and `pr-review-report` is its only transition function.
Every actor's hand-off is a labelled transition through that binary — including
the human's. This plugin is the layer above: the commands a human types so both
halves of a decision — the read it rests on and the ruling it becomes — reach
that binary **through typed calls and nothing else**, rather than a hand-written
JSON-RPC frame, a Python filter over the response, and two raw `gh` calls.

| Command                                      | The call it invokes                                                                                                                                                                                                                                                   |
| -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/nr [1-3]`                                  | `next_ready` + `pr_context` + `pr_checkout` + `clone_release` (MCP) and the `audit` skill — the next `ai:ready` PR, and the vetter's verdict checked against its diff, its issue and its source. Writes no GitHub state                                               |
| `/ncc [1-3]`                                 | `next_close_candidate` + `close_candidate_context` + `pr_context` (MCP) — the next `ai:close-candidate` flag, and the producer's reason checked against the issue as filed and the code it claims about. Writes no GitHub state                                       |
| `/close-candidate <owner/repo#n> uphold "…"` | `human-close` — rule, retire `ai:close-candidate`, close. Issue **or** PR, resolved by lookup                                                                                                                                                                         |
| `/close-candidate <owner/repo#n> reject "…"` | `record-close-candidate-verdict … reject` — drop the flag, back to the producer (issue-only)                                                                                                                                                                          |
| `/reject <owner/repo#n> "…"`                 | `human-rule … reject --rework` / `human-rule-issue … reject --rework` — the send-back: `ai:reject` (PR) plus the trusted `Rework note` work order, one call, pinned to the head sha or the issue. `--rework` is REQUIRED on either subject; there is no parked reject |
| `/design <owner/repo#n> "…"`                 | `human-rule … design --rework\|--park` / `human-rule-issue … design --rework\|--park` — `human:design`, delegated as a work order or explicitly parked, never parked by default. Both subjects take exactly one of the two, and the chosen one carries across         |
| `/keep-open <owner/repo#n> "…"`              | `human-rule-issue … keep-open` — the sacred "never re-flag this" (issue-only)                                                                                                                                                                                         |

Names collide across plugins; `/human-fsm:close-candidate` disambiguates.

## The reads and the writes

`/nr` and `/ncc` are the reads that precede a ruling; the rest are the rulings.
They differ in how they reach the binary, and the difference is the point.

The rulings shell out to a `pr-review-report` subcommand. Both reads call **MCP
tools** — served by the `fsm` server this plugin ships in its own manifest — and
**no shell at all**. `/nr` is granted `next_ready`, `pr_context`, `pr_checkout`
and `clone_release`, plus `Skill` and `Read`, which it needs because it puts the
PR's source on disk and audits it. `/ncc` is granted `next_close_candidate`,
`close_candidate_context` and `pr_context`, and those three only: a flag has no
diff and no tree to check out, so there is nothing for `Skill` or `Read` to
reach, and a grant a command cannot use is surface it cannot account for.
Neither falls back to `gh`, neither assembles a field itself, and neither
quietly answers from memory: either the tools answered or the command says so
and stops. That is the guarantee a merge decision needs, because the way this
goes wrong is not a refusal, it is a plausible answer nobody can trace — and a
close decision needs it no less, since upholding a flag closes an issue somebody
filed.

The `allowed-tools` line is a **declaration**, not a sandbox — measured on
Claude Code 2.1.220, a command granting only `Read` still ran a `Bash` call with
no permission denial — so what binds is the command's own prose, and
`cargo test` holds the declaration and the prose to each other:
`command_contract` admits the typed grants plus `Skill` and `Read` by name,
refuses any shell grant beside them, and refuses a shell line fenced anywhere in
the body.

Same reason the tools exist at all. Six reads — verdict note, head sha, base
branch, CI, CodeRabbit coverage, deploy gate — done by hand, in an order the
reader has to remember, is a decision whose inputs are not auditable. A typed
result is one artefact in the trace, with the same fields in the same order for
every caller, whether that is a human, this agent, or the next one.

**Two reads, because one of them is a check on the other.** `next_ready` carries
the vetter's verdict; `pr_context` carries the diff and the linked issue the
verdict is a claim about. A gate that printed only the first would be the
vetter's conclusion in a second typeface, and it failed that way on
`rain.erc4626.words#230` — presented as clean because the note said so, over a
defect in a file neither the vetter nor `/nr` had opened. So `/nr` derives what
the issue asked for, reads the diff against that, and says where its own reading
and the vetter's diverge.

**And a third read, because the rules are written down.** `pr_checkout` puts the
PR's source on disk and the `audit` skill is invoked over it, scoped to the
changed lines plus the code whose behaviour decides whether they are correct.
Reconstructing that rulebook from memory missed `rain.deploy#20`'s caret pragma
in a new concrete test mock and `rain.deploy#21`'s 22 hardcoded copies of a
derivation the same PR had just added — both stated plainly in the skill, both
caught only when a human named them. Running the same skill the vetter runs does
not make this a second vetter: it supplies the mechanical rules, while the scope
question — does the diff do what the issue asked, all of it and nothing else —
has no counterpart in the skill and is where this gate has actually caught
things (`cyclo.site#393`, `#408`, `#331`). The skill's findings feed the read;
they never replace it, and `clone_release` disposes of the tree before the
result is presented.

It costs three more calls, a skill fan-out and real reasoning, which is the
price of a merge decision rather than an overhead to trim.

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
a server that fails to connect and a `/nr` with no tools to call. Nothing secret
belongs in the manifest — the server reads the same `gh` auth the subcommands
do.

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install human-fsm@issue-pr-cron
```
