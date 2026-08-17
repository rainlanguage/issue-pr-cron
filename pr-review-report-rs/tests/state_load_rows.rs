//! #290 at the PROCESS boundary: the three things about `state-load`'s row selectors that no
//! in-binary test can reach, because each of them is a property of `main` or of the I/O wiring
//! rather than of a pure function.
//!
//!   1. **A REFUSED selector is an EXIT CODE.** `state_load_rows` returning `Err` is unit-tested to
//!      death, but the only thing that turns that refusal into a non-zero exit is one arm of
//!      `main`'s match — and a run's shell reads the code, not the message. `--action <typo>` is
//!      the LIVE route into it: clap does not validate the action string, so a typo reaches the
//!      resolver, and the resolver answers before any network call. Asserted with an EMPTY `PATH`,
//!      which is what makes "before any network call" a fact rather than a claim: had the refusal
//!      not fired, `gh` could not have spawned and the exit would be 1.
//!   2. **A SELECTOR PRINTS ROWS, NOT THE DIGEST.** The dispatch that routes one is a single
//!      `if let` in `state_load_mode` whose two arms are both I/O, so a mutation that deletes it
//!      prints the digest for every selector — the exact defect #290 exists to fix, wearing the
//!      fix's own output. Nothing pure can tell those two apart; only the process can.
//!   3. **A ROW CALL READS ONE HALF.** A fleet selector reads the fleet and never the org-wide
//!      issue search, which is what keeps the typed route cheaper than the `--json | jq` one it
//!      replaces. That is a claim about which calls are NOT made, so it is asserted by a `gh` that
//!      answers the fleet's two calls and refuses everything else.
//!
//! What is stubbed, and why it is not the thing under test: `gh` itself. What these tests hold the
//! binary to is which ANSWER it composes and which exit code it leaves behind — never what GitHub
//! says. The fleet the stub reports is EMPTY on purpose: an empty selection is an ANSWER (the count
//! line and the header), and it is the answer whose SHAPE differs most visibly from the digest's,
//! which is the discrimination these tests exist for. Row CONTENT is projected by pure functions
//! and pinned in `state_load_row_tests`.
//!
//! Every invocation sets `WORKLIST_CACHE`. The default is the live cron install's cache file, and a
//! test that writes there is a test that corrupts the running pipeline's state.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary cargo just built. Never a PATH lookup — that would test whatever
/// `pr-review-report` happens to be installed on the box.
const REAL_BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

/// The org scope every case runs under. A single made-up org, so the archived-repo enumeration is
/// one page and nothing here can be read as a statement about a real org.
const ORG: &str = "stub-org";

/// The `gh` a FLEET row call is allowed to make, and nothing else.
///
/// Two calls: the org-wide open-PR search, and the archived-repo enumeration each org half runs
/// before it will report rows at all (#206). Anything else — above all `gh search issues`, the
/// BACKLOG half — is a loud failure, because "a fleet row call does not pay for the backlog" is one
/// of the properties under test and an over-permissive stub would answer it silently.
///
/// `--include` is the tell for `gh api`'s two spellings: with it the binary expects an HTTP dump
/// and strips the head back off, so the head is written exactly as `gh` writes one — status line
/// terminated by a bare LF, header by CRLF, CRLFCRLF before the body.
const GH_STUB: &str = r#"#!/bin/sh
set -u
case "$1 ${2:-}" in
  'search prs')
    printf '%s\n' '[]'
    ;;
  'api --include')
    printf 'HTTP/2.0 200 OK\nContent-Type: application/json\r\n\r\n%s\n' \
      '{"data":{"repositoryOwner":{"repositories":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}'
    ;;
  *)
    printf 'gh stub: a fleet row call makes neither of these: gh %s\n' "$*" >&2
    exit 9
    ;;
esac
"#;

/// A scratch dir of this case's own — the cache it is redirected to, and the stub bin dir.
fn case_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .join("state-load-rows-tests")
        .join(format!("{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the case dir");
    root
}

/// Write [`GH_STUB`] into `dir/bin` and answer that dir, to be prepended to `PATH`.
fn gh_stub_dir(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("create the stub bin dir");
    let gh = bin.join("gh");
    std::fs::write(&gh, GH_STUB).expect("write the gh stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the gh stub");
    }
    bin
}

/// Run the real binary. `with_gh` puts [`GH_STUB`] on an otherwise EMPTY `PATH`; without it `PATH`
/// is empty outright. Either way nothing but the stub is reachable, so the two differ only in
/// whether the FLEET's own two calls can be answered at all.
fn run(name: &str, args: &[&str], with_gh: bool) -> Output {
    let dir = case_dir(name);
    let path = if with_gh {
        gh_stub_dir(&dir).to_string_lossy().into_owned()
    } else {
        String::new()
    };
    Command::new(REAL_BIN)
        .args(args)
        .env("ORGS", ORG)
        .env("PR_ASSIGNEE", "nobody")
        .env("PATH", path)
        // NEVER the default: that path is the live cron install's cache file.
        .env("WORKLIST_CACHE", dir.join("worklist-cache.json"))
        .output()
        .expect("the binary cargo just built must be runnable")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A row selector prints its ROWS. It is the whole of #290: before it, the row lists the digest
/// counts were reachable only by taking the ~95 KB `--json` document and re-slicing it. A selector
/// that printed the digest instead would be the defect, answering in the fix's own voice.
///
/// The two answers are told apart by SHAPE, not by content: rows lead with a `#` count line and a
/// `#` header, the digest leads with `fleet:` and ends by naming these very selectors.
#[test]
fn a_fleet_selector_prints_rows_and_never_the_digest() {
    let out = run("actionable", &["state-load", "--actionable"], true);
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        stderr(&out)
    );
    let text = stdout(&out);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        vec!["# 0 rows", "# action\tref\tci\tmerge\tcloses\ttitle"],
        "an empty selection is an ANSWER: the count line and the header"
    );
    assert!(
        !text.contains("fleet: ") && !text.contains("rows: state-load"),
        "a selector must not print the DIGEST — that is the defect wearing the fix's output: {text}"
    );

    // The escape hatch takes the selector too: `--json` narrows to the selected rows rather than
    // handing back the whole document.
    let js = run(
        "actionable-json",
        &["state-load", "--actionable", "--json"],
        true,
    );
    assert!(js.status.success(), "stderr: {}", stderr(&js));
    assert_eq!(stdout(&js).trim(), "[]");
}

/// A ROW CALL READS ONE HALF. `--audit` is the backlog's selector, so under a `gh` that answers
/// only the FLEET's calls it must FAIL rather than report rows: a backlog selector served out of
/// the fleet read would be answering from the wrong population, and one served an empty list would
/// be the falsely-empty answer every read in this binary aborts rather than give.
#[test]
fn a_backlog_selector_reads_the_backlog_and_a_fleet_read_cannot_serve_it() {
    let out = run("audit", &["state-load", "--audit"], true);
    assert_eq!(
        out.status.code(),
        Some(1),
        "`--audit` must abort when the ISSUE search is unavailable, not answer off the fleet — \
         stdout: {}, stderr: {}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).is_empty(),
        "an aborted half prints no rows at all: {}",
        stdout(&out)
    );
}

/// A REFUSED selector combination EXITS 2, and it does so before any network call.
///
/// `--action <typo>` is the live one: clap does not validate the string, so the resolver's refusal
/// is what the run meets. The rest are clap's own, asserted here only to pin that the process-level
/// answer to every unusable selector spelling is the same code — a run's shell branches on that
/// number and on nothing else.
///
/// `PATH` is EMPTY for all of them. Exit 2 therefore proves the refusal fired first: a call that
/// reached the fleet read would have failed to spawn `gh` and exited 1.
#[test]
fn a_refused_selector_exits_2_before_any_network_call() {
    let typo = run("typo", &["state-load", "--action", "no-such-action"], false);
    assert_eq!(
        typo.status.code(),
        Some(2),
        "stdout: {}, stderr: {}",
        stdout(&typo),
        stderr(&typo)
    );
    assert!(
        stdout(&typo).is_empty(),
        "a refusal answers on stderr and prints NO rows: {}",
        stdout(&typo)
    );
    let msg = stderr(&typo);
    assert!(
        msg.contains("no classifier produces that action") && msg.contains("rework-needs-work"),
        "the refusal must name the legal spellings — a bare `no` sends the run back to jq: {msg}"
    );

    for (name, args) in [
        (
            "two-selectors",
            vec!["state-load", "--actionable", "--approved"],
        ),
        (
            "action-and-audit",
            vec!["state-load", "--action", "needs-3b", "--audit"],
        ),
        ("limit-alone", vec!["state-load", "--limit", "5"]),
        (
            "zero-page",
            vec!["state-load", "--actionable", "--limit", "0"],
        ),
    ] {
        let out = run(name, &args, false);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} must be refused with 2 — stdout: {}, stderr: {}",
            stdout(&out),
            stderr(&out)
        );
    }
}
