# CLAUDE.md — issue-pr-cron

## The pipeline is a finite state machine

This repo runs an autonomous PR pipeline (a **producer** cron and a **vetter**
cron; landing is interactive). Treat it as a **finite state machine** — that
framing is what keeps it debuggable and honest:

- **The states are GitHub state.** A PR's state is its `ai:*` / `human:*`
  labels, its trusted `🤖 ai:vetter` / `🤖 ai:producer` comments, and its native
  `reviewDecision`. GitHub is the **single source of truth** — there is no
  separate local state. (The `review-verdicts.jsonl` / `review-costs.jsonl`
  ledgers are gone; cost lives in the vetter comment.)
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

**Where a subcommand cannot reach, a PreToolUse hook can — and the hook should
still BE a subcommand.** A tool surface binds only a session that was launched
with it, so it holds nothing for a session opened outside the cron, while the
loose `gh pr create` transition is reachable from every session on the box. That
is a reason to change WHERE the transition function is invoked from, not a
licence to write the guard in bash: `require-qa-block` is a `pr-review-report`
subcommand that Claude Code runs as a PreToolUse `Bash` hook (#83), so the gate
is tested, shipped in the flake closure, and covered by the nix build like every
other transition. `hooks/` holds what has not been converted yet — the two
deny-list bypass guards (`block-nix-wrap-gh.sh`, `block-cron-git-bypass.sh`),
tracked by #10. A hook is not an excuse to leave a transition loose; it is what
holds the invariant while it still is. See
[README.md](README.md#pretooluse-guards--what-a-prompt-cannot-hold).

**A gate on one edge needs a transition on the other.** `require-qa-block`
guards PR-**open**, so it cannot reach a PR already open — and when the vetter
started rejecting for a missing QA block, the producer had no move that fixed
one. Body edits were denied and no subcommand wrote one, so 122 of 160 open PRs
sat with a body the gate itself would refuse (#51). The answer is not to widen
the deny-list back out to the whole of `gh pr edit`; it is `repair-qa-block`, a
transition narrow enough to say what it does — **append** the section, never
rewrite the body — and validated with the **gate's own predicate**
(`carries_qa_block`), so what one writes is what the other accepts. **When a
guard names a defect, check that some transition can clear it**; a reject with
no exit is a deadlock however correct the reject is.

`ai:relink` was the same shape and went unnoticed for longer, because it was a
whole VERDICT rather than a rejection ground: it told the producer to change a
body `Closes #N` to `Refs #N`, and the producer's every body write was denied
(`Bash(gh pr edit:*)`) or absent (its MCP profile was four clone tools). One PR
sat in it. #135 retires the verdict — a linkage error is a `reject` whose note
names the reference, because it always named the same owner and the same move —
and #136 is the transition it never had, `weaken-closes`. The two had to land
together: consolidating alone would have moved an unexecutable instruction into
a bigger bucket, where it is harder to notice, which is the failure mode this
whole section is about.

**A tool that can only WEAKEN.** `weaken-closes` may rewrite `Closes` to `Refs`
and never the reverse, and that direction is the invariant rather than a
default: `Closes` is what GitHub resolves into `closingIssuesReferences`, and
`uncovered-issues` computes the producer's own backlog from that set. A producer
able to ADD one could mark an issue covered without fixing it — marking its own
homework, on its own inbox. Weakening can only ever GROW that inbox. It is held
three ways: the only text an edit carries is the `Refs` constant, the spans come
from the same scanner `commit-closes` uses, and the planner re-runs
`closing_keywords` over the result and refuses any plan whose closing set gained
a number.

**Both body repairs are producer TOOLS** (#136). `repair-qa-block` was reachable
only as a subcommand under the `Bash(pr-review-report:*)` allow rule, so the
producer's ability to write a PR body was a prefix-matched permission rather
than an enumerable transition — and two body repairs with two call shapes is
exactly the split a future reader has to guess at. Both are on the profile now,
and both keep their subcommand: a session opened outside the cron has no MCP
surface, and those are the sessions the QA retrofit exists for (#83).

**A PreToolUse guard is a guard against honest omission, not a security
boundary.** It reads a command line with a lexer that resolves quoting and
nothing else — it is not bash and never will be — so a determined bypass always
exists (a script file it cannot read, a variable it cannot expand). What these
guards buy is that the common ACCIDENT becomes impossible: forgetting the QA
block, or reaching for a `nix`-wrapped `gh` out of habit. Where the gate cannot
tell what a command does it refuses and says so, which is the right posture for
an accident but is not the same thing as enforcement. Do not cite one as proof
that a rule cannot be broken — cite it as proof that breaking it has to be
deliberate.

## Transitions (subcommands)

The state diagram lives in [README.md](README.md#pipeline-state-machine). The
transition functions:

| Subcommand                                                                      | Transition it effects                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--queue`                                                                       | surfaces the presentable review queue (`ai:ready` + green + mergeable + vetted-at-head)                                                                                                                                                                                                                                                                                                            |
| `--record-verdict <owner/repo> <n> <verdict> … --covered-file <p>`              | the vetter's write: apply the `ai:*` label + post the `🤖 ai:vetter` comment, bound to the head sha and stamped with the vet protocol (+ cost). Refused unless the coverage claim accounts for every changed file                                                                                                                                                                                  |
| `--trusted-comments <owner/repo> <n> [--marker] [--issue]`                      | author-verified comment read — the only trusted way to read a comment                                                                                                                                                                                                                                                                                                                              |
| `--commit-closes <owner/repo> <n>`                                              | closing-keyword vs. `closingIssuesReferences` drift check                                                                                                                                                                                                                                                                                                                                          |
| `--backfill-comments`                                                           | one-time completion of the ledger→GitHub migration (replays each ledger verdict as its missing comment)                                                                                                                                                                                                                                                                                            |
| `gc-clones <work-dir>...`                                                       | reclaim merged/closed work-clones across one or more clone roots (state cleanup)                                                                                                                                                                                                                                                                                                                   |
| `unvetted [--json] [--include-skipped] [--limit n]`                             | the VETTER's state-load: which open PRs need a verdict this run, vet-first, with each one's signals (MCP always pages; the CLI is unbounded unless `--limit`). Also runs the `ai:blocked-on` CLEARANCE (#161): all typed deps merged/closed ⇒ clear + fresh re-vet; held and manual-review flags are reported, never vetted                                                                        |
| `already-fixed <owner/repo#n>...`                                               | has a MERGED PR referencing this issue landed SINCE it was filed? `uncovered-issues` cannot answer it: that split is computed from OPEN PRs only, so an issue fixed on `main` with no open PR reads as uncovered and gets worked. Takes ISSUE and PR refs (a PR's own closes). Exit 4 = merged-since, 1 = unreadable                                                                               |
| `state-load [--json] [--no-cache]`                                              | the PRODUCER's state-load: the fleet's `nextAction` histogram, the rows that name work, the approved set, and the audit backlog by severity — in ONE result, pre-grouped. The groupings are the ones the traces show runs actually asked for; the rest of the fleet is counted, not enumerated                                                                                                     |
| `await <owner/repo#n[@sha]>... [--timeout-secs n] [--interval-secs n] [--json]` | the producer's WAIT, as one turn: poll the whole in-flight set until every PR's checks have reported and (with `@<sha>`) a push has moved its head off `<sha>`, then report each subject. Run inside `Monitor`. An unmoved head is never settled by the old head's checks, and an empty rollup takes a bounded grace before it counts as no-CI. Exit 3 = deadline, 4 = a subject stayed unreadable |
| `preflight [--gh-auth] [--sol-shell]`                                           | the run's PRE-MODEL gate: every `HARNESS_TOOLS` binary resolves, plus each opt-in CAPABILITY (a `gh` authed with the scopes the pipeline writes through; a nix that can realise rainix's `sol-shell`). Unsatisfied ⇒ exit 12, one `ToolingFailure` row, no tokens spent                                                                                                                            |
| `unvetted_close_candidates` (MCP)                                               | the vetter's second state-load: which producer close-candidate flags need judging this run                                                                                                                                                                                                                                                                                                         |
| `record_close_candidate_verdict` (MCP)                                          | the vetter's issue write: uphold (queued for the human) or reject (strips the flag → producer's queue)                                                                                                                                                                                                                                                                                             |
| `human-rule <owner/repo> <n> <ruling> "<note>"`                                 | the HUMAN's PR ruling: `human:<ruling>` + a head-sha-pinned `👤 human` comment (supersedes any prior human ruling)                                                                                                                                                                                                                                                                                 |
| `human-rule-issue <owner/repo> <n> <ruling> "<note>"`                           | the HUMAN's issue ruling: adds `keep-open`; pinned to the live close-candidate flag, or to the issue as filed                                                                                                                                                                                                                                                                                      |
| `human-close <owner/repo> <n> "<note>"`                                         | the HUMAN's TERMINAL edge on either subject: rule `close-candidate`, retire the pending `ai:close-candidate`, close — ONE transition (#94)                                                                                                                                                                                                                                                         |
| `record-close-candidate-verdict <owner/repo> <n> <v> …`                         | the vetter's flag verdict, also as a subcommand — `human-rule-issue`'s stranded-flag refusal names it, and a terminal has no MCP                                                                                                                                                                                                                                                                   |
| `require-qa-block`                                                              | the QA-GUIDE §8 gate on PR-open: refuses a `gh pr create` whose body lacks the evidence block. Wired as a PreToolUse `Bash` hook, so it binds every session — including the ones with no MCP surface, which is the only population still reaching for `gh pr create` now `open_pr` exists                                                                                                          |
| `open_pr` (MCP)                                                                 | the PRODUCER'S OUTPUT EDGE: open the PR for a pushed branch, assigned, with a typed `closes` linkage and a body `carries_qa_block` has already accepted — and a RESULT carrying the PR number, so the trace holds `{agent, repo, issue, PR}` as typed data                                                                                                                                         |
| `push` (MCP)                                                                    | the PRODUCER'S REWORK EDGE: fast-forward a work clone's branch onto origin — no force spelling is expressible — and RECORD the PR whose head it moved, named only when an open PR on that branch is at exactly the commit just pushed                                                                                                                                                              |
| `work-tokens <metrics/runs.jsonl> [--json]`                                     | TOKENS TO LAND WORK: per-actor spend joined to the work items `open_pr` and `push` recorded, bucketed landed / delivered-awaiting-human / churn. Only churn is waste; an actor with no typed item is churn and nothing is inferred from a label or a branch name. The main loop carries items but no per-item cost                                                                                 |
| `repair-qa-block <owner/repo> <n> --block-file <path>`                          | the RETROFIT of the same rule on an ALREADY-open PR: appends the §8 block to the body, every other byte identical, validated with `require-qa-block`'s predicate                                                                                                                                                                                                                                   |
| `weaken-closes <owner/repo> <n> <issue>`                                        | the LINKAGE repair a linkage `reject` names: `Closes #issue` → `Refs #issue`, every other byte identical, `## QA` untouched, DIRECTION-LOCKED so it can only ever remove a closing reference                                                                                                                                                                                                       |
| `mcp [--profile vetter\|producer\|human]`                                       | serve a role's transitions over MCP (stdio) — the FSM as a tool surface, not as prose                                                                                                                                                                                                                                                                                                              |
| `plugin-version-lockstep [--root <dir>]`                                        | CI gate: every plugin `.claude-plugin/marketplace.json` lists resolves to a manifest of the same name carrying the same version                                                                                                                                                                                                                                                                    |
| `migrate-reject [--apply]`                                                      | the #133 one-shot: every open PR still carrying the RETIRED `human:reject` → `ai:reject`. A REPORT unless `--apply` — an org-wide relabel is not one forgotten flag away                                                                                                                                                                                                                           |

## The layer a human types: slash commands as a plugin

The transitions above are what a tool call reaches. What a HUMAN types is a
Claude Code plugin published from this repo's own marketplace
(`.claude-plugin/marketplace.json` → `./plugins/human-fsm`), installed with
`/plugin marketplace add rainlanguage/issue-pr-cron`. The org already
distributes assets this way (`claude-audit-skills`, `adversarial-mutation-test`,
`rain-org-health`); `rain-org-health` is the shape followed, publishing from a
subdirectory so a repo that is primarily something else need not pretend to be a
skills repo.

**The commands are prompts and nothing else.** Every guard lives in the binary;
a command that re-derived a transition, or reached for `gh` to do what a
subcommand already does, is a loose transition by another name. A test reads
each shipped command's fenced blocks and requires every runnable line to be a
`pr-review-report` invocation — asserted on the fenced blocks specifically,
because the prose says `gh issue close` in order to FORBID it and a substring
scan cannot tell a prohibition from an instruction.

A plugin's version is stored TWICE (the marketplace listing installers read, and
the manifest it points at) and `/plugin` compares version strings, so a stale
listing serves stale content silently. `plugin-version-lockstep` makes agreement
a gate; it is a subcommand rather than a CI shell script for the same reason
`require-qa-block` is — everything it does is parsing.

## The FSM as a tool surface (MCP)

`pr-review-report mcp` speaks MCP over stdio. `--profile` picks the role, and a
profile is a **surface** filter, not a permission: `tools/list` returns only
that role's tools, so neither role pays preamble for the other's schemas and
neither can name the other's transitions.

| Profile            | Tools                                                                                                                                                                                                                                                                                                                         |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `vetter` (default) | PRs: `unvetted`, `pr_context`, `pr_checkout`, `record_verdict`, `clone_release`. Close-candidate flags: `unvetted_close_candidates`, `close_candidate_context`, `record_close_candidate_verdict`                                                                                                                              |
| `producer`         | `clone_create`, `clone_release`, `clone_list`, `clone_gc`, `push`, `open_pr`, `repair_qa_block`, `weaken_closes` — the clone lifecycle, the two OUTPUT edges (a moved head, a new PR), plus the two body repairs                                                                                                              |
| `human`            | `next_ready`, `pr_context`, `pr_checkout`, `clone_release`, `next_close_candidate`, `close_candidate_context`, `human_rule`, `human_rule_issue`, `human_close` — find the subject, read it, audit its source, rule on it. TWO inboxes means two "which is next" reads: `next_ready` for PRs, `next_close_candidate` for flags |

The vetter has **two subjects**, not one. A PR is judged on its diff; a producer
`ai:close-candidate` flag is judged on its evidence — and the second matters
because a wrong flag asks a human to destroy work. Both follow the same three
moves (state-load → read one → record one verdict) and the same
vetted-at-the-thing-judged rule: a PR is un-vetted again when its head moves, a
flag when the producer posts a new one. The vetter never writes a `human:*`
label in either subject; `uphold` leaves the flag for the human, `reject` strips
`ai:close-candidate` so the issue returns to the producer's queue.

The vetter profile is the vetter's **only** tool surface: `review-run.sh` always
passes `--mcp-config review-mcp.json --strict-mcp-config` with
`review-settings.json`, so the vetter has **no Bash at all** — the tools are
`mcp__fsm__*` and a non-FSM operation is unrepresentable rather than merely
denied (a Bash deny-list is prefix-matched and bypassable). There is no non-MCP
vetter prompt or settings file, and no flag that selects one. The transition
guards — verdict vocabulary, mandatory in-range cost, well-formed PR ref,
human-sacred refusal — live in `validate_call` / `verdict_plan`, tested once,
instead of being re-asserted in prose.

The vetter's surface **replaces** its Bash, so it is `--strict-mcp-config` and
there is no non-MCP prompt or settings file to fall back to. The producer's
server (`campaign-mcp.json`) is **additive** — no `--strict-mcp-config`, it
keeps its Bash — because what it gains is a clone lifecycle it could not
previously perform at all. Neither is selectable at run time.

The vetter's surface is also deliberately **read-only on the filesystem**: it
reads the `pr_checkout` clone, it never builds or runs anything in it.
Clean-by-construction work clones are the producer's obligation
(`campaign-prompt.txt` step 6b) and, for rainix Solidity repos, the
`rainix-copy-artifacts` workflow's `git diff --exit-code`; re-running a PR's
tests is CI's job. The vetter's QA gate checks that the evidence block exists
and holds against the diff it reads, nothing more.

## Work-clone lifecycle

A work clone is created and destroyed through **tools**, never through shell.
`clone_create` clones or re-syncs `<root>/<name>`; `clone_release` disposes of
one; `clone_gc` is the end-of-run backstop sweep; `clone_list` reports what is
on the box. The roots come from the environment (`WORK_DIR`, plus `INSTALL_DIR`
because stranded `vet-*` clones live there) and **never** from a tool argument —
a model-supplied root would make every guard vacuous.

Why a tool: `campaign-settings.json` denies `Bash(rm -rf /:*)`, deny rules are
**prefix-matched**, and so it also denied `rm -rf $WORK_DIR/<clone>` — the exact
deletion `campaign-prompt.txt` mandated. The instruction was impossible to
follow for months and the box grew to 195 GB of clones (#56). Widening the rule
would fix that instance and keep the shape of the problem; moving the delete
behind a tool means "remove something outside the work roots" is not
expressible.

The path guards, in `clone_name_in_root` + `resolve_existing_clone`:

- exactly **one path component** directly under a configured root — a bare name
  or the full path of a direct child, nothing else;
- **no `..`** in any position, checked before any prefix arithmetic;
- **no absolute path outside the root**, including the sibling-prefix trick
  (`/home/gildlab/codeEVIL` shares a string prefix with `/home/gildlab/code` —
  the same class of bug as the deny rule itself);
- **never the root itself**, an ancestor of it, or a `.`-prefixed entry;
- **never a symlink**, and the canonical path must still be a direct child, so a
  symlinked component cannot smuggle the target elsewhere;
- **must contain `.git`** — only a git work clone is ever deletable, so no
  malformed argument reaches ordinary data.

And the release decision, in `release_decision` (shared with the sweep, so the
attended release and the unattended sweep never disagree about whether a clone
still holds work):

- commits that exist **only** in the clone refuse **unconditionally** — there is
  no override flag, because a flag is a thing a model under time pressure sets;
- an unknown push state is treated as unpushed (fail safe) — except an **unborn
  HEAD**, which is not unknown: a clone with no commits has nothing to lose, and
  reading it as unknown made every interrupted clone immortal;
- uncommitted changes refuse too, but `discard_uncommitted: true` overrides,
  because in practice that dirt is build output and refusing it outright is what
  leaves the clone on disk forever.

One rule the unattended sweep does **not** share with release: an **audit-lens
checkout** (`vet-<repo>-<n>`, made by `pr_checkout`) is disposable on **age
alone** — one day, ignoring its PR state. The vetter checks out the PR it is
JUDGING, so that PR is always OPEN, and "open PR → active work" made every
leaked checkout immortal: 83 of them, 349 MB, under a sweep that had been
running nightly the whole time (#81). The dirt/unpushed guards still run first.
The sweep is also the ONLY thing that reclaims one — a run that dies is exactly
the run that leaks, so an end-of-run `clone_release` cannot be the mechanism —
which means the midnight `gc` line must name **every** clone root (`WORK_DIR`
_and_ the install dir), not just the first.

**One result budget, and it must be under the harness's ceiling.** Every tool
result is checked against the same 36,000 bytes — `pr_context` included, which
used to get `max_diff_bytes + 32,000` (up to 332,000, about six times what the
harness accepts, so its guard never fired). Ordering is the mechanism: if the
harness speaks first the caller gets an untyped message with `is_error`
**unset**, and "a tool error is an instruction" stops applying exactly when it
is needed. The ceiling is measured against the running harness, never derived by
halving a payload that was refused; 2.1.220 has TWO untyped gates (a byte gate
around 50,011–50,176 bytes, not governed by `MAX_MCP_OUTPUT_TOKENS`, and a token
gate governed by it) and the budget sits ~28% under both. One budget for every
tool is also what makes narrowing CONVERGE — while the allowance scaled with
`max_diff_bytes`, lowering the argument lowered both sides equally. `pr_context`
fits itself to the budget rather than waiting to be refused, and reports
`diffBytes` / `diffIncluded` / `diffTruncated` so the shortfall is visible.

`pr_checkout` itself holds a binary postcondition: **the PR head at `dir`, or no
`dir`**. It fetches `refs/pull/<n>/head` into `refs/remotes/origin/pr/<n>`
(works on a shallow clone, works for forks, keeps the head provably pushed),
returns the `dir` and the `head` sha, and deletes what it made if any step
fails. Nothing downstream may search the filesystem for a checkout: the leftover
it finds is a different PR's code.

## Invariants

- **Human decisions are sacred.** A `human:*` label, a native `APPROVED` /
  `CHANGES_REQUESTED` review, OR a `👤 human` ruling comment pinned to the
  CURRENT head is never overwritten by the vetter — `--record-verdict` refuses
  (exit 3), closing the TOCTOU race.
- **A reject is ONE state, and the ruler rides on the comment (#133).**
  `ai:reject` and `human:reject` demanded the same move from the same actor, so
  they are one state: `ai:reject`, whoever ruled. The label says what the work
  is; the **sha-pinned `👤 human` comment** says who said so, and that is where
  the authority lives. The vetter cannot forge one: `trusted_comments`
  authenticates by AUTHOR and matches the marker with `starts_with`, and every
  comment the vetter can post begins `🤖 ai:vetter` — a marker in the middle of
  a vetter note is body text. **The anchor is also the release.** A rework moves
  the head, the ruling stops describing the code, and the PR re-enters vetting
  through the ordinary un-vetted path with the ruling in
  `pr_context.humanComments`. `reworked-reject` is retired: its timestamp
  comparison proved only that SOME commit post-dated the label event, and what
  actually protects the objection is a stateless re-vet that can read it. So
  `human:*` now means one thing absolutely — sacred, never written or cleared by
  an AI actor — with no carve-out. `human:reject` survives only as a RETIRED
  label on PRs the migration has not moved; it stays sacred and stays bucketed
  until `migrate-reject` does.
- **A verdict accounts for every file the PR changes.** Scope coverage was the
  one thing `record_verdict` took on trust, and a verdict formed without a
  changed file in view is indistinguishable from a diligent one:
  `rain.erc4626.words#230` recorded `ready` while the PR was in the act of
  ADDING a file whose `address private immutable _ext;` breaks the audit skill's
  Solidity naming rule twice. So the claim is an ARGUMENT the tool checks, not a
  rule the prompt states. `covered` (MCP) / `--covered-file` (CLI) names each
  changed file; each **hand-written** one also carries an ANCHOR — a new-side
  diff line number plus its content — verified against the PR's own diff, which
  is the same move `Reviewed <sha>:` makes for the head one level up. Generated,
  vendored, lockfile and binary paths need the name alone, and so does any file
  the diff shows **no new-side line** for, because an anchor that cannot exist
  is a gate nothing can pass. The refusal (exit 4) reports **every** unmet entry
  at once and prints the line ranges that would satisfy it: the vetter cannot
  escalate to a human the way a producer can, so a correct verdict must never be
  more than ONE corrected call from being recorded. A PR that changes files
  whose diff carries not one `diff --git` header is refused outright: a claim
  checked against a diff that is not there is not checked, and a guard that
  silently stops firing is this very failure one level up, inside the thing
  built to prevent it.
- **The human's TERMINAL edge is a transition too.** `gh issue close` knows
  nothing about the FSM, so a hand-close left `ai:close-candidate` attached: 74
  closed subjects org-wide (55 issues, 19 PRs) carried it when #94 was filed, a
  state no modeled transition produces. `human-close` is that edge as ONE
  transition — comment, `human:close-candidate`, retire the pending flag, close,
  in that order, with the close LAST because a closed subject reads as moot to
  every ruling plan. It retires `ai:close-candidate` and no other `ai:*` label:
  that one means "a human still has to ACT", and closing is the act. Chaining a
  ruling tool with a Bash `gh close` would put the order and the flag clear in a
  prompt, which is exactly the half that was wrong every time.
- **A ruling names its POPULATION.** `ai:close-candidate` covers two separately
  sized sets — `closeCandidateIssues` (a producer claim on an issue) and
  `lanes.vetter-verdicts.ai:close-candidate` (the vetter's own verdict on a PR).
  Every human transition takes a full `owner/repo#n`; where it can act on either
  it READS the subject's `url` rather than trusting which command was typed, and
  where it cannot it refuses by naming what was referenced and printing the
  command that fits — in both directions.
- **A human ruling is a transition too, and it pins to what it ruled on.**
  `human-rule` / `human-rule-issue` are the only sanctioned way to write a
  `human:*` label: raw `gh issue edit --add-label` binds to nothing and, on an
  issue whose close-candidate flag has not been judged, permanently strands it
  (every AI transition refuses once a human has ruled). A ruling pins to the
  head sha, the live flag's timestamp, or the issue as filed — whichever anchor
  the AI's ruling on that same subject uses — so the two stale together.
- **Comments are trusted by AUTHOR, never by marker text.** Any third party can
  post a `🤖 ai:vetter` / `🤖 ai:producer` / "Rework note" line; a comment
  counts only when the trusted account authored it. Read via
  `--trusted-comments`.
- **Vetting is a pure function of the PR at its head, and `vetted_at_head` is
  its CACHE KEY.** A prior verdict is not an input to vetting, so there is no
  second kind of pass and no "awaiting re-vet" state — a PR is either vetted or
  **un-vetted**. A stored verdict may stand in for recomputing one only while
  both halves of the key hold: the trusted `🤖 ai:vetter` comment pins the
  **current head** (the input) **and** carries the **current `vet-protocol`
  stamp** (the function). A push invalidates the first, bumping `VET_PROTOCOL`
  invalidates the second for every verdict at once, and an unstamped comment is
  `VetProtocol::Unknown` — never current, because rules that cannot be
  identified are not the rules in force (`Merge::Unknown`,
  `CodeRabbitCoverage::Unreadable`). Bump the protocol when what vetting MEANS
  changes; nothing has to be pushed, relabelled or rewritten for the pipeline to
  recompute.
- **Landing is interactive-only** (the merge cron is retired):
  `gh pr merge --merge --admin` on the human's explicit per-PR word, after the
  SHA-bound review gate.
