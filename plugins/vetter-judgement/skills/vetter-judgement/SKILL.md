---
name: vetter-judgement
description: The judgement half of a PR verdict — the properties that decide `ready` / `needs-work` / `design` / `close` where the prompt's gates and the tool's refusals stop. Covers what the QA gate is actually asking and what supersedes it, what makes a locally sound PR a question for the human, what makes a PR moot rather than wrong, which of a PR's own statements are premises to falsify, what a moved head obliges, and what a human's words on a PR do and do not settle. Invoke once per run, before the first verdict. Triggers on "vet this PR", "record a verdict", "ready or needs-work", "is this a design question", "close or needs-work", "does the QA gate apply", "re-vet at a moved head".
version: 0.1.0
---

# Vetter judgement

The prompt states the gates and the tool refuses what is mechanical. This states
the properties that decide everything left over.

**Four rules about this file, binding over everything in it.**

- **It never overrides the prompt and never adds a verdict.** The four are
  `ready`, `needs-work`, `design`, `close`. Where this and the prompt appear to
  disagree, the prompt is what the tool enforces — and a rule here you cannot
  apply to the PR in front of you takes the prompt's can't-apply exit, exactly
  as one of the prompt's own does.
- **Nothing here narrows the read.** Vetting is a pure function of the PR at its
  current head. No property below is a reason to read less of a diff.
- **Nothing here belongs in a note.** A note describes this code at this head.
- **No examples, by design.** An instruction that needs a worked example to be
  obeyed is underspecified — sharpen the instruction instead. A case citation
  invites reasoning by analogy in place of applying the rule, it rots the moment
  the case changes state, and its context cost is paid on every load.

**Where a rule belongs.** Content that CALIBRATES a gate belongs here. Content a
gate is INCOMPLETE without belongs in the prompt, because the prompt is read in
full on every run and this is read when something invokes it.

## The QA gate: what it is asking

- **The gate asks whether the claim is PINNED, not whether it is formatted.**
  Pinned means: named mutations with the tests that kill them, an oracle
  independent of the implementation, and a stated base-fail signature. A body
  carrying that substance in some other shape is `ready` with a format note. A
  body carrying none of it is `needs-work` on the gate whatever its headings
  say.
- **Format is owed by content that could have used it.** Judge the format
  question against the date of the last CONTENT commit, not the head date.
  Content written before the format existed is judged on substance alone;
  content written after it is held to the literal block. A substantive test
  section does not substitute for the block on content that is owed it.
- **A missing block never outranks an open question.** Where the PR raises
  something only the human can settle, the verdict is `design` with the gap
  named. A `needs-work` dispatches a formatting order to a producer who cannot
  answer the question, and the question goes on being entrenched.
- **Evidence is trusted by its AUTHOR, not its location.** A trusted producer
  comment carrying the full block, pinned to the current head, satisfies the
  gate. The producer cannot always edit a body, and a gate that reads only the
  body fails on where the bytes sit rather than on what they say.
- **Read every test NAME the block claims against the diff.** A claimed mutant
  kill whose killing test is not in the diff is impossible with the committed
  suite. That is FALSE QA evidence and a `needs-work`, even where the tests that
  are committed are sound.
- **An oracle that is the implementation under test pins nothing.** A test
  asserting a helper's output by calling that helper is tautological. The gate
  is met only where discriminating literal-oracle assertions exist somewhere in
  the diff, boundary cases included.

## The body is part of what you judge

- **Every statement a PR body makes about the code is a claim at THIS head.** A
  body describing a superseded iteration is false now, and that is `needs-work`
  even where the code it describes has since become correct. Check the body
  against the current diff on every re-vet.
- **A commit that only greens CI can still falsify the body.** Splitting a file
  or moving a test to satisfy a repo gate can contradict a QA claim the body
  made before it. Re-check the body even when the only drift is the producer's
  own fix.
- **A body claiming a scope the linked issue does not name is a false premise**,
  even where every statement it makes about the code is accurate.
- **`Closes #N` alongside an explicit statement of what `#N` still leaves open
  is the CLEAN shape for a deliberate partial**, not a contradiction to send
  back.
- **The producer cannot edit a PR body.** A `needs-work` whose ground is the
  body therefore does not clear when the head moves. Expect it back every run as
  drift, and re-pin it until the body itself changes — sound code does not clear
  a false body.

## `design`: the PR is a question

- **Read a linked issue as a SPEC WITH A CHOSEN APPROACH, not a bug report.** A
  PR that overrides the approach the issue chose is `design` whatever its code
  quality: flawless code is why it is not `needs-work`, and `ready` launders the
  choice past the human. Where landing it is expensive to reverse — a changed
  deployed address, a published artifact, a migration — that is why the question
  cannot wait to surface later.
- **A diff that faithfully CONFORMS to an existing convention still asks a
  question when the convention is itself the doubt.** Conformance is what every
  automated pass checks instead of asking, so a bad convention survives any
  number of green reviews. Judge the convention, not the conformance.
- **A PR built to force a ruling is `design` however sound it is.** The shape:
  source identical to base, a body saying the red is expected and merging is not
  the goal, tests that exist to demonstrate the question. If your own note would
  state a question, the verdict is `design` and the note IS the question.
- **A producer note that CONTESTS a human ruling with evidence and asks for a
  ruling is `design`.** Never overrule either side.
- **A PR that makes an EXISTING consumer-facing path depend on something the
  same PR adds to an artifact consumers resolve at a pinned revision breaks
  every consumer until that pin is bumped.** Name the sequencing and route it to
  the human. The same shape on a path nothing consumes yet is harmless: the red
  documents the window and nothing downstream is affected. The distinguishing
  question is whether anything already resolves the path being rewired.

## `needs-work`: incompleteness and unvalidated premises

- **Incompleteness survives a clean read, because a diff that does part of the
  job is internally consistent.** Hunt for the ask the diff does NOT touch: the
  second directory the same conversion applies to, the coupled literal a
  centralization leaves behind, the last instance of a pattern the change fixes
  everywhere else. Enumerate what is uncovered in the note.
- **A PR whose body defers to another repo's PR makes two claims — that the PR
  exists, and that the interface it lands is the one this PR calls.** Check both
  against the other repo's default branch. A call passing inputs the callee does
  not declare fails at startup, so the callee's declared input list is the
  cheapest disqualifier. Re-check both legs on every re-vet: the paired PR
  appearing does not clear it if the interface still disagrees.
- **A PR that ENFORCES a convention is claiming the convention exists.** Verify
  it against the live repos it would bind before accepting the conformance
  argument. A convention imported from a neighbouring domain does not
  automatically hold for first-party code.
- **Hydrating a placeholder constant is incomplete until the test that GUARDS
  the placeholder is updated with it.** Search the constant's name across the
  test tree before passing any such diff; the stale guard is both the
  incompleteness and, usually, one of the reds.
- **A red the PR itself caused is a code defect, not incidental CI.** Where the
  producer's own note says a fix attempt failed and the regression is
  unexplained, that is `needs-work` naming the regression.

## `close`: moot rather than wrong

- **A diff with no changed files is `close`.** Merging the base into a branch
  can converge it onto the base so the net contribution is nil while the PR
  still reads mergeable. Zero files in the size overview is the tell.
- **An issue offering alternative resolutions can be settled by the branch this
  PR did not take.** A PR against a premise the base has already resolved is
  moot: `close`, not `needs-work`, because no rework reaches it. Confirm by
  looking for the symbol in the current base tree — a PR re-adding surface the
  base deliberately removed is the common shape.
- **A PR that supersedes a parked one covering the same issues makes the parked
  one a close candidate only once the superseder LANDS.** An open PR is not a
  landed fix.
- **A `close`-as-duplicate rests on the canonical PR's state, so it expires when
  that state changes.** Re-establish the basis on every re-vet: a canonical that
  was sent back can leave the duplicate as the only live carrier of a fix that
  is still wanted.

## A moved head

- **Establish the last CONTENT commit before deciding whether a format gate
  applies.** Head-movers that carry no content — a merge of the base branch, a
  CI retrigger — un-pin the previous verdict and present the PR as drift while
  changing nothing to judge. Applying a format gate to content written before
  that format existed produces a send-back that has to be reversed, and each
  flip posts two public comments.
- **When the base has been merged INTO the branch, the diff's removed lines show
  the base's CURRENT code.** Check the branch has not undone what the base did:
  a re-inlined call site can leave the base's new helper dead with a stale body,
  which is a defect even where behaviour is unchanged.
- **Expect a content-free drift to reach the same verdict, and re-derive it
  anyway.** That expectation is a prior about the workload, never a licence to
  skip the read.

## Premises are read THIS run

- **Every statement a PR makes about how something CURRENTLY behaves is a
  premise — a CI gate, a workflow, a repo convention, a caller's behaviour — and
  it is checked against source read THIS run.** A tree kept from an earlier run
  can be stale enough to invert the verdict in either direction: it can
  contradict a claim that live source supports, and support one live source
  refutes. Read it fresh, and date-stamp any reference tree you keep so its age
  is visible.

## What a human's words settle

- **A human rework note supersedes the QA and coverage gates for EXACTLY what it
  enumerates, and nothing else.** The human set the bar: verify the rework did
  the named fix and record `ready`. A note stating that the named fix is
  sufficient to merge is that ruling.
- **A human note defining what REMAINS OPEN on an issue is the controlling scope
  for a later PR's `Closes`.** A PR covering exactly that residual closes the
  issue legitimately.
- **An unmarked comment from a trusted account approving a head is the HUMAN's
  own landing-gate word**, not a vetter verdict and not a modeled state — every
  AI comment carries its `🤖 ai:*` marker. It does not make the PR sacred, so an
  un-vetted PR carrying one is vetted normally; but never record a verdict that
  counter-rules it on a FORMALITY. Judge the code.
- **An endorsement of one dimension is not a review of the others.** A human
  calling test substance strong is a judgement about intent, not a completed
  review of a file's naming, imports and structure. And a file the diff ADDS has
  never been reviewed as landed code by anyone, on any head — deference to an
  earlier reader is the stateless rule broken in a form that reads as diligence.

## The screenshot gate: what a rework can newly owe

- **A rework that completes the caller side can ADD a visible state the original
  diff lacked**, so a PR whose QA gate the rework cured can fail on visual
  evidence it never previously owed. The gate is re-derived at the current head,
  never inherited from the previous pass.
- **A pure-visual PR carrying screenshots plus an honest verified / not-verified
  analysis is not sent back for the QA block's absence where a human has already
  approved at the head.** The block's purpose is met by the evidence that is
  there.
