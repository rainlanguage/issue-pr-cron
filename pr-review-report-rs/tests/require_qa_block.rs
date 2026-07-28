//! Behavioural tests for `hooks/require-qa-block.sh`.
//!
//! The hook is the producer-side half of QA-GUIDE.md section 8: a PR opened without the evidence
//! block is known-bad the moment it is opened, so it is refused at `gh pr create` rather than
//! discovered a queue round-trip later by the vetter (#83). What it guards is a CONTENT invariant
//! on a command line, and a command line is not something a static read of the script can judge —
//! the failure modes are all parsing (a chained command, a relative `--body-file`, a heading whose
//! section a subheading truncates). So every test here drives the real script with a real hook
//! payload and asserts what it actually did: the exit code the harness reads (0 allow, 2 block)
//! and the stderr the model reads.
//!
//! These live in `tests/` for the same reasons as `refresh_human_queue.rs`: they need nothing from
//! the binary's internals, and the crate's unit tests are one 14k-line file. In the nix build
//! sandbox the repo root is not part of the derivation's source (the fileset is the manifests plus
//! the crate), so the script is absent and every test returns early — `rainix-rs-test` runs `cargo
//! test` against a full checkout, which is where these execute.
//!
//! `python3` is asserted rather than tolerated on Linux: every hook this repo ships parses its
//! payload with it, so a Linux box without it is a broken environment, not a skip.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A section-8-complete QA block: all four lines, the shape the guide's template gives.
const COMPLETE_BLOCK: &str = "## QA\n\
     - Discriminating tests: t_guard_negated — fails on base (ran on the base checkout)\n\
     - Mutations applied: guard.rs:12 negate -> t_guard_negated\n\
     - Oracle: the issue's worked example, not the implementation\n\
     - Category check: issue asks A,B; covered A,B\n";

/// The repo root, one level up from the crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate always has a parent directory")
        .to_path_buf()
}

/// `None` when the checkout is not there (nix build sandbox) — enforced by the rs-test gate.
fn hook_under_test() -> Option<PathBuf> {
    let p = repo_root().join("hooks").join("require-qa-block.sh");
    p.is_file().then_some(p)
}

/// Is the hook's own runtime present? Only `python3` is in question; bash is on both CI platforms.
fn python3_available() -> bool {
    let have = Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(
        have || !cfg!(target_os = "linux"),
        "python3 is missing on a Linux host: every hook this repo ships parses its payload with \
         it, so this is a broken environment, not a test that may be skipped"
    );
    have
}

/// The hook plus a scratch directory it can be pointed at. `None` when there is nothing to drive.
struct Fixture {
    hook: PathBuf,
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Option<Self> {
        // Hook first: in the nix sandbox there is no checkout, and that is the reason to skip —
        // the python3 assertion must not fire there.
        let hook = hook_under_test()?;
        if !python3_available() {
            return None;
        }
        let dir = std::env::temp_dir()
            .join("require-qa-block-tests")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        Some(Self { hook, dir })
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

    /// Drive the hook with a Bash payload from the fixture dir. Returns (exit code, stderr).
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

    /// Drive the hook with a whole payload, so a non-Bash tool can be exercised too.
    fn payload(&self, payload: &serde_json::Value) -> (i32, String) {
        self.payload_env(payload, &[])
    }

    fn payload_env(&self, payload: &serde_json::Value, env: &[(&str, &str)]) -> (i32, String) {
        let mut child = Command::new(&self.hook)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the hook");
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("hook stdin")
                .write_all(payload.to_string().as_bytes())
                .expect("write the hook payload");
        }
        let out = child.wait_with_output().expect("hook exit");
        (
            out.status.code().expect("the hook always exits normally"),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }
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
    let Some(f) = Fixture::new("complete") else {
        return;
    };
    let body = f.body_file(
        "body.md",
        &format!("Closes #1\n\nA fix.\n\n{COMPLETE_BLOCK}"),
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn a_body_with_no_qa_heading_is_blocked() {
    let Some(f) = Fixture::new("no-heading") else {
        return;
    };
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
    let Some(f) = Fixture::new("incomplete") else {
        return;
    };
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n- Discriminating tests: t_one\n- Mutations applied: L1 -> flip -> t_one\n- Oracle: the issue\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_blocked(code, &err);
}

#[test]
fn the_refusal_names_the_lines_that_are_missing_and_the_ones_that_are_not() {
    let Some(f) = Fixture::new("names-missing") else {
        return;
    };
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
    let Some(f) = Fixture::new("one-liner") else {
        return;
    };
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
    let Some(f) = Fixture::new("one-per-line") else {
        return;
    };
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
fn the_refusal_prints_the_section_8_template() {
    let Some(f) = Fixture::new("template") else {
        return;
    };
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
    let Some(f) = Fixture::new("inline") else {
        return;
    };
    let (code, err) = f.bash(&format!(
        "gh pr create --title t --body \"Closes #1\n\n{COMPLETE_BLOCK}\""
    ));
    assert_allowed(code, &err);
    let (code, err) = f.bash("gh pr create --title t --body \"Closes #1\n\nno evidence\"");
    assert_blocked(code, &err);
}

#[test]
fn an_equals_form_body_flag_is_parsed() {
    let Some(f) = Fixture::new("equals") else {
        return;
    };
    let body = f.body_file("body.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!("gh pr create --title t --body-file={body}"));
    assert_allowed(code, &err);
    let bare = f.body_file("bare.md", "Closes #1\n");
    let (code, err) = f.bash(&format!("gh pr create --title t --body-file={bare}"));
    assert_blocked(code, &err);
}

// --- the QA section's extent ---------------------------------------------------------------

#[test]
fn a_subheading_does_not_truncate_the_qa_section() {
    let Some(f) = Fixture::new("subheading") else {
        return;
    };
    let body = f.body_file(
        "body.md",
        "Closes #1\n\n## QA\n\n### Discriminating tests\nt_one\n\n### Mutations applied\nL1 -> flip -> t_one\n\n### Oracle\nthe issue\n\n### Category check\nasks A; covered A\n",
    );
    let (code, err) = f.bash(&open_pr(&body));
    assert_allowed(code, &err);
}

#[test]
fn evidence_after_the_qa_section_ends_does_not_count() {
    let Some(f) = Fixture::new("outside") else {
        return;
    };
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

// --- bodies gh would build for itself, and bodies that cannot be read ----------------------

#[test]
fn a_pr_create_with_no_body_flag_is_blocked() {
    let Some(f) = Fixture::new("no-body") else {
        return;
    };
    let (code, err) = f.bash("gh pr create --assignee thedavidmeister --title t");
    assert_blocked(code, &err);
    assert!(
        err.contains("no body"),
        "the refusal must say the command supplied no body: {err}"
    );
}

#[test]
fn fill_cannot_substitute_for_the_block() {
    let Some(f) = Fixture::new("fill") else {
        return;
    };
    for flag in ["--fill", "-f", "--fill-first", "--template pr.md"] {
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
    let Some(f) = Fixture::new("unreadable") else {
        return;
    };
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
    // containing the word would satisfy the assertion below without the hook saying anything.
    let Some(f) = Fixture::new("dash-body") else {
        return;
    };
    let (code, err) = f.bash("gh pr create --title t --body-file -");
    assert_blocked(code, &err);
    assert!(
        err.contains("reads the body from stdin"),
        "the refusal must say why a stdin body cannot be checked: {err}"
    );
}

#[test]
fn a_body_file_relative_to_a_leading_cd_is_resolved() {
    let Some(f) = Fixture::new("relative-cd") else {
        return;
    };
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
    let Some(f) = Fixture::new("sequential-cd") else {
        return;
    };
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
fn a_body_file_relative_to_the_session_cwd_is_resolved() {
    let Some(f) = Fixture::new("relative-cwd") else {
        return;
    };
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
    let Some(f) = Fixture::new("chained") else {
        return;
    };
    let bare = f.body_file("bare.md", "Closes #1\n");
    let (code, err) = f.bash(&format!(
        "git -C /w push -q -u origin br && gh pr create -R o/r --head br --body-file {bare}"
    ));
    assert_blocked(code, &err);
}

#[test]
fn gh_reached_through_a_prefix_command_is_still_gated() {
    let Some(f) = Fixture::new("prefixed") else {
        return;
    };
    let (code, err) = f.bash("timeout 60 gh pr create --title t --body \"no evidence\"");
    assert_blocked(code, &err);
}

#[test]
fn an_interpreter_wrapped_pr_create_is_still_gated() {
    let Some(f) = Fixture::new("wrapped") else {
        return;
    };
    // shlex sees the script as ONE token, so there is no `gh` `pr` `create` word sequence to
    // find and the line would sail through — the wrapper bypass block-nix-wrap-gh.sh exists for.
    for cmd in [
        "bash -c 'gh pr create --title t --body \"no evidence\"'",
        "sh -c 'gh pr create --title t --body \"no evidence\"'",
        "nix shell nixpkgs#gh --command bash -c 'gh pr create --title t --body \"no evidence\"'",
    ] {
        let (code, err) = f.bash(cmd);
        assert_blocked(code, &err);
    }
}

#[test]
fn an_interpreter_wrapped_pr_create_with_the_block_opens() {
    let Some(f) = Fixture::new("wrapped-ok") else {
        return;
    };
    let body = f.body_file("body.md", COMPLETE_BLOCK);
    let (code, err) = f.bash(&format!(
        "bash -c 'gh pr create --title t --body-file {body}'"
    ));
    assert_allowed(code, &err);
}

#[test]
fn a_second_pr_create_in_one_command_is_not_missed() {
    let Some(f) = Fixture::new("second") else {
        return;
    };
    let good = f.body_file("good.md", COMPLETE_BLOCK);
    let bare = f.body_file("bare.md", "Closes #2\n");
    // Both separators, because they take different paths through the parser: `&&` becomes two
    // segments, while a newline is whitespace to shlex and leaves TWO invocations in ONE segment.
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
fn a_draft_pr_is_gated_too() {
    let Some(f) = Fixture::new("draft") else {
        return;
    };
    let bare = f.body_file("bare.md", "Closes #1\n");
    let (code, err) = f.bash(&format!(
        "gh pr create --draft --title t --body-file {bare}"
    ));
    assert_blocked(code, &err);
}

#[test]
fn an_unparseable_command_that_opens_a_pr_fails_closed() {
    let Some(f) = Fixture::new("unparseable") else {
        return;
    };
    let (code, err) = f.bash("gh pr create --title t --body \"unterminated");
    assert_blocked(code, &err);
    assert!(
        err.contains("could not parse"),
        "a parse failure must not become the way through: {err}"
    );
}

// --- everything the hook must keep its hands off --------------------------------------------

#[test]
fn commands_that_do_not_open_a_pr_pass_through() {
    let Some(f) = Fixture::new("passthrough") else {
        return;
    };
    for cmd in [
        "gh pr view 12 --json body",
        "gh issue create --title t --body \"no qa block here\"",
        "git commit -m \"create the release tag\"",
        "cargo test --all-features",
        "gh pr list --state open",
        "grep -rn \"gh pr create\" campaign-prompt.txt",
        "rg 'gh pr create' --files-with-matches",
    ] {
        let (code, err) = f.bash(cmd);
        assert_eq!(code, 0, "{cmd} must pass through untouched: {err}");
    }
}

#[test]
fn an_unquoted_gh_pr_create_anywhere_in_the_line_is_treated_as_the_command() {
    let Some(f) = Fixture::new("overmatch") else {
        return;
    };
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
    let Some(f) = Fixture::new("non-bash") else {
        return;
    };
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
    let Some(f) = Fixture::new("bad-payload") else {
        return;
    };
    let mut child = Command::new(&f.hook)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the hook");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("hook stdin")
            .write_all(b"{not json, but it does mention create}")
            .expect("write");
    }
    let out = child.wait_with_output().expect("hook exit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a payload the harness never sends must not wedge every Bash call"
    );
}

// --- the scope decision, pinned ---------------------------------------------------------------

#[test]
fn the_gate_is_not_scoped_to_the_cron() {
    let Some(f) = Fixture::new("scope") else {
        return;
    };
    let bare = f.body_file("bare.md", "Closes #1\n");
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "cwd": f.dir.display().to_string(),
        "tool_input": {"command": open_pr(&bare)},
    });
    // The other two hooks in this repo return early unless RAINIX_CRON_HOOK is set. This one must
    // not: the PRs that leaked (#83) were opened by sessions the cron env never touched, and the
    // cron producer is the population that was already complying.
    for env in [vec![], vec![("RAINIX_CRON_HOOK", "1")]] {
        let (code, err) = f.payload_env(&payload, &env);
        assert_blocked(code, &err);
    }
}
