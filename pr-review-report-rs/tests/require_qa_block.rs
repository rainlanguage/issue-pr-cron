//! Behavioural tests for `pr-review-report require-qa-block`.
//!
//! The subcommand is the producer-side half of QA-GUIDE.md section 8: a PR opened without the
//! evidence block is known-bad the moment it is opened, so it is refused at `gh pr create` rather
//! than discovered a queue round-trip later by the vetter (#83). What it guards is a CONTENT
//! invariant on a command line, and a command line is not something a static read of the source can
//! judge — the failure modes are all parsing (a chained command, a relative `--body-file`, a heading
//! whose section a subheading truncates). So every test here drives the real binary with a real
//! PreToolUse payload and asserts what it actually did: the exit code the harness reads (0 allow,
//! 2 block) and the stderr the model reads.
//!
//! These live in `tests/` for the same reasons as `refresh_human_queue.rs`: they need nothing from
//! the binary's internals, and the crate's unit tests are one 14k-line file.
//!
//! Unlike the shell hook this replaced, NOTHING here is skippable. The subject is
//! `env!("CARGO_BIN_EXE_pr-review-report")` — a build artefact of this crate — so the suite runs
//! wherever cargo does, INCLUDING inside the nix derivation's check phase, where the repo root is
//! not part of the source and every test that drove a `hooks/*.sh` script returned early instead.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A section-8-complete QA block: all four lines, the shape the guide's template gives.
const COMPLETE_BLOCK: &str = "## QA\n\
     - Discriminating tests: t_guard_negated — fails on base (ran on the base checkout)\n\
     - Mutations applied: guard.rs:12 negate -> t_guard_negated\n\
     - Oracle: the issue's worked example, not the implementation\n\
     - Category check: issue asks A,B; covered A,B\n";

/// The gate under test, plus a scratch directory it can be pointed at.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join("require-qa-block-tests")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        Self { dir }
    }

    /// Write a PR body file and return its absolute path as a string.
    fn body_file(&self, name: &str, contents: &str) -> String {
        let p = self.dir.join(name);
        std::fs::write(&p, contents).expect("write body file");
        p.display().to_string()
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    /// Drive the gate with a Bash payload from the fixture dir. Returns (exit code, stderr).
    fn bash(&self, command: &str) -> (i32, String) {
        self.bash_from(&self.dir.display().to_string(), command)
    }

    /// The same, with the session cwd chosen — the two ways a relative `--body-file` resolves
    /// (a leading `cd`, or the session's own directory) are only distinguishable when they differ.
    fn bash_from(&self, cwd: &str, command: &str) -> (i32, String) {
        self.payload(&serde_json::json!({
            "tool_name": "Bash",
            "cwd": cwd,
            "tool_input": {"command": command},
        }))
    }

    /// Drive the gate with a whole payload, so a non-Bash tool can be exercised too.
    fn payload(&self, payload: &serde_json::Value) -> (i32, String) {
        self.payload_env(payload, &[])
    }

    fn payload_env(&self, payload: &serde_json::Value, env: &[(&str, &str)]) -> (i32, String) {
        run_gate(payload.to_string().as_bytes(), env)
    }
}

/// Feed the subcommand a raw payload on stdin, exactly as Claude Code's PreToolUse hook does.
fn run_gate(payload: &[u8], env: &[(&str, &str)]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pr-review-report"))
        .arg("require-qa-block")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the gate");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("gate stdin")
            .write_all(payload)
            .expect("write the hook payload");
    }
    let out = child.wait_with_output().expect("gate exit");
    (
        out.status.code().expect("the gate always exits normally"),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// `gh pr create` naming a body file, in the shape the producer traces actually use.
fn open_pr(body_file: &str) -> String {
    format!("gh pr create --assignee thedavidmeister --title \"t\" --body-file {body_file} 2>&1 | tail -2")
}

fn assert_blocked(code: i32, stderr: &str) {
    assert_eq!(code, 2, "expected a block (exit 2), got {code}: {stderr}");
    assert!(
        stderr.contains("BLOCKED"),
        "a block must say so on stderr, where the model reads it: {stderr}"
    );
}

fn assert_allowed(code: i32, stderr: &str) {
    assert_eq!(code, 0, "expected the command to pass through: {stderr}");
}

// --- the invariant itself ------------------------------------------------------------------

#[test]
fn a_body_carrying_the_section_8_block_opens() {
    let f = Fixture::new("complete");
    let body = f.body_file(
        "body.md",
        &format!("Closes #1\n\nA fix.\n\n{COMPLETE_BLOCK}"),
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn a_body_with_no_qa_heading_is_blocked() {
    let f = Fixture::new("no-heading");
    let body = f.body_file("body.md", "Closes #1\n\nA fix with no evidence at all.\n");
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    assert!(
        err.contains("no `## QA` heading"),
        "the refusal must name the defect: {err}"
    );
    assert!(
        err.contains(&body),
        "the refusal must name the body it read, so the fix is unambiguous: {err}"
    );
}

#[test]
fn an_incomplete_qa_section_is_blocked() {
    let f = Fixture::new("incomplete");
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating tests: t_one\n- Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
}

#[test]
fn the_refusal_names_the_lines_that_are_missing_and_the_ones_that_are_not() {
    let f = Fixture::new("names-missing");
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating tests: t_one\n- Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    assert!(
        err.contains("still missing: Category check"),
        "a refusal has to say WHAT to add, not just that something is wrong: {err}"
    );
    assert!(
        err.contains("already present: Discriminating tests, Mutations applied, Oracle"),
        "and it must not ask again for the lines already written: {err}"
    );
}

#[test]
fn all_four_subjects_on_one_line_is_not_the_block() {
    let f = Fixture::new("one-liner");
    // Section 8 is four SEPARATE entries. A single bullet naming all four subjects satisfies a
    // per-keyword search while being nothing like the block, so it must not pass.
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating mutation oracle category: all done\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    assert!(
        err.contains("not on four distinct lines"),
        "the refusal must say what is wrong with it: {err}"
    );
}

#[test]
fn one_entry_per_line_is_the_block() {
    let f = Fixture::new("one-per-line");
    // The control for the test above, and for the distinctness check generally: a body whose
    // first line names two subjects still passes when a distinct line exists for each of them.
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating test, mutation-validated: t_one\n\
         - Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n\
         - Category check: asks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn an_oracle_inside_a_longer_word_does_not_count() {
    let f = Fixture::new("oracle-substring");
    // `Oracle` is the one subject matched as a WHOLE WORD: a filename that happens to contain the
    // stem is not an oracle line. A substring search would take `oracles.md` for the Oracle entry
    // and then fail on distinctness instead — a refusal naming the wrong defect.
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating tests: t_one\n\
         - Mutations applied: see oracles.md\n- Category check: asks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    assert!(
        err.contains("still missing: Oracle"),
        "a stem inside a longer word is not the Oracle line: {err}"
    );
}

#[test]
fn the_refusal_prints_the_section_8_template() {
    let f = Fixture::new("template");
    let body = f.body_file("body.md", "Closes #1\n");
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    for line in [
        "- Discriminating tests: <test names>",
        "- Mutations applied: <line -> mutation -> killing test>",
        "- Oracle: <where expected values come from",
        "- Category check: <issue asks A,B,C;",
    ] {
        assert!(
            err.contains(line),
            "the template line {line:?} must be in the refusal so the retry needs no lookup: {err}"
        );
    }
}

#[test]
fn an_inline_body_is_checked_like_a_file() {
    let f = Fixture::new("inline");
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body \"Closes #1\n\n{COMPLETE_BLOCK}\""
    ));
    assert_allowed(code, &err);
    let (code, err) = f.bash("gh pr create --title t --body \"Closes #1\n\nno evidence\"");
    assert_blocked(code, &err);
}

#[test]
fn an_equals_form_body_flag_is_parsed() {
    let f = Fixture::new("equals");
    let body = f.body_file("body.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!("gh pr create --title t --body-file={body}"));
    assert_allowed(code, &err);
    let bare = f.body_file("bare.md", "Closes #1\n");
    let (code, err) = f.bash(&format!("gh pr create --title t --body-file={bare}"));
    assert_blocked(code, &err);
}

#[test]
fn the_short_body_flags_are_the_same_flags() {
    let f = Fixture::new("short-flags");
    // `-b` and `-F` are gh's own spellings of `--body` and `--body-file`. Dropping either would not
    // show up as a leak — an unrecognised flag reads as "no body at all", which still blocks — it
    // would show up as a COMPLIANT PR refused, so the allow cases are what pin them.
    let body = f.body_file("body.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!("gh pr create --title t -F {body}"));
    assert_allowed(code, &err);
    let (code, err) = f.bash(&format!("gh pr create --title t -b \"{COMPLETE_BLOCK}\""));
    assert_allowed(code, &err);
    let (code, err) = f.bash("gh pr create --title t -b \"Closes #1\"");
    assert_blocked(code, &err);
}

#[test]
fn when_several_bodies_are_named_a_complete_one_passes_and_the_first_bad_one_is_reported() {
    let f = Fixture::new("several-bodies");
    let good = f.body_file("good.md", COMPLETE_BLOCK);
    let bare = f.body_file("bare.md", "Closes #1\n");
    let other = f.body_file("other.md", "Closes #2\n");
    // gh takes ONE body, so a line naming two is already malformed — but which one it would use is
    // gh's business, not the gate's. Refusing on the strength of a body gh may discard would block
    // a compliant PR, so a complete body ANYWHERE in the invocation passes.
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body-file {bare} --body-file {good}"
    ));
    assert_allowed(code, &err);
    // When none is complete the FIRST is the one reported: it is the one the author wrote first,
    // and a refusal that names a later argument sends the fix to the wrong file.
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body-file {bare} --body-file {other}"
    ));
    assert_blocked(code, &err);
    assert!(
        err.contains(&bare) && !err.contains(&other),
        "the refusal must name the first incomplete body: {err}"
    );
}

// --- the QA section's extent ---------------------------------------------------------------

#[test]
fn a_subheading_does_not_truncate_the_qa_section() {
    let f = Fixture::new("subheading");
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n\n### Discriminating tests\nt_one\n\n### Mutations applied\nL1 -> flip -> t_one\n\n### Oracle\nthe issue\n\n### Category check\nasks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn evidence_after_the_qa_section_ends_does_not_count() {
    let f = Fixture::new("outside");
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating tests: t_one\n- Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n\n## Notes\n- Category check: issue asks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    assert!(
        err.contains("still missing: Category check"),
        "a line under a LATER same-level heading is outside the block: {err}"
    );
}

#[test]
fn an_issue_reference_at_the_start_of_a_line_is_not_a_heading() {
    let f = Fixture::new("hash-number");
    // A heading needs whitespace after its hashes. Without that requirement `#83` — an issue
    // reference, which QA blocks in this repo write constantly — reads as an `<h1>` and TRUNCATES
    // the section, so the lines after it stop counting and a complete block is refused.
    let body = f.body_file(
        "body.md",
        "Closes #83\n\n## QA\n- Discriminating tests: t_one\n\
         - Mutations applied: L1 -> flip -> t_one\n\
         #83 is what this covers, and the block continues:\n\
         - Oracle: the issue\n- Category check: asks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn a_line_that_is_not_a_markdown_heading_does_not_open_the_block() {
    let f = Fixture::new("not-a-heading");
    // Two spellings that LOOK like the heading and are not: four spaces of indent makes it an
    // indented code block (PR bodies in this repo quote section 8's template that way), and seven
    // hashes is past markdown's six. Either one taken for the heading would let a body that merely
    // QUOTES the template pass as one that carries it.
    for heading in ["    ## QA", "####### QA"] {
        let body = f.body_file(
            "body.md",
            &format!(
                "Closes #1\n\n{heading}\n- Discriminating tests: t_one\n\
                 - Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n\
                 - Category check: asks A; covered A\n"
            ),
        );
        let (code, err) = f.bash(&open_pr(&body));
        assert_blocked(code, &err);
        assert!(
            err.contains("no `## QA` heading"),
            "{heading:?} is not a markdown heading: {err}"
        );
    }
}

#[test]
fn a_bolded_qa_heading_is_the_same_heading() {
    let f = Fixture::new("bold-heading");
    // `## **QA**` is what a body written in a bold-headings style produces, and the corpus has it.
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## **QA**\n- Discriminating tests: t_one\n\
         - Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n\
         - Category check: asks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn a_heading_that_merely_starts_with_qa_is_not_the_block() {
    let f = Fixture::new("qa-prefix");
    // `QA` has to end on a word boundary. Without that, `## QAnything` opens the block and every
    // line under an unrelated heading counts as evidence.
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QAnything\n- Discriminating tests: t_one\n\
         - Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n\
         - Category check: asks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
    assert!(
        err.contains("no `## QA` heading"),
        "a heading that merely starts with QA is not the QA heading: {err}"
    );
}

// --- bodies gh would build for itself, and bodies that cannot be read ----------------------

#[test]
fn a_pr_create_with_no_body_flag_is_blocked() {
    let f = Fixture::new("no-body");
    let (code, err) = f.bash("gh pr create --assignee thedavidmeister --title t");
    assert_blocked(code, &err);
    assert!(
        err.contains("no body"),
        "the refusal must say the command supplied no body: {err}"
    );
}

#[test]
fn fill_cannot_substitute_for_the_block() {
    let f = Fixture::new("fill");
    for flag in [
        "--fill",
        "-f",
        "--fill-first",
        "--fill-verbose",
        "--template pr.md",
        "-T pr.md",
    ] {
        let (code, err) = f.bash(&format!("gh pr create --title t {flag}"));
        assert_blocked(code, &err);
        assert!(
            err.contains("commits or a repo template"),
            "{flag}: a generated body cannot carry evidence from this run: {err}"
        );
    }
}

#[test]
fn an_unreadable_body_file_fails_closed() {
    let f = Fixture::new("unreadable");
    let missing = f.path("never-written.md");
    let (code, err) = f.bash(&open_pr(&missing));
    assert_blocked(code, &err);
    assert!(
        err.contains(&missing),
        "the refusal must name the path it could not read: {err}"
    );
}

#[test]
fn a_body_file_read_from_stdin_is_blocked() {
    // NOT named "stdin-…": the fixture's own path appears in the refusal, so a fixture name
    // containing the word would satisfy the assertion below without the gate saying anything.
    let f = Fixture::new("dash-body");
    let (code, err) = f.bash("gh pr create --title t --body-file -");
    assert_blocked(code, &err);
    assert!(
        err.contains("reads the body from stdin"),
        "the refusal must say why a stdin body cannot be checked: {err}"
    );
}

#[test]
fn a_body_file_relative_to_a_leading_cd_is_resolved() {
    let f = Fixture::new("relative-cd");
    // The `cd` target is NOT the session cwd, so only the `cd` can resolve `body.md`.
    let sub = f.dir.join("sub");
    std::fs::create_dir_all(&sub).expect("create sub");
    std::fs::write(sub.join("body.md"), COMPLETE_BLOCK).expect("write body");
    let (code, err) = f.bash_from(
        &f.dir.display().to_string(),
        &format!(
            "cd {} && gh pr create --title t --body-file body.md",
            sub.display()
        ),
    );
    assert_allowed(code, &err);
}

#[test]
fn the_last_cd_before_the_invocation_is_the_one_that_counts() {
    let f = Fixture::new("sequential-cd");
    // `cd good; cd bad; gh pr create --body-file body.md` opens the PR from `bad`. Checking the
    // FIRST cd would read `good/body.md` — a file the shell never opens — and pass a bad PR.
    for (dir, contents) in [("good", COMPLETE_BLOCK), ("bad", "Closes #1\n")] {
        let d = f.dir.join(dir);
        std::fs::create_dir_all(&d).expect("create dir");
        std::fs::write(d.join("body.md"), contents).expect("write body");
    }
    // Both separators, because they walk different lines of the shell-state walk: `;` puts each
    // `cd` in its own segment (state carried BETWEEN segments), a newline leaves all three in one
    // (state carried WITHIN a segment, up to the invocation).
    for sep in ["\n", " ; "] {
        let (code, err) = f.bash(&format!(
            "cd {good}{sep}cd {bad}{sep}gh pr create --title t --body-file body.md",
            good = f.dir.join("good").display(),
            bad = f.dir.join("bad").display()
        ));
        assert_blocked(code, &err);
        assert!(
            err.contains("/bad/body.md"),
            "the refusal must name the body the SHELL would open: {err}"
        );
    }
}

#[test]
fn relative_cds_compound_and_dot_dot_resolves() {
    let f = Fixture::new("compounding-cd");
    // Sequential `cd`s are not "the last one wins" — they COMPOUND, so `cd sub` then `cd ..` is
    // back where it started. Resolving each against the session cwd instead would read
    // `sub/body.md` (complete) for a command the shell runs against `body.md` (bare).
    std::fs::create_dir_all(f.dir.join("sub")).expect("create sub");
    std::fs::write(f.dir.join("sub/body.md"), COMPLETE_BLOCK).expect("write sub body");
    std::fs::write(f.dir.join("body.md"), "Closes #1\n").expect("write bare body");
    let (code, err) = f.bash("cd sub && gh pr create --title t --body-file body.md");
    assert_allowed(code, &err);
    let (code, err) = f.bash("cd sub && cd .. && gh pr create --title t --body-file body.md");
    assert_blocked(code, &err);
    assert!(
        err.contains("no `## QA` heading"),
        "`cd sub && cd ..` lands back at the session cwd, whose body is bare: {err}"
    );
    // And it is named the way the shell would land on it. The kernel resolves `..` either way, so
    // an unnormalised path still READS the right file — it just tells the model to go fix
    // `sub/../body.md`, a path that is not where the body is.
    assert!(
        err.contains(&format!("--body-file {}/body.md", f.dir.display())),
        "the refusal must name the resolved path, not the walk that reached it: {err}"
    );
}

#[test]
fn an_invocations_arguments_stop_at_the_shell_operator() {
    let f = Fixture::new("segment-bounds");
    let body = f.body_file("body.md", COMPLETE_BLOCK);
    // A chained command's flags are not this invocation's flags. Without the split on `&&`, the
    // `--body-file` belonging to the command AFTER it is read as one of `gh pr create`'s own — so a
    // note the run writes later fails a compliant PR open closed, naming a file gh never touches.
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body-file {body} && gh issue comment 83 --body-file {}",
        f.path("note-written-later.md")
    ));
    assert_allowed(code, &err);
}

#[test]
fn a_body_file_relative_to_the_session_cwd_is_resolved() {
    let f = Fixture::new("relative-cwd");
    f.body_file("body.md", COMPLETE_BLOCK);
    // No `cd`: the session's own directory is the only thing that can resolve it.
    let (code, err) = f.bash("gh pr create --title t --body-file body.md");
    assert_allowed(code, &err);
    let bare = f.dir.join("bare.md");
    std::fs::write(&bare, "Closes #1\n").expect("write bare");
    let (code, err) = f.bash("gh pr create --title t --body-file bare.md");
    assert_blocked(code, &err);
}

// --- finding the invocation at all ----------------------------------------------------------

#[test]
fn a_chained_pr_create_is_still_gated() {
    let f = Fixture::new("chained");
    let bare = f.body_file("bare.md", "Closes #1\n");
    let (code, err) = f.bash(&format!(
        "git -C /w push -q -u origin br && gh pr create -R o/r --head br --body-file {bare}"
    ));
    assert_blocked(code, &err);
}

#[test]
fn gh_reached_through_a_prefix_command_is_still_gated() {
    let f = Fixture::new("prefixed");
    let (code, err) = f.bash("timeout 60 gh pr create --title t --body \"no evidence\"");
    assert_blocked(code, &err);
}

#[test]
fn an_interpreter_wrapped_pr_create_is_still_gated() {
    let f = Fixture::new("wrapped");
    // The lexer sees the script as ONE token, so there is no `gh` `pr` `create` word sequence to
    // find and the line would sail through — the wrapper bypass block-nix-wrap-gh.sh exists for.
    for cmd in [
        "bash -c 'gh pr create --title t --body \"no evidence\"'",
        "sh -c 'gh pr create --title t --body \"no evidence\"'",
        // `-lc`/`-ic` are the login/interactive spellings — a `-c` flag is any `-<letters>c`, not
        // the literal two characters, or a login shell is a way out.
        "bash -lc 'gh pr create --title t --body \"no evidence\"'",
        "nix shell nixpkgs#gh --command bash -c 'gh pr create --title t --body \"no evidence\"'",
    ] {
        let (code, err) = f.bash(cmd);
        assert_blocked(code, &err);
    }
}

#[test]
fn an_interpreter_wrapped_pr_create_with_the_block_opens() {
    let f = Fixture::new("wrapped-ok");
    let body = f.body_file("body.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!(
        "bash -c 'gh pr create --title t --body-file {body}'"
    ));
    assert_allowed(code, &err);
}

#[test]
fn a_second_pr_create_in_one_command_is_not_missed() {
    let f = Fixture::new("second");
    let good = f.body_file("good.md", COMPLETE_BLOCK);
    let bare = f.body_file("bare.md", "Closes #2\n");
    // Both separators, because they take different paths through the parser: `&&` becomes two
    // segments, while a newline is whitespace to the lexer and leaves TWO invocations in ONE segment.
    for sep in [" && ", "\n"] {
        let (code, err) = f.bash(&format!(
            "gh pr create --title one --body-file {good}{sep}gh pr create --title two --body-file {bare}"
        ));
        assert_blocked(code, &err);
        assert!(
            err.contains(&bare),
            "the refusal must name the SECOND body, not the first: {err}"
        );
    }
}

#[test]
fn a_pr_create_assembled_through_a_variable_is_refused() {
    let f = Fixture::new("expanded-word");
    let bare = f.body_file("bare.md", "Closes #1\n");
    // `C=create; gh pr $C …` is `gh pr create` to bash and three unrelated words to a literal
    // match, so the gate would find no invocation, read no body, and allow the open — one shell
    // variable defeating the whole thing. The lexer resolves quoting, never expansion, so where a
    // word in the invocation position is unevaluable the gate cannot tell what the command is, and
    // "cannot tell" is a refusal.
    for cmd in [
        format!("C=create; gh pr $C --title t --body-file {bare}"),
        format!("gh pr $(echo create) --title t --body-file {bare}"),
        format!("gh pr `echo create` --title t --body-file {bare}"),
        format!("gh $SUB create --title t --body-file {bare}"),
        format!("timeout 60 gh pr $C --title t --body-file {bare}"),
        // A wrapper is not a way out of this either: an interpreter payload is re-checked as a
        // command in its own right, so the same rule applies at every depth.
        format!("bash -c 'gh pr $C --title t --body-file {bare}'"),
    ] {
        let (code, err) = f.bash(&cmd);
        assert_blocked(code, &err);
        assert!(
            err.contains("cannot tell which words are the invocation"),
            "{cmd}: the refusal must say WHY it cannot check, so the way out is obvious: {err}"
        );
    }
    // The way out the refusal prints has to actually work: written literally, the same open is
    // judged on its body like any other.
    let good = f.body_file("good.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!("gh pr create --title t --body-file {good}"));
    assert_allowed(code, &err);
}

#[test]
fn an_expansion_that_is_not_the_invocation_is_left_alone() {
    let f = Fixture::new("expansion-elsewhere");
    let good = f.body_file("good.md", COMPLETE_BLOCK);
    // The rule costs something, so it is bounded: TWO of the three words must still be literal.
    // Without that bound every command carrying a couple of variables near the word "create" is
    // refused, and a gate that blocks `gh pr view $N` to catch `gh pr $C` is not worth deploying.
    for cmd in [
        "gh pr view $N --json body".to_string(),
        "gh pr list --state $STATE".to_string(),
        "echo $A $B create".to_string(),
        "mkdir -p $DIR && cd $DIR".to_string(),
        format!("gh pr create --title t --body-file {good}"),
        // The `$` is in the BODY, which is data, not the invocation.
        "gh pr create --title t --body \"the fee is $5\n\n## QA\n\
         - Discriminating tests: t_one\n- Mutations applied: L1 -> flip -> t_one\n\
         - Oracle: the issue\n- Category check: asks A; covered A\""
            .to_string(),
    ] {
        let (code, err) = f.bash(&cmd);
        assert_allowed(code, &err);
    }
}

#[test]
fn an_eval_wrapped_pr_create_is_still_gated() {
    let f = Fixture::new("eval-wrapped");
    // `eval` is the shell interpreting its own argument — the same hand-off as `bash -c`, and
    // otherwise a clean way past every literal word match here, because the whole command is one
    // token. It is followed for the same reason and with the same depth limit.
    let (code, err) = f.bash("eval \"gh pr create --title t --body 'no evidence'\"");
    assert_blocked(code, &err);
    // …and nested inside a wrapper, which is the spelling that stacks both hand-offs.
    let (code, err) = f.bash("bash -c 'eval \"gh pr create --title t --body no-evidence\"'");
    assert_blocked(code, &err);
    let good = f.body_file("good.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!(
        "eval \"gh pr create --title t --body-file {good}\""
    ));
    assert_allowed(code, &err);
}

#[test]
fn a_draft_pr_is_gated_too() {
    let f = Fixture::new("draft");
    let bare = f.body_file("bare.md", "Closes #1\n");
    let (code, err) = f.bash(&format!(
        "gh pr create --draft --title t --body-file {bare}"
    ));
    assert_blocked(code, &err);
}

#[test]
fn an_unparseable_command_that_opens_a_pr_fails_closed() {
    let f = Fixture::new("unparseable");
    let (code, err) = f.bash("gh pr create --title t --body \"unterminated");
    assert_blocked(code, &err);
    assert!(
        err.contains("could not parse"),
        "a parse failure must not become the way through: {err}"
    );
}

#[test]
fn a_command_ending_in_a_dangling_escape_fails_closed() {
    let f = Fixture::new("dangling-escape");
    // The other way a line stops lexing: it ends mid-escape, so the last word is unknowable. A
    // trailing backslash is what a line-continuation looks like when the continuation is lost.
    // Both positions, because they are different states of the lexer: after a space the escape
    // OPENS a word, glued to the previous token it continues one.
    for cmd in [
        "gh pr create --title t --body-file body.md \\",
        "gh pr create --title t --body-file body.md\\",
    ] {
        let (code, err) = f.bash(cmd);
        assert_blocked(code, &err);
        assert!(
            err.contains("could not parse"),
            "{cmd:?}: an unfinished escape must fail closed like an unbalanced quote: {err}"
        );
    }
}

#[test]
fn a_backslash_escape_is_removed_from_the_argument_it_escapes() {
    let f = Fixture::new("escape-removed");
    // Inside double quotes a backslash escapes `"` and `\` and is REMOVED — everything else keeps
    // it, so `"\n"` stays two characters. Keeping the removed ones would hand the gate a path two
    // characters off from the one the shell opens: it would then fail a compliant PR closed on a
    // file that does not exist, which is the exact false-block a fail-closed gate cannot afford.
    let dir = f.dir.join("quote\"and\\slash");
    std::fs::create_dir_all(&dir).expect("create dir");
    std::fs::write(dir.join("body.md"), COMPLETE_BLOCK).expect("write body");
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body-file \"{}/quote\\\"and\\\\slash/body.md\"",
        f.dir.display()
    ));
    assert_allowed(code, &err);
}

#[test]
fn escaped_quotes_inside_a_body_do_not_end_it() {
    let f = Fixture::new("escaped-quote");
    // A `\"` inside a double-quoted body is a literal quote, not the end of the argument. Getting
    // that wrong splits the body in two and the block goes missing from a body that has it.
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body \"He said \\\"ship it\\\".\n\n{COMPLETE_BLOCK}\""
    ));
    assert_allowed(code, &err);
    let (code, err) =
        f.bash("gh pr create --title t --body \"He said \\\"ship it\\\".\n\nno evidence\"");
    assert_blocked(code, &err);
}

// --- everything the gate must keep its hands off --------------------------------------------

#[test]
fn commands_that_do_not_open_a_pr_pass_through() {
    let f = Fixture::new("passthrough");
    for cmd in [
        "gh pr view 12 --json body",
        "gh issue create --title t --body \"no qa block here\"",
        "git commit -m \"create the release tag\"",
        "cargo test --all-features",
        "gh pr list --state open",
        "grep -rn \"gh pr create\" campaign-prompt.txt",
        "rg 'gh pr create' --files-with-matches",
        // A line that does not lex is only failed closed when it still looks like a PR open. These
        // two do not: one names none of the three words, the other names them OUT OF ORDER. An
        // unordered or unconditional fail-closed here would wedge every unbalanced-quote command
        // on the box, which is a far worse failure than the one this gate guards.
        "git commit -m \"unterminated",
        "echo \"create a pr with gh",
        // …and this one names them in order but not in the case the shell would run. The ordinary
        // token match is case-sensitive, so a fallback that folded case would refuse a line its
        // own primary path lets straight through.
        "echo \"GH PR CREATE checklist: unterminated",
    ] {
        let (code, err) = f.bash(cmd);
        assert_eq!(code, 0, "{cmd} must pass through untouched: {err}");
    }
}

#[test]
fn an_unquoted_gh_pr_create_anywhere_in_the_line_is_treated_as_the_command() {
    let f = Fixture::new("overmatch");
    // Deliberate over-match, not an oversight. Requiring `gh` at the head of its segment would
    // let every prefix spelling through — `timeout 60 gh pr create`, `nix develop -c gh pr
    // create` — and the wrapper trick is exactly how a guard in this repo has been bypassed
    // before (see hooks/block-nix-wrap-gh.sh). Three consecutive UNQUOTED words is the cost:
    // a mention inside quotes is a single token and passes (see the test above).
    let (code, err) = f.bash("echo gh pr create");
    assert_blocked(code, &err);
}

#[test]
fn a_non_bash_tool_call_is_ignored() {
    let f = Fixture::new("non-bash");
    for input in [
        serde_json::json!({"file_path": "/tmp/x.md", "content": "gh pr create --body no-qa"}),
        // A `command` key on a tool that is not Bash is not hypothetical — MCP tool inputs are
        // arbitrary JSON. Only Bash executes one, so anything else is a string, not a PR.
        serde_json::json!({"command": "gh pr create --title t --body no-qa"}),
    ] {
        let (code, err) = f.payload(&serde_json::json!({
            "tool_name": "mcp__fsm__clone_create",
            "tool_input": input,
        }));
        assert_eq!(code, 0, "only Bash opens a PR: {err}");
    }
}

#[test]
fn an_unparseable_payload_is_not_a_block() {
    let (code, err) = run_gate(b"{not json, but it does mention create}", &[]);
    assert_eq!(
        code, 0,
        "a payload the harness never sends must not wedge every Bash call: {err}"
    );
}

// --- the scope decision, pinned ---------------------------------------------------------------

#[test]
fn the_gate_is_not_scoped_to_the_cron() {
    let f = Fixture::new("scope");
    let bare = f.body_file("bare.md", "Closes #1\n");
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "cwd": f.dir.display().to_string(),
        "tool_input": {"command": open_pr(&bare)},
    });
    // The two hooks left in `hooks/` return early unless RAINIX_CRON_HOOK is set. This gate must
    // not: the PRs that leaked (#83) were opened by sessions the cron env never touched, and the
    // cron producer is the population that was already complying.
    for env in [vec![], vec![("RAINIX_CRON_HOOK", "1")]] {
        let (code, err) = f.payload_env(&payload, &env);
        assert_blocked(code, &err);
    }
}
