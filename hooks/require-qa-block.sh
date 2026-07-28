#!/usr/bin/env bash
# Claude Code PreToolUse hook for Bash. Refuses `gh pr create` when the PR body
# does not carry QA-GUIDE.md section 8's evidence block.
#
# DEPLOY: copy to the box's claude hooks dir and add it as a PreToolUse "Bash"
# hook in the user settings.json (alongside the other block-*.sh hooks).
#
# WHY THIS IS A HOOK AND NOT PROMPT TEXT (issue #83). The contract was already
# written on both sides — `campaign-prompt.txt` step 4 ("No evidence block = the
# PR does not open") and QA-GUIDE.md section 8 ("The vetter rejects any PR whose
# body lacks this block") — and the cron producer HONOURS it: every PR body it
# wrote across the traces in `runs/` carries the block. The five PRs the vetter
# rejected on 2026-07-28 for a missing block were opened while the producer cron
# was DISABLED, by interactive sessions under the same bot account. They never
# read `campaign-prompt.txt`, so no wording in it could have reached them.
#
# That is also why this cannot be an MCP tool: a tool surface only binds a
# session launched with that surface, and the sessions that leak are exactly the
# ones that were not. A PreToolUse hook binds every session on the box, so the
# invariant holds wherever the PR is opened from. For the same reason it is NOT
# gated on RAINIX_CRON_HOOK the way the other two repo hooks are — the cron is
# the compliant population; gating it to the cron would guard everything except
# the thing that actually failed.
#
# The gate is MECHANICAL, deliberately: the block must be PRESENT and name all
# four of section 8's evidence lines. Whether those lines' claims HOLD is the
# vetter's judgement and stays there — that split is the point. The mechanical
# half settles at the producer for one retry inside the run; the judgement half
# is the only thing left to cost a round trip.
#
# Requiring all four is what makes the block a STRUCTURE rather than QA-shaped
# prose, and it is what lets a refusal name the specific lines that are absent.
# Measured against the real corpus (`runs/`): of the 32 bodies the producer
# wrote, 24 pass untouched and 8 are one-to-three lines short of section 8's own
# template — each already carrying the heading and at least one line, so the
# retry is a small edit, not a rewrite. All 6 bodies the vetter rejected for a
# missing block carry no `## QA` heading at all and fail on the first check.
#
# SCOPE: `gh pr create` only. A rework push carries no body to inspect, so
# QA-GUIDE's "a rework without it does not get re-pushed" stays the vetter's.
#
# Exit codes: 0 -> allow; 2 -> block (stderr surfaces to the model).

payload=$(cat)

# Cheap prefilter so the common Bash call never pays for a python start. Every
# spelling of `gh pr create` contains the literal token `create`, so this is a
# superset of what the parser below can block.
case "$payload" in
*create*) ;;
*) exit 0 ;;
esac

QA_HOOK_PAYLOAD="$payload" python3 - <<'PY'
import json
import os
import re
import shlex
import sys

try:
    payload = json.loads(os.environ["QA_HOOK_PAYLOAD"])
except Exception:
    sys.exit(0)

if payload.get("tool_name") != "Bash":
    sys.exit(0)
command = (payload.get("tool_input") or {}).get("command") or ""
cwd = payload.get("cwd") or ""

TEMPLATE = """## QA
- Discriminating tests: <test names> - each fails on base (<how verified>)
- Mutations applied: <line -> mutation -> killing test>
- Oracle: <where expected values come from, independent of the implementation>
- Category check: <issue asks A,B,C; covered A,B,C / Refs because ...>"""

# The four lines section 8 names, each with the pattern that recognises it in a
# body. Loose on wording ("Discriminating test", "Mutation matrix", "Category:")
# because the corpus writes all of those and the vetter accepted them; the
# subject matter is what is recognised, not a fixed string.
REQUIRED = [
    ("Discriminating tests", re.compile(r"discriminating", re.I)),
    ("Mutations applied", re.compile(r"mutation", re.I)),
    ("Oracle", re.compile(r"\boracle\b", re.I)),
    ("Category check", re.compile(r"categor", re.I)),
]
HEADING = re.compile(r"^[ \t]{0,3}(#{1,6})[ \t]*(?:\*\*|__)?[ \t]*QA\b", re.M | re.I)
OPERATORS = {"&&", "||", ";", "|", "&"}
BODY_FLAGS = ("--body", "-b")
BODY_FILE_FLAGS = ("--body-file", "-F")
# gh builds the body itself from commits or a repo template; neither can carry
# evidence the agent produced during this run.
GENERATED_BODY_FLAGS = ("--fill", "-f", "--fill-first", "--fill-verbose", "--template", "-T")


def qa_section(body):
    """The text under the body's `## QA` heading, or None if there is no heading.

    Ends at the next heading of the SAME OR HIGHER level, so a `### Mutation
    matrix` written under `## QA` stays inside the block instead of truncating it.
    """
    m = HEADING.search(body)
    if not m:
        return None
    rest = body[m.end():]
    closer = re.compile(r"^[ \t]{0,3}#{1,%d}[ \t]" % len(m.group(1)), re.M)
    nxt = closer.search(rest)
    return rest[: nxt.start()] if nxt else rest


def missing_lines(section):
    """Section 8's lines the QA section does not name (all four when there is none)."""
    return [name for name, pat in REQUIRED if not (section and pat.search(section))]


def refuse(reason, detail, section=None):
    missing = missing_lines(section)
    present = [name for name, _ in REQUIRED if name not in missing]
    lines = [
        "BLOCKED - `gh pr create` without the QA-GUIDE.md section-8 evidence block.",
        "",
        "  " + reason,
    ]
    if detail:
        lines.append("  " + detail)
    lines += [
        "",
        "The vetter rejects any PR whose body lacks this block, so opening it now",
        "costs a full round trip through the queue. Put it in the body instead:",
        "",
        TEMPLATE,
        "",
    ]
    if present:
        lines.append("  already present: " + ", ".join(present))
    lines.append("  still missing: " + ", ".join(missing))
    lines += [
        "",
        "You ran the mutation testing to get here - transcribe what it produced.",
        "`n/a` with a reason is a valid value for a line the change cannot have",
        "(a docs-only diff has no mutations); an ABSENT line is not.",
    ]
    sys.stderr.write("\n".join(lines) + "\n")
    sys.exit(2)


def segments(tokens):
    """Split a shlex'd command line on shell operators."""
    out, cur = [], []
    for t in tokens:
        if t in OPERATORS:
            out.append(cur)
            cur = []
        else:
            cur.append(t)
    out.append(cur)
    return out


def pr_create_spans(seg):
    """[start, end) index ranges of each `gh pr create` invocation in one segment.

    Matches `gh` `pr` `create` as CONSECUTIVE non-flag words rather than at the
    head of the segment, so `timeout 60 gh pr create …` and a newline-joined
    `git push` + `gh pr create …` (shlex flattens newlines into one segment) are
    both seen. Three consecutive words cannot be spoofed by a `--title "gh pr
    create"`, which is a single token.
    """
    words = [(i, t) for i, t in enumerate(seg) if not t.startswith("-")]
    starts = [
        words[k][0]
        for k in range(len(words) - 2)
        if [words[k][1], words[k + 1][1], words[k + 2][1]] == ["gh", "pr", "create"]
    ]
    return [(s, starts[j + 1] if j + 1 < len(starts) else len(seg)) for j, s in enumerate(starts)]


def resolve(path, tokens):
    """Absolute as given; otherwise relative to a leading `cd <dir>`, else the session cwd."""
    if os.path.isabs(path):
        return path
    for s in segments(tokens):
        if s[:1] == ["cd"] and len(s) > 1:
            return os.path.join(s[1], path)
    return os.path.join(cwd, path) if cwd else path


def bodies(argv, tokens):
    """Every body this invocation supplies, plus whether gh was told to generate one.

    Returns (list of (source, text), generated).
    """
    found, generated = [], False
    i = 0
    while i < len(argv):
        tok, val = argv[i], None
        if tok.startswith("--") and "=" in tok:
            tok, val = tok.split("=", 1)
        if tok in BODY_FLAGS or tok in BODY_FILE_FLAGS:
            if val is None:
                i += 1
                val = argv[i] if i < len(argv) else ""
            if tok in BODY_FLAGS:
                found.append(("--body", val))
            elif val == "-":
                refuse(
                    "`--body-file -` reads the body from stdin, which cannot be checked here.",
                    "Write the body to a file and pass its absolute path.",
                )
            else:
                path = resolve(val, tokens)
                try:
                    with open(path, encoding="utf-8", errors="replace") as fh:
                        found.append(("--body-file " + path, fh.read()))
                except OSError as exc:
                    refuse(
                        "could not read the --body-file: %s (%s)." % (path, exc.strerror),
                        "Pass an ABSOLUTE path to a file that exists when the command runs.",
                    )
        elif tok in GENERATED_BODY_FLAGS:
            generated = True
        i += 1
    return found, generated


try:
    tokens = shlex.split(command, comments=False)
except ValueError:
    # Unparseable (unbalanced quote, stray backslash). Fail CLOSED if it still
    # looks like a PR open — a parse failure must not become the way through.
    if re.search(r"\bgh\b[\s\S]*\bpr\b[\s\S]*\bcreate\b", command):
        refuse(
            "could not parse this command line, so its PR body cannot be checked.",
            "Open the PR with a plain `gh pr create … --body-file <absolute path>`.",
        )
    sys.exit(0)

for seg in segments(tokens):
    for start, end in pr_create_spans(seg):
        found, generated = bodies(seg[start:end], tokens)
        if not found:
            refuse(
                "`--fill`/`--template` builds the body from commits or a repo template."
                if generated
                else "this `gh pr create` supplies no body to carry the block.",
                "Pass `--body-file <absolute path>` with the section-8 block in it.",
            )
        # gh takes one body; if several are named, a complete one anywhere passes.
        worst = None
        for source, text in found:
            section = qa_section(text)
            if not missing_lines(section):
                worst = None
                break
            if worst is None:
                worst = (source, section)
        if worst is None:
            continue
        source, section = worst
        if section is None:
            refuse("the body (%s) has no `## QA` heading." % source, "")
        refuse(
            "the `## QA` section in the body (%s) is incomplete." % source,
            "A heading is not the block — section 8 is all four lines.",
            section,
        )
sys.exit(0)
PY
