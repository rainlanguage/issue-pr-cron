# vetter-precedents — the machine vetter's judgement calibration

One skill. The
[issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron)'s vetting
prompt states the **gates**; this states their **calibration** — the readings
that were made wrong once, corrected, and are not to be re-derived from scratch
on every run.

It is grouped by the verdict each entry decides, because that is the question
being asked at the moment it is invoked:

| Section                      | The question it answers                                                    |
| ---------------------------- | -------------------------------------------------------------------------- |
| `ready` or `needs-work`      | which QA omissions are formatting and which are the gate firing            |
| The body is part of the diff | when a PR's own prose is the defect, and why it survives a rework          |
| `design`                     | what makes a locally sound PR a question for the human rather than a merge |
| `needs-work`                 | the incompleteness and unvalidated-premise classes                         |
| `close`                      | what makes a PR moot rather than wrong                                     |
| The drift re-vet             | what a moved head obliges, and which head-movers are not content at all    |
| Premises are read THIS run   | why a stale reference clone inverts a verdict                              |
| Human words on a PR          | what a human's note supersedes, and what it does not                       |
| The screenshot gate          | when a "visually inert" change is not                                      |

Every entry is a **rule**; the PR reference after it is the **evidence** it was
learned from, not a case to match against.

**Nothing here is a tool, a transition, or a verdict.** The skill reaches no MCP
server, takes no argument and writes no GitHub state; it is calibration for
gates the prompt already mandates, and where the two appear to disagree the
prompt is what `pr-review-report` enforces. It is also **not an audit lens** — a
verdict still requires a `pr_checkout` tree at the PR's head and an `audit`
invocation naming that PR at a declared `pr:<number>` scope, and only a skill
whose id ends in `audit` is ever credited as one.

Why it is a separate plugin from `human-fsm`, why it is a skill rather than a
slash command, and what extracting it took out of `review-prompt.txt`:
[The vetter's precedents as a skill](https://github.com/rainlanguage/issue-pr-cron#the-vetters-precedents-as-a-skill).

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install vetter-precedents@issue-pr-cron
```
