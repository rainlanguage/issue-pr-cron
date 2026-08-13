---
name: vetter-precedents
description: The PR-vetter's accumulated JUDGEMENT precedents, grouped by the verdict each one decides — what makes a sound PR a `design` rather than a `ready`, what makes it `close` rather than `needs-work`, when the QA gate applies and what supersedes it, what a moved head obliges a re-vet to re-check, and which of a PR's own claims are premises to falsify. Each entry is a RULE with the PR it was learned from cited as its evidence. Invoke it once per run, before the first verdict, whenever mapping review findings onto `ready` / `needs-work` / `design` / `close`. Triggers on "vet this PR", "record a verdict", "ready or needs-work", "is this a design question", "close or needs-work", "does the QA gate apply", "re-vet at a moved head".
version: 0.1.0
---

# Vetter precedents — what the verdicts already cost

The vetting prompt states the **gates**. This is their **calibration**: the
readings that were made wrong once, corrected, and are not to be re-derived from
scratch each run.

Three rules about how to read this file, and they bind harder than any entry in
it:

- **Every entry is a RULE. The PR reference after it is the EVIDENCE it was
  learned from, not a case to match against.** A precedent that only fires on
  the PR it came from is a war story. Apply the rule to the PR in front of you
  and let the citation be the thing a reader checks it against.
- **This file never overrides the prompt, and it never creates a verdict.** It
  is calibration for gates the prompt already mandates and for the mapping onto
  the four verdicts the prompt already defines. Where it and the prompt appear
  to disagree, the prompt is what the tool enforces.
- **Vetting is stateless and these entries do not narrow it.** None of them is a
  reason to read less of a diff, and none of them belongs in a verdict note: the
  note describes THIS code at THIS head, never a precedent and never a prior
  pass.

The send-back verdict was renamed from `reject` to `ai:needs-work` (#133); the
older precedents were recorded under the old name and are transcribed here under
the current one.

## `ready` or `needs-work` — calibrating the QA gate

- **Substantive inline evidence satisfies the QA gate's PURPOSE even when the
  literal section-8 block is absent — `ready` with a format note.** What the
  gate is for is a pinned claim: named mutations with the tests that kill them,
  a live dry-run oracle, a base-fail signature. Where the body carries that
  substance in some other shape, the defect is formatting.
  (`issue-pr-cron#28`, `#33`, `rain.erc4626.words#252`.)
- **Zero QA substance is the gate firing, not a formatting note — `needs-work`.**
  (`rainix#252`, `issue-pr-cron#36`, `#37`.)
- **A PR that raises a question for the human AND lacks the block is `design`
  with the gap named, not `needs-work`.** The missing block does not outrank the
  question; a `needs-work` sends it to a producer who cannot answer it.
  (`st0x.deploy#116`, `#150`.)
- **A fresh PR missing the literal block is `needs-work` on the gate even when
  its `## Tests` section carries a substantive mutation table** — the producer
  demonstrably knows the format, so the omission is not the pre-guide shape
  above. (`rain.sol.codegen#32`, `rain.math.float#253`.)
- **The evidence's trust comes from its AUTHOR, not its location.** A trusted
  `🤖 ai:producer` comment carrying the full QA block, pinned to the current
  head, SATISFIES the gate — the producer cannot always edit a PR body, and a
  gate that reads only the body fails on where the bytes sit rather than on what
  they say. (`rain.math.float#253`.)
- **Check every test NAME the QA block claims against the diff before trusting
  its mutation evidence.** A body claiming two tests where the diff commits one
  makes the claimed mutant kill IMPOSSIBLE with the committed suite — that is
  `needs-work` for FALSE QA evidence even when the one committed test is sound.
  (`rain.dia#53`; sibling `#54`, both claimed tests present, `ready`.)
- **A tautological oracle does not satisfy the gate on its own.** A test that
  asserts a helper's output using that same helper pins nothing. The gate is met
  when the discriminating LITERAL-oracle assertions exist somewhere in the diff —
  typically the helper's own unit test, boundary cases included.
  (`cyclo.site#435`.)

## The body is part of the diff — the false-claim `needs-work`

- **A reworked PR whose body still describes the REJECTED iteration is a
  `needs-work` even when the code is now sound.** On any re-vet after a
  send-back, diff the BODY's claims against the reworked diff, not just the
  code. (`rain-org-health#31`: the rework swapped baked sample data for a runtime
  fetch and posted the screenshot, while the body still claimed the sample was
  committed, the live wiring was follow-up, and no screenshot was possible.)
- **A body claiming more scope than the linked issue names is a false premise
  even when its content is accurate.** (`rain.solmem#59` claimed the issue
  "covers three sites"; `#54` names two.)
- **`Closes #N` alongside an explicit "#N stays open" is the CLEAN shape for a
  deliberate partial**, not a contradiction to send back. (`rain.solmem#57`.)
- **The producer cannot edit PR bodies, so a false-body `needs-work` persists
  across head moves until a HUMAN edits the body.** Expect these to re-present
  every run as un-vetted drift and re-pin the send-back until the body actually
  changes; the code being sound does not clear it. (`raindex#2777`, two stale
  "unweighted mean" claims; `claude-audit-skills#32`, every diff-side
  contradiction fixed while the body still 180-inverted its own invariant.)
- **A greening commit can invalidate the QA block it rode in on.** A CI-fix
  commit that splits a test file to satisfy a repo gate makes a body's "no new
  test files" claim false at the new head — a prior `ready` flips to
  `needs-work`. Re-diff body claims against the CURRENT diff on every drift
  re-vet, even when the drift is the producer's own fix. (`rain.dia#55`.)

## `design` — a sound PR with a question inside it

- **When the linked issue is itself a DESIGN ANALYSIS that considered and
  REJECTED the PR's approach, the verdict is `design` — not `ready`, not
  `needs-work`.** Read a linked issue as a SPEC WITH A CHOSEN APPROACH, not just
  a bug report: a PR that overrides the issue's stated approach is a question for
  the human whatever the code quality. Locally flawless code is why it is not
  `needs-work`; `ready` would launder the design choice past the human, and an
  address-changing "requires redeploy at land" PR entrenches it cross-chain
  before the ruling. (`rain.factory#41` implemented `CloneFactory.cloneDeterministic`
  where issue `#40` names that exact approach "NOT the right fix".)
- **A diff that faithfully CONFORMS to a questionable existing convention asks a
  question.** Conformance is what every automated pass judges instead of asking,
  which is how vault-address-as-decimal-`Float` survived an audit, five `ready`
  verdicts and multiple hardening PRs. (`rain.erc4626.words#242`.)
- **A PR whose whole PURPOSE is to force a ruling is `design`, however sound it
  is.** Test-only red-by-design repros with `src/` byte-identical to `main` and a
  body opening "CI is expected to be RED … merging it is not the goal" were both
  recorded `ready` by a vetter whose own note read "red-by-design until the human
  decides guard-vs-document" — that note IS the question, so the verdict was
  `design`. (`rain.solmem#100`, `#101`, over `#54`.)
- **A producer note that CONTESTS a human's ruling WITH EVIDENCE and asks for a
  ruling is `design`. Never overrule either side.** (`rain.flare#161`,
  env-name convention.)
- **A PR that rewires an EXISTING consumer-facing path through an artifact
  pinned to a pre-merge SHA breaks every consumer for the bootstrap window** —
  name the sequencing in the note, and route it to the human rather than banking
  the red. A NEW path nothing consumes yet is harmless-red and only wants the
  caveat documented. (`rainix#264` rewired the soldeer gate through a binary
  added to `flake.nix` in the same PR, command-not-found at the pinned SHA until
  the routine post-merge bump; `rainix#263`, a reusable nothing consumed,
  harmless.)

## `needs-work` — incompleteness and unvalidated premises

- **When a PR body says "merge the other repo's PR first", CONFIRM that PR
  EXISTS and check the referenced interface against the other repo's `main`.**
  A cross-repo `workflow_call` that passes inputs the callee does not declare
  fails at startup, and the cheapest disqualifier is the callee's input list.
  Expect such a PR to keep cycling until the other side lands, and re-check both
  legs each time — the paired PR appearing does not clear it if the interface
  still disagrees. (`st0x.deploy#243` passed `soldeer-next-version` /
  `soldeer-generate-cmd` to a reusable declaring neither, while the "paired
  rainix PR" it cited did not exist; when that PR did appear as `rainix#264`, its
  interface still had no such input and its own body said `#243` must drop it.)
- **A PR that ENFORCES a convention the org does not actually hold is
  `needs-work` — verify the claimed convention against live repos before
  accepting a conformance argument.** rainlanguage repos' own rainix reusable
  workflows ref `@main`; SHA-pinning is a third-party-actions convention, not a
  rainix-reusable one, so the PR that would have enforced a pinned
  `RAINIX_SHA` was itself the defect. (`rainix#252`.)
- **Un-placeholdering a pinned constant must update its placeholder-GUARD test.**
  Hydrating a constant from `address(0)` while the guard still asserts
  `address(0)` leaves a test whose own failure message demands the replacement.
  Grep the constant name across `test/` before passing any pin-hydration diff;
  the stale guard is both an incompleteness ground and usually one of the CI
  reds. (`st0x.deploy#250`.)
- **A producer note admitting its own PR-caused red is unresolved after a failed
  fix attempt is `needs-work` naming the unexplained regression**, not a red to
  wave through as incidental. (`raindex#2793`.)
- **Coverage: sound diffs that cover only SOME of what the issue asked.** All
  three passed AI review and were then human-rejected as incomplete —
  `rain.flare#170` converted the `test/` bare imports and left `script/`;
  `rain.flare#178` centralized `BLOCK_NUMBER` and left the coupled literal its
  scenario is about; `rain.erc4626.words#230` added a discriminating test and
  left one bare `test/` import.

## `close` — moot rather than wrong

- **An empty diff is `close` (superseded), and the tell is cheap: `files=0` in
  the size overview.** A merge-update can converge a branch onto `main` so the
  net contribution is nil while `mergeState` reads CLEAN and the issue is already
  closed. (`rain.flare#141`.)
- **An either/or issue resolved via the OTHER branch is `close`, not
  `needs-work`.** A PR implementing option B against an issue already closed via
  option A re-adds removed dead surface against a resolved premise; it is moot,
  and no rework can save it. Grep the main-derived tree for the symbol to
  confirm. (`rain.flare#177` fork-tested `setDefaultFee` per issue `#82`'s option
  B after the declaration had been dropped from `main`.)
- **A PR that supersedes a PARKED PR covering the same issue set makes the parked
  one a close-as-superseded candidate — once the superseder LANDS, not when it is
  recorded `ready`.** An open PR is not a landed fix. (`cyclo.site#427` over
  parked `#400`; `#435` over parked `#410`.)
- **A `close`-as-duplicate ROTS when its canonical PR changes state — re-examine
  the duplicate basis at every re-vet.** A duplicate whose canonical went
  human-rejected on a formality, with the note calling its substance
  "substantively right", flips to `ready` on its own merits.
  (`rain.flare#175`, closed as duplicate of `#137`.)

## The drift re-vet — what a moved head obliges

- **Establish the last CONTENT commit's date BEFORE deciding whether the QA gate
  even applies.** Head-movers that are NOT content, and must not trigger the
  gate: `merge(main): … [merge-update]` and content-free CI retriggers
  (`chore(ci): retrigger against current org gates (stale-ci guard)`). Both move
  the head, which un-pins the trusted vetter comment and presents the PR as
  drift, while changing nothing to judge. A PR whose content predates the QA
  guide is vetted SUBSTANTIVELY — discriminating evidence in the diff — and is
  not sent back for lacking the literal block; a rework PUSHED after the guide
  without the block still is. Doing this check LAST rather than first has cost
  three PRs sent back and re-readied in one run, and each flip posts two public
  comments. (`rain.flare#153`, `#150`; `dotrain#143`, `#141`.)
- **Merge-update fallout is a defect class of its own.** An automated bot merges
  `origin/main` into every open branch, moving all heads at once, so a re-vet
  run's whole workload is usually these — and the diff's MINUS lines then show
  `main`'s CURRENT code. Check the branch did not UNDO a main-side refactor.
  (`raindex#2779`: `main` had centralized post-task context into `_doOrderPost`;
  the branch re-inlined both call sites and the merge left the helper DEAD with a
  stale body and comment — `needs-work` despite correct behaviour.)
- **Expect the substantive verdict to be unchanged from the prior at-head one on
  a retrigger-only drift** — and re-derive it anyway. That expectation is a prior
  about the workload, never a licence to skip the read.

## Premises are read THIS run

- **When a PR body quotes CI-gate, workflow or repo-convention behaviour, that
  quote is a PREMISE to verify against a checkout made THIS RUN.** An existing
  reference clone of a reusable-workflow repo goes stale enough to INVERT a
  verdict: a stale copy flatly contradicted a PR's gate claims and implied a
  send-back, while a fresh clone showed live `main` matching the PR verbatim —
  `ready`. Whatever the premise names, read it fresh; date-suffix any reference
  clone you keep. (`rain.dia#55` near-miss.)

## Human words on a PR

- **A `👤 human` rework note SUPERSEDES the QA and coverage gates for EXACTLY
  what it enumerates.** The human set the bar: verify the rework did the named
  fix and record `ready`. A note ending "then it's good to merge" or "then
  `Closes #N` stands" is that ruling. It supersedes nothing it did not name.
  (`rain.erc4626.words#230`, `rain.flare#170`.)
- **A human note defining what REMAINS OPEN on an issue is the controlling scope
  for a sibling PR's `Closes`** — a PR closing the issue legitimately when it
  covers exactly the residual the human scoped. (`rain.erc4626.words#185`
  scoping, `#259` closing `#133`.)
- **An UNMARKED trusted-account comment `Reviewed <sha>: approve` is the HUMAN's
  interactive landing-gate word**, not a vetter verdict and not a modeled
  `human:*` state — every AI comment starts `🤖 ai:vetter` / `🤖 ai:producer`.
  It does not make the PR human-sacred, so an un-vetted PR carrying one is vetted
  normally; but never record a verdict that counter-rules it on a FORMALITY.
  Judge the code. (`rain-org-health#76`.)
- **A human's endorsement of one dimension is not an audit of the others, and
  inheriting it is the stateless rule broken while wearing deference.** A human
  calling test substance "strong and discriminating" is a judgement about INTENT,
  never a completed review of a file's naming, imports and structure — and a file
  the diff ADDS has never been reviewed as landed code by anyone, on any head.
  (`rain.erc4626.words#230` was recorded `ready` on a note reading "rework
  executed the human note exactly (no other changes); test substance previously
  human-endorsed", while the PR was in the act of adding a wrapper whose
  `address private immutable _ext;` breaks the audit skill's Solidity
  storage-class rule twice.)

## The screenshot gate's calibration

- **An a11y markup change is a VISIBLE change.** Wrapping a bare icon in a real
  `<button>` introduces a `:focus-visible` outline the icon never had, so
  "pixel-identical, hence no screenshot" waives nothing. Judge the FOCUS and
  HOVER states, not just the resting render, whenever a producer claims an a11y
  markup change is visually inert. (`cyclo.site#436`: code fully correct, four
  discriminating tests, no screenshot and no valid pending marker — `needs-work`.)
- **A rework that completes the CALLER side can ADD a visible state the original
  diff lacked and newly trip the gate** — a PR whose QA gate the rework cured can
  fail on visual evidence it never previously owed. (`cyclo.site#403`.)
- **A waiver made of a CLAIM about the render waives nothing, and a marker
  carried across passes is not evidence.** "Rendered output is pixel-identical"
  waived a change that removed attributes from an input carrying a long
  conditional Tailwind class list — identical is precisely what an unintended
  shift hides behind (`cyclo.site#431`). A screenshot-pending marker rode through
  THREE vetter passes on a rewritten rewards card that, rendered once, listed
  four epochs that had ALL ENDED — the defect all three verdicts missed
  (`cyclo.site#408`).
- **A pure-visual PR carrying screenshots plus an honest verified /
  not-verified analysis is not sent back for the QA block's absence when a human
  has already approved at head.** (`rain-org-health#76`.)
