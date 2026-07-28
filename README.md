# rainlanguage org issue→PR pipeline

Local, autonomous cron jobs that drive issues to merge-ready PRs across the
rainlanguage **and cyclofinance** GitHub orgs. The pipeline is a **finite state
machine**: a PR's state _is_ its GitHub state — `ai:*` / `human:*` labels,
trusted `🤖 ai:vetter` comments, and its native review — and the
`pr-review-report` tool is the **only** transition function between states. A
**producer** cron and a **vetter** cron drive the automated transitions; landing
is interactive (a human merges on their explicit per-PR word). See
[CLAUDE.md](CLAUDE.md) for the framing.

## Pipeline state machine

```mermaid
stateDiagram-v2
    direction LR
    state "open issue" as issue
    state "ai:close-candidate (issue)" as icand
    state "close-candidate · upheld" as iupheld
    state "un-vetted PR" as unvetted
    state "awaiting re-vet" as revet
    state "ai:ready" as ready
    state "ai:reject" as reject
    state "ai:relink" as relink
    state "ai:design" as design
    state "ai:close-candidate (PR)" as close
    state "ai:blocked-deploy" as bdeploy
    state "ai:blocked-infra" as binfra
    state "ai:blocked-on" as bon
    state "human:reject" as hreject
    state "human:design" as hdesign
    state "human:close-candidate" as hclose
    state "presentable · in queue" as queue
    state "approved · human review" as approved
    state "merged" as merged

    [*] --> issue
    issue --> unvetted : producer opens PR

    %% issue close-candidate lifecycle — the vetter's SECOND subject. A flag is a CLAIM, and it is
    %% vetted before a human is asked to act on it (a bad flag asks a human to destroy work).
    issue --> icand : producer flag-close-candidate
    icand --> iupheld : vetter uphold · evidence holds
    icand --> issue : vetter reject · strips the flag → back to uncovered
    icand --> icand : producer re-flags (new evidence) → un-vetted → re-vet
    iupheld --> [*] : human closes
    icand --> hclose : human close-candidate (sacred)
    icand --> hreject : human reject (sacred)

    %% vet lifecycle — the vetter is the sole verdict transition fn
    unvetted --> ready : vetter record-verdict
    unvetted --> reject : vetter record-verdict
    unvetted --> relink : vetter record-verdict
    unvetted --> design : vetter record-verdict
    unvetted --> close : vetter record-verdict
    ready --> revet : head moves (producer fix)
    revet --> ready : vetter re-vets
    revet --> reject : vetter re-vets

    %% ready → the human merge queue
    ready --> queue : queue · green·mergeable·vetted@head
    queue --> approved : human review = APPROVED
    approved --> merged : gh pr merge --admin · human word

    %% vetter verdicts route back to the producer, then re-vet
    reject --> unvetted : producer reworks → head moves → re-vet
    relink --> unvetted : producer relinks Closes→Refs → re-vet

    %% producer deploy + blocked hand-offs → human resolves → re-work
    ready --> ready : producer deploy · red prod-pin → green
    ready --> bdeploy : flag-blocked-deploy · deploy FAILED
    unvetted --> binfra : flag-blocked-infra · infra/tooling gap OR can't classify
    unvetted --> bon : flag-blocked-on · waiting on a dependency PR
    bdeploy --> unvetted : human resolves deploy → re-work
    binfra --> unvetted : human clears infra / models a new state → re-work
    bon --> unvetted : dependency merges → producer re-works

    %% human decisions are sacred — the vetter never re-verdicts these
    ready --> hreject : human reject + Rework note
    ready --> hdesign : human design ruling
    ready --> hclose : human close-candidate
    hreject --> unvetted : producer reworks → reworked-reject clears labels → re-vet
    hdesign --> [*] : human rules
    hclose --> [*] : human closes

    design --> [*] : human design ruling
    close --> [*] : human closes
    merged --> [*]
```

Every transition above is a `pr-review-report` subcommand. A raw `gh` / `git`
state change from a prompt is a _loose_ transition — unenforced and untested —
so the prompts route **all** GitHub I/O through the tool. That is what makes
this an actual finite state machine rather than a picture of one.

### The vetter's transitions as an MCP surface

For the producer, routing through the tool is enforced by the **prompt**, and a
Bash deny-list is prefix-matched and bypassable. For the vetter that gap is
closed: `pr-review-report mcp` serves its transitions over MCP (stdio), and that
server is the vetter's **only** tool surface.

| Tool                             | The move it makes                                                                                                                                                                           |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `unvetted`                       | state-load: ONE PAGE of the open PRs to vet, vet-first, each with head/labels/review/sacred/vetted/ci/mergeable, plus the whole-queue `counts`, `more`, and the `openThreads` withhold list |
| `pr_context`                     | read one PR: body, files, diff, every linked issue, and the trusted `🤖 ai:*` comments — one call                                                                                           |
| `pr_checkout`                    | local read-only clone of the PR head, so the `audit` skill has source — returns the `dir` AND the `head` sha it produced, or errors having left nothing behind                              |
| `record_verdict`                 | the PR write: `ai:<verdict>` label + sha-bound `🤖 ai:vetter` comment + cost                                                                                                                |
| `clone_release`                  | dispose of a checkout it is finished with (guarded — see below)                                                                                                                             |
| `unvetted_close_candidates`      | state-load: ONE PAGE of the producer close-candidate flags to judge, each with its `flagAt` + stated evidence                                                                               |
| `close_candidate_context`        | read one flag: the issue's title/body/`createdAt`/labels plus the full flag body and any prior verdicts                                                                                     |
| `record_close_candidate_verdict` | the issue write: `uphold` (flag stands, queued for the human) or `reject` (strips `ai:close-candidate`)                                                                                     |

The last three are the vetter's **second subject**. A PR asks a human to merge
code; a close-candidate flag asks a human to **destroy work**, so the flag is
judged before it reaches the triage queue. The shape is identical to the PR side
— state-load, read one, record one verdict — including the
vetted-at-the-thing-judged rule: a PR re-vets when its head moves, a flag
re-vets when the producer posts a new one.

`review-run.sh` always launches the model with `--mcp-config review-mcp.json`,
`--strict-mcp-config` and `--settings review-settings.json`, so the vetter's
entire tool surface is
`mcp__fsm__{unvetted,pr_context,pr_checkout,record_verdict,clone_release,unvetted_close_candidates,close_candidate_context,record_close_candidate_verdict}`
plus `Read`/`Grep`/`Glob`/`Skill`/`ToolSearch` — **no Bash**, so there is no raw
`gh` or `git` to reach for. There is no second vetter configuration and no flag
that selects one. The guards (verdict vocabulary, a mandatory 0-1000 cost, a
well-formed `owner/repo#n`, the human-sacred refusal) are enforced in the server
and unit-tested rather than restated in the prompt.

Those thirteen schemas are **presented, not deferred**: `review-run.sh` exports
`ENABLE_TOOL_SEARCH=false`, so the surface rides in the preamble instead of
costing the vetter a first-turn `ToolSearch` to rediscover a fixed allowlist by
name. `ToolSearch` nonetheless stays in the allow-list as the fail-safe — if a
harness defers anyway, a vetter that cannot call it sees its own tools as
nonexistent and records nothing at all (#63). The producer keeps deferral: it
has Bash and a far larger surface, where the round trip pays for itself.

### Every tool result is bounded, and going over is the tool's error

A state-load is a **page**, not a dump. `unvetted` and
`unvetted_close_candidates` return at most `limit` rows (default 10, max 25)
with the whole-queue `counts` alongside and `more` naming what the page left
behind; the vetter re-calls for the next page, and because each `record_verdict`
removes its subject from the queue, paging converges without an offset argument.
The page size is what makes the bound structural — the payload no longer grows
with the number of open PRs.

Every result is then checked against **one byte budget, the same for every
tool** (36,000 bytes), and a result over budget is returned as a **tool error
naming the argument to narrow** — never truncated, never spilled. On 2026-07-27
`unvetted {"include_skipped": true}` returned 63,742 characters on one line, the
harness refused it, and the vetter improvised a fallback that silently dropped
the whole open-threads accounting; the run log looked normal. A partial
state-load cannot say what it is missing, so the tool refuses to produce one.

**The budget must be lower than what the harness accepts, and that is the
mechanism, not a preference.** If the harness is the thing that speaks, what
comes back is untyped and arrives with `is_error` **unset** — so every rule
downstream about "a tool error is an instruction" stops applying at the moment
it is needed. `pr_context` used to be budgeted at `max_diff_bytes + 32,000`, up
to **332,000 bytes**, roughly six times what the harness accepts; its guard
could not fire, and the harness's message arrived instead. The value is now
measured rather than derived from halving a payload that had already been
refused — see [the ceiling, measured](#the-ceiling-measured).

Two consequences follow from one budget for every tool. `max_diff_bytes` can no
longer be raised past it, so a `pr_context` cannot buy itself more room than any
other tool gets. And **narrowing converges**: while the budget scaled with
`max_diff_bytes` and the diff was truncated to `max_diff_bytes`, lowering the
argument lowered allowance and payload equally, so "re-call NARROWER" was a loop
with no exit. Against a fixed allowance a smaller argument is a strictly smaller
result.

`pr_context` does not wait to be refused: it **fits itself** to the budget,
shrinking the diff until the document lands under it, and reports `diffBytes`
(the whole diff), `diffIncluded` (what actually made it in) and `diffTruncated`
so the gap between what exists and what was handed over is visible rather than
inferred. The shrink terminates — each round removes at least the overflow from
the cap, and one raw byte of diff is at least one byte of document — and the one
case no argument can fix, metadata alone over the budget, is a typed error that
says exactly that.

#### The ceiling, measured

Against Claude Code 2.1.220, by calling `pr_context` through the real harness at
increasing `max_diff_bytes` and reading the `tool_result` the model actually
received. There are **two** independent gates and **both** arrive with
`is_error` unset:

| gate  | what the model gets                                             | boundary                                                      |
| ----- | --------------------------------------------------------------- | ------------------------------------------------------------- |
| byte  | `<persisted-output> Output too large (NN KB)` + a 2 KB preview  | delivered at 50,011 bytes, replaced at 50,176                 |
| token | `Error: result (N characters …) exceeds maximum allowed tokens` | the gate the live traces hit, at 63,742 and 56,789 characters |

The byte gate is **not** governed by `MAX_MCP_OUTPUT_TOKENS` — forcing that to
200,000 still replaced a 50,486-byte result — and it is the more dangerous of
the two, because the 2 KB preview it substitutes looks like the head of a real
answer. The token gate is: forcing the variable to 100 replaced a 4.5 KB result.
Isolating it at `MAX_MCP_OUTPUT_TOKENS=10000` puts its boundary between 27,152
and 30,163 bytes, so this JSON measures **2.7–3.0 chars/token**; nothing on the
box sets that variable, and 56,789 characters tripped it live, which puts the
default near 19–21k tokens. Both gates therefore land around 50 kB for this
content.

36,000 sits ~28% under both. The margin is not timidity: the token gate scales
with the **content**, and a diff of generated hex — which this org has, in every
`src/generated/*.pointers.sol` — tokenises far worse than prose. At 36,000 bytes
even a payload tokenising at 1.5 chars/token stays inside a 19k-token cap.

The `openThreads` list is unconditional for the same reason: the PRs withheld
for unresolved threads (and their `unresolvedThreads` counts) are the only
skipped rows carrying information the vetter can act on, and making that
accounting depend on an optional argument is exactly how it went missing.

### The audit lens's working tree: the PR head, or nothing

`pr_checkout` shallow-clones the repo and fetches **`refs/pull/<n>/head`** into
`refs/remotes/origin/pr/<n>`, then checks that out as `pr-<n>`. Three properties
follow, and each fixes a distinct failure:

- **It works on a shallow clone.** `gh pr checkout` does not: a `--depth 1`
  clone's fetch refspec is
  `+refs/heads/<default>:refs/remotes/origin/<default>`, so a same-repo PR's
  head arrives as a bare fetched ref and `git checkout --track` refuses it —
  _"cannot set up tracking information; starting point 'origin/<head>' is not a
  branch"_ — for **every** same-repo PR, i.e. every PR that needed the audit
  lens (#81).
- **It stays shallow.** The obvious repair — widen the refspec and keep
  `gh pr checkout` — makes the follow-up fetch deepen the clone to nearly full
  history. Measured on raindex: **156 MiB** of pack against **3.9 MiB** for the
  pull-ref fetch, with `--depth 1 --no-single-branch` at 78 MiB and a full clone
  at 180 MiB. Disk-full silently killed both crons for ~17h (#56), so the fix
  that costs nothing over the shallow clone already intended is the only one
  that is not a trade.
- **The head is on a remote-tracking ref**, which is what makes the commit count
  as pushed. `gh pr checkout` puts a **fork** PR's head on a plain local branch,
  so `rev-list HEAD --not --remotes` reports the whole branch as unpushed and
  both `clone_release` and the sweep refuse the clone forever.

**Its postcondition is binary: the PR head at `dir`, or no `dir` at all.** The
old code returned an error while leaving the directory behind, because
`gh repo clone` had succeeded and only the checkout failed — so a directory
named after the PR sat at exactly the path the audit lens looks for, holding the
**default branch**. On 2026-07-27 the vetter met that failure, went looking for
a tree, found the leftover `vet-rain.factory-dep` from an unrelated run, and
began enumerating its Solidity sources as `rain.factory#47`'s. The chain is
broken in four places: the checkout works; a failed one deletes what it made;
the failure is an `isError` that names the wrong answer a filesystem search
returns and forbids it; and the sweep reclaims the leftovers a search would
otherwise find. The success value carries the `dir` **and** the `head` sha, so
locating (or recognising) the tool's own output is not a step that exists.

**What the vetter therefore does not verify.** With no Bash it cannot build, and
cannot execute anything in the clone `pr_checkout` gives it — it reads source,
it does not run it. Two checks live elsewhere as a result, and the vetter prompt
does not ask for either:

- **A clean working tree after a build.** Keeping a work clone clean by
  construction is the **producer's** obligation (`campaign-prompt.txt` step 6b:
  `git status --porcelain` must be empty before the work counts as submitted,
  and build/tooling dirt is gitignored as part of the PR). For rainix Solidity
  repos the committed-artifact half is additionally enforced on every push by
  the shared `rainix-copy-artifacts` workflow, which regenerates, builds, and
  fails on `git diff --exit-code`.
- **Re-running a PR's tests.** The QA gate checks that the QA-GUIDE.md section-8
  evidence block **exists** and that its claims are consistent with the diff it
  reads. It does not re-run the named tests against base; CI runs them, and a
  red CI is the producer's to green, never a vetter `reject` ground.

### Work-clone lifecycle as an MCP surface (always on)

`pr-review-report mcp --profile producer` serves the **producer's** clone
lifecycle: `clone_create`, `clone_release`, `clone_list`, `clone_gc`. Unlike the
vetter's surface this one is **additive** — the producer keeps its Bash, and is
wired unconditionally (`--mcp-config campaign-mcp.json`, no
`--strict-mcp-config`), because what it gains is an operation it could not
previously perform at all:

`campaign-settings.json` denies `Bash(rm -rf /:*)`. Deny rules are
**prefix-matched**, so that also denied `rm -rf /home/gildlab/code/<clone>` —
every work-clone path — while `campaign-prompt.txt` mandated exactly that
deletion the moment a PR was pushed. The two contradicted each other for months;
the clone directory grew to **195 GB** and disk-full is the documented cause of
the producer's silent-death failure mode (#56).

The fix is not a wider deny rule (that fixes the instance and keeps the shape).
It is that **the model no longer supplies a path to delete** — it names a clone,
and the name is resolved in Rust before any syscall:

- exactly one path component directly under a configured root (`WORK_DIR`, plus
  `INSTALL_DIR`, where the vetter's `vet-*` clones were stranded); roots come
  from the environment, never from a tool argument;
- no `..` anywhere, no absolute path outside the root (including the
  sibling-prefix trick that fooled the deny rule), never the root itself or an
  ancestor, never a `.`-prefixed entry, never a symlink;
- the target must contain `.git` — **only a git work clone is ever deletable**,
  so no malformed argument can reach ordinary data.

Release refuses **unconditionally** on commits that exist only in the clone (or
an unknown push state); uncommitted changes refuse too, overridable with
`discard_uncommitted` once the caller has confirmed the dirt is build output.
`clone_gc` remains the unattended backstop with the old, deliberately
conservative rule — it deletes only what it can prove is finished.

**Audit-lens checkouts are the one exception, and they had to be.** A `vet-*`
clone is the PR the vetter is **judging**, so its PR is always OPEN, and "open
PR → active work" made every leaked checkout immortal: 83 of them, 349 MB, under
a sweep that had been running nightly the whole time (#81). They are now
disposable on **age alone** — one day, ~12× the vetter's own 2h `REVIEW_MAXTIME`
ceiling, independent of `--max-age-days`. The dirt/unpushed guards still run
first, and "idle" is read as the newer of the clone directory's mtime and
`.git/HEAD`'s — a checkout rewrites files below the top level, so the
directory's own mtime does not move, and a clone the vetter checked out minutes
ago would otherwise read as days idle and be deleted underneath a run still
using it. So "never delete something that holds work" is unchanged; what changed
is that a read-only copy of a commit already on GitHub stopped being treated as
work. This sweep is the **only** thing that reclaims a leaked checkout, and it
has to be: a run that dies is exactly the run that leaks, so a `clone_release`
on the way out can never be the mechanism.

The machine has **no dead-ends**: every state has an exit back into the
lifecycle or to a terminal (`merged` / a human ruling). The vet lifecycle
(`un-vetted → vetting → awaiting re-vet`) re-runs the vetter whenever a PR's
head moves, so a reworked PR is always re-judged against its current code. The
**human reject is TRANSIENT**, not terminal: when a human applies `human:reject`
and a trusted "Rework note", the producer executes the rework, pushes a fix
commit, and then calls **`pr-review-report reworked-reject <owner/repo> <n>`**
as its final step. That subcommand REMOVES `human:reject` **and any stale `ai:*`
verdict** (the code changed → re-vet from scratch), returning the PR to
ready-to-vet so it re-enters the normal vet → queue → human loop. It is guarded:
it clears `human:reject` **only** when the PR head commit provably
**post-dates** the `human:reject` label event (the one sanctioned carve-out from
"never remove a `human:*` label"); a head that does not post-date the reject is
refused, so a still-standing human reject is never silently undone.

`human-queue --json` emits the **full** inventory — every modeled state's PRs,
grouped into four lanes so the dashboard can show where PRs pile up:

- **vet-lifecycle** — `un-vetted` (open PRs awaiting a first verdict) and
  `awaiting-re-vet` (an `ai:ready` PR whose head moved past its last vetter
  verdict).
- **vetter-verdicts** — `ai:ready`, `ai:reject`, `ai:relink`, `ai:design`,
  `ai:close-candidate`.
- **producer-blocked** — `ai:blocked-deploy`, `ai:blocked-infra`,
  `ai:blocked-on`.
- **human-decisions** — `human:reject`, `human:design`, `human:close-candidate`.

Each PR is bucketed **once**, by FSM precedence (a human decision dominates a
stale `ai:*` label). The legacy `states` / `leaks` / `counts` keys are preserved
unchanged; `lanes` and the additive `counts` keys (`reject`, `relink`,
`closeCandidatePrs`, `humanReject`, `humanDesign`, `humanCloseCandidate`,
`unvetted`, `awaitingReVet`) are the full-machine view the dashboard renders.

The ISSUE close-candidate lifecycle carries two further additive counts, which
split the existing `closeCandidateIssues` (unchanged: every issue carrying the
label) by vet state:

- **`closeCandidateUnvetted`** — flagged, no `human:*` ruling, and no vetter
  verdict against the CURRENT flag. This is the vetter's inbox, and it is the
  same set `unvetted-close-candidates` returns, so the dashboard and the vetter
  cannot disagree about its size.
- **`closeCandidateUpheld`** — the vetter judged the evidence sound, so the flag
  genuinely awaits human triage.

A **rejected** flag needs no count: the vetter strips `ai:close-candidate`, so
the issue leaves this set entirely and reappears under `uncoveredIssues` — the
producer's queue — which is exactly the behaviour a rejection should have.

Both are emitted exactly as `closeCandidateIssues` and `uncoveredIssues` are:
the key appears **twice** — at the top level as the ITEM ARRAY (one
`{repo, number, title}` per issue) and under `counts` as its length. The
dashboard's state boxes are click-through, so a count without its array renders
a number that then lists nothing. Arrays and counts are derived from a single
document, so `counts.X == X.len()` holds by construction. These are ISSUE
states, so — like `closeCandidateIssues` — they are **not** in `lanes`, which
groups PRs.

The producer never narrates a hand-off in prose. Anything it cannot land is a
labeled transition into exactly one modeled state: `design`, `close-candidate`,
`blocked-deploy`, `blocked-infra`, or `blocked-on`. Those five plus `ready` (the
merge queue) are the **human-gated states** — the daily review queue, a plain
label search, no prose scraping. `blocked-infra` is the **total-function
fallback**: any situation the producer cannot classify into a state lands there
with a free-text reason, so it can never act _outside_ the machine. Reviewing
the `blocked-infra` queue is exactly where a human decides what needs to change
to move each item back into a well-defined state — fix the infra, model a new
state, or forbid the behavior; a recurring `blocked-infra` reason is the
evidence to promote it to a first-class state.

The three crons are **staggered by 2 h** so work flows downstream within each
4-hour cycle (all times UTC):

```
   :00  ✅ MERGE     lands the PRs you approved last cycle
   :01  🤖 PRODUCER  greens its own red PRs FIRST, then opens new fix PRs
   :03  🔍 VETTER    AI-reviews the fresh PRs → records verdicts
        👤  ……  you approve anytime  ·  pr-review-report.sh --ready
   :04  ✅ MERGE     (next cycle) lands what you just approved … ⟳

   6 cycles/day. A PR opened at :01 is vetted by :03; once you approve,
   the next :00/:04/:08… merge run lands it — hours end-to-end, hands-off.
```

- **Producer** (`campaign-run.sh`, every 4h at :00 of 1,5,9,13,17,21 UTC) —
  opens drives its OWN red PRs green FIRST (existing in-flight work, non-force
  commits), THEN opens one fix PR per tractable, uncovered issue (audit-backlog
  first). Org-mutating actions: `gh pr create`, `gh pr comment` (screenshots),
  and non-force `git push` to its own PR branches. Never
  merges/closes/deploys/force-pushes. Skips issues with a `reject` verdict
  (parked for a human, so a rejected fix isn't re-attempted into dead PRs).
- **Vetter** (`review-run.sh`, every 4h at :00 of 3,7,11,15,19,23 UTC) —
  AI-reviews open PRs and records a verdict as an `ai:*` label plus a sha-bound
  comment. Approval is the human's gate.
- **You approve** — review with `pr-review-report.sh`; approval is a GitHub
  `APPROVED` review, and only approved PRs are mergeable.
- **Merge cron** — RETIRED. Landing is interactive-only: the human merges, or
  the interactive assistant merges on an explicit per-PR go-ahead. Noted here
  only so a reader of older docs is not left looking for it.

## Scope — read this first

**The org-mutating actions this routine takes are `gh pr create`,
`gh pr comment` (UI screenshots), and a non-force `git push` of fix commits to
its OWN open red PR branches (to drive them green).** It **never** merges,
deploys, force-pushes, or closes/edits/comments-on issues. If it believes an
issue should be closed (already fixed, invalid, duplicate) it records a
_close-candidate_ — it never acts on it. This is enforced two ways: the
permission deny-list in `campaign-settings.json` and the rules in
`campaign-prompt.txt` (step 7 / 7a).

That flag is then **vetted before a human sees it**. The producer is the party
with an incentive to believe its own evidence, so the vetter judges the claim
the same way it judges a PR: `uphold` leaves the flag queued for the human, and
`reject` strips `ai:close-candidate` and returns the issue to the producer's
uncovered queue. Only the human ever CLOSES an issue — but the queue they triage
has had its wrong flags filtered out first. Hand-triage of ~29 flags found
roughly one in three unsupported (#72), in three classes the vetter now checks
explicitly: evidence that predates the issue, evidence that is unreachable code,
and evidence that answers a narrower question than the issue asked.

## Files (tracked here)

| File                     | Purpose                                                                                                                                                                                                                                                                                                          |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `campaign-run.sh`        | Durable runner (built as the `campaign-run` flake package): `flock` single-run lock, `DISABLED` kill-switch, `timeout`, invokes `claude --print` with the prompt + settings, logs to `campaign.log` (+ per-run JSONL traces in `runs/`). Nix builds its PATH; it sets none itself.                               |
| `campaign-prompt.txt`    | The campaign instructions fed to the model.                                                                                                                                                                                                                                                                      |
| `campaign-settings.json` | Tool allow/deny list passed via `--settings` (the permission guardrails).                                                                                                                                                                                                                                        |
| `review-run.sh`          | Vetting runner (same hardened pattern as `campaign-run.sh`): vets open PRs on the MCP surface, logs to `review.log`. Its one GitHub write is `record_verdict`. Kill-switch `review-DISABLED`.                                                                                                                    |
| `review-prompt.txt`      | The AI-vetting instructions fed to the model: the judgement gates only — every `gh` recipe is a tool schema instead.                                                                                                                                                                                             |
| `review-settings.json`   | Tool allow/deny for the vetter: the five `mcp__fsm__*` tools + `Read`/`Glob`/`Grep`/`Skill`/`ToolSearch`, **Bash denied outright**.                                                                                                                                                                              |
| `review-mcp.json`        | The vetter's MCP config: one stdio server, `pr-review-report mcp`, named `fsm` (so its tools are `mcp__fsm__*`).                                                                                                                                                                                                 |
| `campaign-mcp.json`      | MCP config for the producer's clone-lifecycle surface: one stdio server, `pr-review-report mcp --profile producer`, named `fsm`. Additive — the producer keeps its Bash.                                                                                                                                         |
| `cron.env.example`       | Template for deployment-specific values (PR assignee, work dir, models, run caps). Copy to `cron.env` (gitignored) and edit.                                                                                                                                                                                     |
| `pr-review-report.sh`    | Thin wrapper (flake package `pr-review-report-sh`) over the binary. Reports every open PR by its pipeline stage (approved / AI-vetted / needs-producer-fix (red) / conflicting / relink / reject / close / unreviewed / pending / draft), reading `ai:*`/`human:*` labels + GitHub approvals, as clickable URLs. |
| `hooks/`                 | The two bash PreToolUse guards that close deny-list bypasses. See [PreToolUse guards](#pretooluse-guards--what-a-prompt-cannot-hold).                                                                                                                                                                            |

## PreToolUse guards — what a prompt cannot hold

A prompt is advice and a permission deny-list is prefix-matched, so some
invariants can only be held by a PreToolUse hook, which sees the actual tool
call. Three are wired that way. **Only two of them are scripts:**

| Guard                                | Holds                                                                                                                                                   |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pr-review-report require-qa-block`  | QA-GUIDE.md section 8 — a `gh pr create` whose body has no `## QA` section, or names fewer than all four evidence lines, is refused with what's missing |
| `hooks/block-nix-wrap-gh.sh`         | `nix shell/run nixpkgs#gh` re-wrapping, which makes a command start with `nix` and so slips the `Bash(gh …)` deny-list                                  |
| `hooks/block-cron-git-bypass.sh`     | `git -C <dir> reset --hard` / `git -C <dir> push --force`, the spellings that evade guards anchored on a bare `git reset` / `git push`                  |

The QA gate is a **subcommand**, per CLAUDE.md's north star: everything it does
is parsing — a shell word-splitter, a heading scanner, a distinct-line
assignment — which is the work this binary exists to own. Being in the binary
also means it ships in the flake closure and its tests run inside the nix build;
a script under `hooks/` cannot, because the derivation's fileset is the
manifests plus the crate, so a repo-root script is absent there and every test
driving one skipped. The other two are still bash and still untested — [#10](https://github.com/rainlanguage/issue-pr-cron/issues/10)
tracks giving them the same treatment.

Nothing here is **installed by the flake as a hook**: wire each into the box's
user `settings.json` as a PreToolUse `Bash` hook. The two scripts carry their
own `DEPLOY:` note; the subcommand is invoked directly, with no wrapper script
around it:

```jsonc
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          // The flake-built binary, invoked directly — no bash wrapper.
          // `nix profile install <install-dir>#pr-review-report` puts it on PATH;
          // otherwise use the absolute path from
          // `nix build --no-link --print-out-paths <install-dir>#pr-review-report`.
          { "type": "command", "command": "pr-review-report require-qa-block" },
          { "type": "command", "command": "<install-dir>/hooks/block-nix-wrap-gh.sh" },
          { "type": "command", "command": "<install-dir>/hooks/block-cron-git-bypass.sh" }
        ]
      }
    ]
  }
}
```

Each reads the hook payload on stdin and exits `0` to allow or `2` to block,
with the refusal on stderr — the stream Claude Code feeds back to the model, so
a refusal is a **failed tool call carrying the fix**, not advice printed beside
a PR that opened anyway.

The two scripts return early unless `RAINIX_CRON_HOOK=1` (the cron runners
export it), so interactive sessions are untouched. `require-qa-block`
deliberately does not: the producer cron was the population already honouring
section 8 — every PR body in `runs/` carries the block — and the five PRs the
vetter rejected for a missing block (#83) were opened while that cron was
`DISABLED`, by interactive sessions under the same bot account. A guard scoped
to the cron would have covered everything except the thing that failed, and for
the same reason this cannot be an MCP transition: a tool surface binds only a
session launched with it, while a PreToolUse hook binds every session on the
box.

Its behaviour is covered by `pr-review-report-rs/tests/require_qa_block.rs` — a
content invariant on a command line is all parsing, and parsing is not something
a static read of the source can judge.

## Configuration

Deployment-specific values are **not** committed. Copy `cron.env.example` to
`cron.env` (gitignored) and set at least `PR_ASSIGNEE` (the GitHub handle every
opened PR is assigned to). `WORK_DIR`, `MODEL`, `MAXTIME`, `KEEP_RUNS` have
defaults and may be overridden there. The runner takes its install dir from
`CRON_DIR` (falling back to the working directory) and gets its `PATH` from the
flake closure, so there are no machine paths in the repo; `campaign-prompt.txt`
uses `{{WORK_DIR}}` / `{{CLOSE_CANDIDATES}}` / `{{ASSIGNEE}}` placeholders that
the runner substitutes at run time.

## Reviewing the output — the merge pipeline

A PR moves through two distinct gates before it merges:

```
🟦 unreviewed  →  🤖 AI-vetted  →  ✅ you approve  →  merge
```

- **AI review** is the automated pass (the review campaign): it records a
  verdict as an `ai:*` label plus a sha-bound comment. An `ai:ready` verdict
  means "passed automated review" — it is **NOT** a human sign-off.
- **Human approval** is _your_ gate: a GitHub `APPROVED` review, or a `human:*`
  label. **Only an approved PR is "ready to merge"**, and the merge is only ever
  performed on your explicit go-ahead.

`./pr-review-report.sh` prints every open PR bucketed by where it sits in that
pipeline, all as clickable URLs: **✅ approved by you** (ready to merge) · **🤖
AI-vetted — awaiting your approval** · **🔴 needs a producer fix** (CI red — the
producer drives it green) · **🔧 AI-flagged: relink** · **❌ reject /
changes-requested** · **🗑️ close (dup/superseded)** · **🟦 not yet reviewed** ·
**⚠️ conflicting** (needs rebase) · **🟡 pending** · **📝 drafts** · plus the
issues the cron flagged `ai:close-candidate`. `--ready` prints only the
approved-by-you set.

### The open-threads gate

A PR does not reach **either** the vetter or the human while it carries
**unresolved review threads** (CodeRabbit's or a human's). Both gates read the
same typed GraphQL state — `reviewThreads { isResolved }`, paginated — and never
the prose of a review body: on 2026-07-27 CodeRabbit reported check `SUCCESS` on
rain-org-health#128 with four threads open, so neither the status check nor an
`Actionable comments posted: N` line is evidence of clean.

- `unvetted` (the vetter's state-load) withholds a thread-dirty PR: it is
  counted as `skipOpenThreads`, listed in `openThreads` with its thread count,
  and never handed over, so no `ai:ready` verdict can be recorded while a thread
  is open. The vetter itself has no `gh` — the exclusion has to happen in the
  tool that builds its list, and the accounting has to come back unconditionally
  or the vetter cannot tell a withheld PR from an absent one.
- `queue` (the human approval queue) withholds it a second time, counted as
  `open-threads`, in case a PR was vetted before a thread was opened.

Both fail **closed**: a thread state that cannot be read is not presented. In
`queue` it is reported as `fetch-error` — a transient API failure is visible
rather than silently read as clean (which would present a dirty PR) or as dirty
(which would blank the queue). Resolving the threads is the **producer's**
step-3e duty, and `worklist` routes the PR there as `nextAction:
coderabbit-3e`.

**There is no local review ledger.** Verdict state lives on GitHub as `ai:*` /
`human:*` labels plus sha-bound comments, so it survives a lost box, is visible
without shell access, and cannot drift from what the PR itself shows. To approve
a PR, approve it on GitHub. The report self-provisions `gh`+`jq` via nix and
reads `cron.env` for `ORG` / `ORGS` / `PR_ASSIGNEE`.

## Runtime state (NOT tracked — see `.gitignore`)

- `campaign.log` — distilled human-readable log (`tail -f` to watch).
- `runs/<ts>.jsonl` — full per-run stream-json traces (`KEEP_RUNS` most recent).
- Issue close-candidates are NOT a local file — the cron applies the
  `ai:close-candidate` label and never closes anything itself. The human triage
  view is `gh search issues --label ai:close-candidate`.
- `DISABLED` — presence pauses the cron (kill-switch).
- `campaign.lock` — flock file (prevents overlapping runs).

## Schedule & controls

- **crontab:** the runners are flake packages, so cron invokes them through nix
  rather than by script path. `CRON_DIR` names the install dir (where
  `cron.env`, the prompts, the logs and the ledgers live) — the script can no
  longer derive it from `$0`, which is now a read-only path in the nix store.

  ```cron
  0 1,5,9,13,17,21 * * * PATH=$HOME/.nix-profile/bin:/usr/bin:/bin CRON_DIR=<install-dir> nix run git+file://<install-dir>#campaign-run
  ```

  The `PATH=` prefix exists only so cron can find `nix` itself; everything the
  run then executes comes from the flake closure. Use the `git+file:` form, not
  `path:`: a `path:` ref copies the working directory verbatim into the store on
  every evaluation, and the install dir accumulates gitignored work clones and
  traces (~5GB against ~1MB of tracked files). CI asserts the two refs produce
  identical derivations, so the cheap one is always safe.

  The **disk sweep** gets its own line, at midnight — the one run-free gap,
  since every producer/vetter tick is on an odd hour. It must name **every**
  clone root: clones land in `WORK_DIR`, and `vet-*` checkouts also accumulate
  in the install dir, which a `WORK_DIR`-only sweep never looks at (that
  omission is where 83 leaked checkouts and 349 MB sat, #81).

  ```cron
  0 0 * * * PATH=$HOME/.nix-profile/bin:/usr/bin:/bin nix run git+file://<install-dir>#pr-review-report -- gc <work-dir> <install-dir> >> <install-dir>/gc.log 2>&1
  ```
- **Pause:** `touch DISABLED` · **Resume:** `rm DISABLED`
- **Watch:** `tail -f campaign.log` · **Run now:**
  `CRON_DIR=<install-dir> nix run git+file://<install-dir>#campaign-run`

## What a run does

1. Auth + toolchain check (`gh auth status`, nix `forge --version`); stop loudly
   if broken.
2. Enumerate open issues org-wide.
3. Cheaply dedup against open PRs (single `jq` pass; byte-grepping the PR JSON
   is forbidden).
4. For each tractable, genuinely-uncovered issue: clone, branch, implement a
   minimal fix with mutation-validated tests, build + test, open ONE PR per
   issue (`gh pr create --assignee $PR_ASSIGNEE`, body `Closes #N` / `Refs #N`).
   If already fixed on main → no PR, log a close-candidate.
5. UI PRs require a screenshot (headless chromium harness → `pr-screenshots`
   branch).
6. End with a summary: PRs opened, issues skipped, close-candidates logged.
