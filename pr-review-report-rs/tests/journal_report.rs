//! Behavioural tests for `journal-report`: the subcommand as a PROCESS.
//!
//! The pure reading — recurrence, backlog, the denominator — is unit tested in `src/main.rs`'s
//! `journal_tests`. What only a process can demonstrate is the part a caller actually branches on:
//! the EXIT CODE. `journal-report` has three, and each one is a different claim about the journal:
//!
//!   * `2` — the file could not be read at all. An invocation problem.
//!   * `3` — the file was read and is DEFECTIVE, so no reading of it is trustworthy. This is the
//!     one that matters: a journal with a duplicate id or an unordered instant still parses into
//!     something that prints, and printing it would answer "has this recurred" wrongly rather than
//!     not at all. The refusal is the whole guarantee.
//!   * `0` — a report.
//!
//! A unit test over `journal_report_mode` would assert our own belief about what the CLI wires up;
//! these drive the real binary with real files.

/// The binary cargo just built for this test. Never a PATH lookup — that would test whatever
/// `pr-review-report` happens to be installed on the box.
const BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

/// A temp dir that removes itself. `std::env::temp_dir` plus the test's own name keeps concurrent
/// tests (cargo runs them threaded) out of each other's files.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("prr-journal-report-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        TempDir(p)
    }
    fn write(&self, name: &str, body: &str) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, body).expect("write fixture");
        p.to_string_lossy().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the subcommand, returning `(exit code, stdout, stderr)`.
fn run(args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(BIN)
        .arg("journal-report")
        .args(args)
        .output()
        .expect("run journal-report");
    (
        out.status.code().expect("an exit code"),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// One valid producer entry whose rule landed an hour after it.
const ENTRY: &str = r#"{"id":"LJ-0001","at":"2026-08-09T14:51:50Z","population":"producer","kind":"backgrounded-gate-strands-the-run","run":"20260809T145150Z","what":"stranded on a backgrounded gate and reported success","evidence":["result event: {\"subtype\":\"success\",\"num_turns\":5}"],"rule":{"landedAt":"2026-08-09T15:14:58Z","site":"campaign-prompt.txt","issue":"https://github.com/rainlanguage/issue-pr-cron/issues/249"}}"#;

/// A second incident of the same kind, AFTER the rule landed — a recurrence.
const RECURRENCE: &str = r#"{"id":"LJ-0002","at":"2026-08-09T18:00:00Z","population":"producer","kind":"backgrounded-gate-strands-the-run","run":"20260809T180000Z","what":"did it again","evidence":["did it again, verbatim"],"rule":null}"#;

/// An incident nothing addresses.
const BACKLOG: &str = r#"{"id":"LJ-0003","at":"2026-08-09T15:52:41Z","population":"producer","kind":"fact-asserted-without-reading-it","run":"20260809T155241Z","what":"asserted a state-load field into two briefs","evidence":["has3bAttempt was false on both rows"]}"#;

/// Two producer runs, both after the rule landed, neither a skip.
const RUNS: &str = concat!(
    r#"{"runId":"20260809T160000Z","role":"producer","outcome":"ok"}"#,
    "\n",
    r#"{"runId":"20260809T170000Z","role":"producer","outcome":"ok"}"#,
    "\n",
);

#[test]
fn a_journal_that_cannot_be_read_is_an_invocation_error() {
    let (code, _, err) = run(&["/nonexistent/mistake-journal.jsonl"]);
    assert_eq!(code, 2, "a missing journal is exit 2");
    assert!(err.contains("cannot read journal"), "{err}");
}

#[test]
fn a_denominator_that_cannot_be_read_is_an_invocation_error_and_not_a_silent_unknown() {
    let d = TempDir::new("bad-runs");
    let j = d.write("mistake-journal.jsonl", &format!("{ENTRY}\n"));
    let (code, out, err) = run(&[&j, "--runs", "/nonexistent/runs.jsonl"]);
    // Falling back to "unknown" would turn a typo into a report saying nothing is deletable, which
    // reads exactly like an answer.
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("cannot read runs metrics"), "{err}");
}

#[test]
fn a_defective_journal_refuses_the_report_rather_than_printing_a_wrong_one() {
    let d = TempDir::new("defective");
    // The SAME id twice: it parses, it prints, and every citation into it is a coin toss.
    let j = d.write("mistake-journal.jsonl", &format!("{ENTRY}\n{ENTRY}\n"));
    let (code, out, _) = run(&[&j]);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("appears more than once"), "{out}");
    assert!(out.contains("REFUSED"), "{out}");
    // …and nothing that looks like an answer is printed alongside the refusal.
    assert!(!out.contains("DELETE NEXT"), "{out}");
    assert!(!out.contains("NO RULE YET"), "{out}");
}

#[test]
fn a_defective_journal_refuses_in_json_too() {
    let d = TempDir::new("defective-json");
    let j = d.write("mistake-journal.jsonl", "{\"id\":\"nope\"}\n");
    let (code, out, _) = run(&[&j, "--json"]);
    assert_eq!(code, 3, "{out}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON object");
    assert!(
        v["defects"].as_array().is_some_and(|d| !d.is_empty()),
        "{out}"
    );
    assert!(v.get("verdict").is_none(), "no verdict on a refusal: {out}");
}

#[test]
fn the_report_answers_both_questions_and_names_its_denominator() {
    let d = TempDir::new("report");
    let j = d.write("mistake-journal.jsonl", &format!("{ENTRY}\n{BACKLOG}\n"));
    let runs = d.write("runs.jsonl", RUNS);

    // Without a denominator: the backlog is still answerable, the deletion question is not.
    let (code, out, _) = run(&[&j]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("fact-asserted-without-reading-it"), "{out}");
    assert!(
        out.contains("DELETE NEXT: nothing"),
        "silence over an unstated number of runs proposes nothing: {out}"
    );

    // With one: the same journal now has a deletion candidate, and the count is on the record.
    let (code, out, _) = run(&[&j, "--runs", &runs]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("DELETE NEXT: the inline reasoning under backgrounded-gate-strands-the-run"),
        "{out}"
    );
    assert!(out.contains("0 recurrences over 2 run(s) since"), "{out}");
    // The backlog does not go away because something became deletable.
    assert!(out.contains("NO RULE YET"), "{out}");
    assert!(out.contains("LJ-0003"), "{out}");
}

#[test]
fn a_recurrence_replaces_the_deletion_proposal_with_the_opposite_reading() {
    let d = TempDir::new("recurrence");
    let j = d.write("mistake-journal.jsonl", &format!("{ENTRY}\n{RECURRENCE}\n"));
    let runs = d.write("runs.jsonl", RUNS);
    let (code, out, _) = run(&[&j, "--runs", &runs]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("RECURRED SINCE ITS RULE LANDED"), "{out}");
    assert!(out.contains("tripped since by: LJ-0002"), "{out}");
    assert!(
        out.contains("DELETE NEXT: nothing"),
        "a tripped rule is evidence FOR keeping it: {out}"
    );
}

#[test]
fn the_json_carries_every_reading_the_table_does() {
    let d = TempDir::new("json");
    let j = d.write(
        "mistake-journal.jsonl",
        &format!("{ENTRY}\n{BACKLOG}\n{RECURRENCE}\n"),
    );
    let runs = d.write("runs.jsonl", RUNS);
    let (code, out, _) = run(&[&j, "--runs", &runs, "--json"]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON object");
    assert_eq!(v["entryCount"], 3);
    assert_eq!(v["runsCorpus"]["runsCounted"], 2);
    assert_eq!(v["verdict"]["deleteNext"], serde_json::Value::Null);
    assert_eq!(
        v["verdict"]["noRuleYet"],
        serde_json::json!(["fact-asserted-without-reading-it"])
    );
    assert_eq!(
        v["verdict"]["recurredSinceItsRule"],
        serde_json::json!(["backgrounded-gate-strands-the-run"])
    );
    let kinds = v["kinds"].as_array().expect("kinds");
    let strand = kinds
        .iter()
        .find(|k| k["kind"] == "backgrounded-gate-strands-the-run")
        .expect("the kind");
    assert_eq!(strand["recurrences"], serde_json::json!(["LJ-0002"]));
    assert_eq!(strand["runsSince"], 2);
    assert_eq!(
        strand["rule"]["issue"],
        "https://github.com/rainlanguage/issue-pr-cron/issues/249"
    );
}

#[test]
fn the_journal_path_defaults_to_the_one_canonical_file() {
    // Run with NO positional argument from a directory that has no journal: the refusal must name
    // the default, which is what proves the default is wired rather than merely documented.
    let d = TempDir::new("default");
    let out = std::process::Command::new(BIN)
        .arg("journal-report")
        .current_dir(&d.0)
        .output()
        .expect("run journal-report");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("cannot read journal mistake-journal.jsonl"),
        "{err}"
    );
}
