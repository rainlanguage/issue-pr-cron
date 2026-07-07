# CLAUDE.md — issue-pr-cron

## The pipeline is a finite state machine

This repo runs an autonomous PR pipeline (a **producer** cron and a **vetter**
cron; landing is interactive). Treat it as a **finite state machine** — that
framing is what keeps it debuggable and honest:

- **The states are GitHub state.** A PR's state is its `ai:*` / `human:*`
  labels, its trusted `🤖 ai:vetter` / `🤖 ai:producer` comments, and its native
  `reviewDecision`. GitHub is the **single source of truth** — there is no
  separate local state. (The old `review-verdicts.jsonl` / `review-costs.jsonl`
  ledgers are being removed; cost moves into the vetter comment.)
- **The `pr-review-report` Rust tool is the ONLY transition function.** Every
  move between states — record a verdict, present the queue, backfill a comment,
  gc a clone, check closing keywords, read a comment — is a **tested**
  `pr-review-report` subcommand.
- **Raw `gh` / `git` in a prompt is a _loose_ transition.** It mutates state
  outside the transition function: unenforced, untested, and free to drift
  between the producer and the vetter. **A pipeline with loose transitions is
  not actually a finite state machine** — it's a diagram of one. So the producer
  and vetter prompts must do **all** GitHub input and output through
  `pr-review-report` subcommands, never raw `gh`. A read or write not yet
  covered by a subcommand is a gap to close by **adding one** — not a license to
  reach for `gh`.

North star for any change here: if you're about to instruct a prompt to call
`gh`, stop and add (or extend) a tool subcommand instead.

## Transitions (subcommands)

The state diagram lives in [README.md](README.md#pipeline-state-machine). The
transition functions:

| Subcommand                                                 | Transition it effects                                                                                   |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `--queue`                                                  | surfaces the presentable review queue (`ai:ready` + green + mergeable + vetted-at-head)                 |
| `--record-verdict <owner/repo> <n> <verdict> …`            | the vetter's write: apply the `ai:*` label + post the sha-bound `🤖 ai:vetter` comment (+ cost)         |
| `--trusted-comments <owner/repo> <n> [--marker] [--issue]` | author-verified comment read — the only trusted way to read a comment                                   |
| `--commit-closes <owner/repo> <n>`                         | closing-keyword vs. `closingIssuesReferences` drift check                                               |
| `--backfill-comments`                                      | one-time completion of the ledger→GitHub migration (replays each ledger verdict as its missing comment) |
| `--gc-clones <work-dir>`                                   | reclaim merged/closed work-clones (state cleanup)                                                       |

## Invariants

- **Human decisions are sacred.** A `human:*` label OR a native `APPROVED` /
  `CHANGES_REQUESTED` review is never overwritten by the vetter —
  `--record-verdict` refuses (exit 3), closing the TOCTOU race.
- **Comments are trusted by AUTHOR, never by marker text.** Any third party can
  post a `🤖 ai:vetter` / `🤖 ai:producer` / "Rework note" line; a comment
  counts only when the trusted account authored it. Read via
  `--trusted-comments`.
- **Vetted-at-head.** An `ai:*` label alone is not a verdict; a PR is vetted
  only when its trusted `🤖 ai:vetter` comment pins the **current** head. A
  moved head (code changed) → un-vetted → re-vet.
- **Landing is interactive-only** (the merge cron is retired):
  `gh pr merge --merge --admin` on the human's explicit per-PR word, after the
  SHA-bound review gate.
