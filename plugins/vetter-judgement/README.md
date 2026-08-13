# vetter-judgement — the properties that decide a verdict

One skill. The
[issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron)'s vetting
prompt states the **gates** and `pr-review-report` refuses what is
**mechanical**. This states the properties that decide everything left over.

It is grouped by the verdict each instruction bears on, because that is the
question being asked at the moment it is invoked:

| Section                    | The question it answers                                                    |
| -------------------------- | -------------------------------------------------------------------------- |
| The QA gate                | what the gate is actually asking, and what satisfies or supersedes it      |
| The body                   | when a PR's own prose is the defect, and why it survives a rework          |
| `design`                   | what makes a locally sound PR a question for the human rather than a merge |
| `needs-work`               | the incompleteness and unvalidated-premise classes                         |
| `close`                    | what makes a PR moot rather than wrong                                     |
| A moved head               | what a drift re-vet obliges, and which head-movers are not content at all  |
| Premises are read THIS run | why a tree kept from an earlier run inverts verdicts in both directions    |
| A human's words            | what a human's note settles, and what it does not                          |
| The screenshot gate        | what a rework can newly owe                                                |

**It states properties, never cases.** No PR numbers, no dates, no outcomes. An
instruction that needs a worked example to be obeyed is underspecified — the fix
is a sharper instruction. A cited case also invites reasoning by analogy in
place of applying the rule, it rots the moment that PR changes state, and its
context cost is paid on every load while its benefit is an assumption nobody has
measured.

**Nothing here is a tool, a transition, or a verdict.** The skill reaches no MCP
server, takes no argument and writes no GitHub state. Where it and the prompt
appear to disagree, the prompt is what `pr-review-report` enforces. It is also
**not an audit lens** — a verdict still requires a `pr_checkout` tree at the
PR's head and an `audit` invocation naming that PR at a declared `pr:<number>`
scope, and only a skill whose id ends in `audit` is ever credited as one.

**A rule you cannot apply is not argued down.** The prompt carries a
total-function exit: an instruction that does not fit the PR in front of you is
reported and stopped at `design`, naming what could not be applied. That applies
to everything in this file too.

Why it is a separate plugin from `human-fsm`, why it is a skill rather than a
slash command, and what extracting it took out of `review-prompt.txt`:
[The vetter's judgement as a skill](https://github.com/rainlanguage/issue-pr-cron#the-vetters-judgement-as-a-skill).

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install vetter-judgement@issue-pr-cron
```
