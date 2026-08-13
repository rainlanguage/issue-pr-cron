# rain-repo-conventions — what is true around the work

One skill. Every agent working in a rainlanguage repo is bound by the same
standing constraints, and until they were shipped somewhere they were
hand-copied into brief after brief — which is a per-brief cost paid to restate
something that never varies, and a per-brief opportunity to restate it wrongly
or leave one out.

It is grouped by the KIND of rule each entry is, because that is what decides
what a reader does with it:

| Section         | What the group is                                                                                            |
| --------------- | ------------------------------------------------------------------------------------------------------------ |
| **Rules**       | chosen, and they do not expire — nothing about the box lifts one                                             |
| **Facts**       | true of the environment rather than of the work — stated so you can check one, wrong the day the box changes |
| **Workarounds** | a route around a defect elsewhere, each naming what would retire it                                          |

The split is not decoration. A rule is argued with, a fact is verified, and a
workaround is a piece of debt with an owner — an agent that cannot tell which it
is holding treats a stale environment fact as a prohibition, or a prohibition as
something to route around.

**It states properties, never cases.** No PR numbers, no dates, no incidents. An
instruction that needs a worked example to be obeyed is underspecified; a cited
case rots when its subject changes state, and its context cost is paid on every
load. The one anecdote the source list carried — a shared scratch path that
nearly put one repo's test numbers in another repo's PR body — is here as the
mechanism instead, which is the half that is actually applicable.

**It is not a gate and it does not replace one.** The `## QA` block's content is
[QA-GUIDE.md](https://github.com/rainlanguage/issue-pr-cron/blob/main/QA-GUIDE.md)'s,
enforced by `pr-review-report require-qa-block` and judged by the vetter; this
records only that the gate is there and what shape it refuses. Where this file
and a repo's own `CLAUDE.md` disagree, the repo wins.

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install rain-repo-conventions@issue-pr-cron
```
