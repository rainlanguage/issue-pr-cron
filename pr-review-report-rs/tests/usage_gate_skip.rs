//! Behavioural tests for the runners' usage-gate paths (#160).
//!
//! A gate PAUSE (exit 10) must leave one skip row in `metrics/runs.jsonl` — before this, a paused
//! stretch left nothing at all and the dashboard drew nine consecutive gated ticks as a dead
//! cron. A config REFUSAL (exit 2) must stay a loud abort that writes NO row: dressing broken
//! config up as pacing is the conflation the typed fields exist to prevent. Both properties live
//! in the SHELL of `campaign-run.sh` / `review-run.sh`, which no unit test in the binary can see,
//! so these drive the real scripts as processes.
//!
//! Only `usage-gate` is stubbed — its verdict is the fixture's input. Every other subcommand,
//! `run-metrics` above all, is delegated to the REAL binary cargo just built, because the row's
//! shape is exactly what must not be asserted against a stub's belief: "there is no second place
//! that knows what a runs.jsonl line looks like" is the invariant under test.
//!
//! Like `refresh_human_queue.rs`, these return early when the checkout is absent (the nix build
//! sandbox filters the scripts out of the crate's source); the `rainix-rs-test` gate runs against
//! a full checkout, which is where they execute. Both gate paths exit before the runners' `flock`,
//! `timeout` or `claude` are ever reached, so nothing else needs stubbing or skipping.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary cargo just built. Never a PATH lookup — that would test whatever
/// `pr-review-report` happens to be installed on the box.
const REAL_BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

/// The gate's real ceiling-pause line, verbatim — the fixture feeds it through the runner's
/// `$_ug` capture and the row must return it byte-identical.
const PAUSE_LINE: &str =
    "PAUSE: 91% of the weekly budget used (endpoint) — at/over the 90% ceiling";

/// The refusal's first line (the real one is longer; one line is enough to prove it is carried).
const REFUSE_LINE: &str = "REFUSED: USAGE_SLACK_PCT=3 is set, but that knob is retired (#158)";

/// The repo root, one level up from the crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate always has a parent directory")
        .to_path_buf()
}

struct Fixture {
    root: PathBuf,
    install: PathBuf,
    script: PathBuf,
}

impl Fixture {
    /// `None` when the checkout is not there (nix build sandbox) — enforced by the rs-test gate.
    fn new(
        name: &str,
        script: &str,
        prompt_file: &str,
        gate_rc: i32,
        gate_line: &str,
    ) -> Option<Self> {
        let script = repo_root().join(script);
        if !script.is_file() {
            return None;
        }
        let root = std::env::temp_dir()
            .join("usage-gate-skip-tests")
            .join(format!("{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&root);
        let install = root.join("install");
        std::fs::create_dir_all(&install).expect("create install dir");
        // The runner's install-dir probe: any content will do, the gate exits long before it is read.
        std::fs::write(install.join(prompt_file), "prompt body (never reached)\n").unwrap();

        // The stub: `usage-gate` answers with the fixture's verdict; EVERYTHING else is the real
        // binary, so the emitted row is the real contract and not this test's opinion of it.
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("create stub bin dir");
        let stub = format!(
            "#!/usr/bin/env bash\n\
             set -uo pipefail\n\
             if [ \"${{1:-}}\" = usage-gate ]; then\n\
             \x20 printf '%s\\n' {line}\n\
             \x20 exit {rc}\n\
             fi\n\
             exec {real} \"$@\"\n",
            line = shell_quote(gate_line),
            rc = gate_rc,
            real = REAL_BIN,
        );
        let path = bin.join("pr-review-report");
        std::fs::write(&path, stub).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        Some(Fixture {
            root,
            install,
            script,
        })
    }

    /// Run one cron tick. `bash -euo pipefail` is the prelude `writeShellApplication` puts above
    /// the script text, so the shell options match the packaged runner exactly.
    fn tick(&self) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("bash")
            .args(["-euo", "pipefail"])
            .arg(&self.script)
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("CRON_DIR", &self.install)
            .output()
            .expect("run the runner script")
    }

    fn runs_jsonl(&self) -> PathBuf {
        self.install.join("metrics/runs.jsonl")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Single-quote a string for embedding in the stub script.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The one skip row a paused tick leaves, parsed. Asserts there is exactly one line.
fn the_only_row(f: &Fixture) -> serde_json::Value {
    let content = std::fs::read_to_string(f.runs_jsonl()).expect("metrics/runs.jsonl must exist");
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "a paused tick writes exactly one row, got: {content:?}"
    );
    serde_json::from_str(lines[0]).expect("the row must be one valid JSON object")
}

fn assert_skip_row(f: &Fixture, out: &Output, role: &str) {
    assert!(
        out.status.success(),
        "a paused tick still exits 0 — a pause is not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = the_only_row(f);
    assert_eq!(row["skipped"], "usage-gate");
    assert_eq!(
        row["skipReason"], PAUSE_LINE,
        "the reason must be the gate's own line, verbatim"
    );
    assert_eq!(row["role"], role);
    assert_eq!(row["model"], "claude-fable-5", "the runners' default model");
    assert_eq!(
        row["exitCode"], 10,
        "the row records the GATE's exit, like the preflight row records preflight's 12"
    );
    assert_eq!(row["outcome"], "skipped");
    assert_eq!(row["stage"], "final");
    let run_id = row["runId"].as_str().expect("runId present");
    assert_eq!(
        run_id.len(),
        "20260731T090001Z".len(),
        "runId is the runner's UTC stamp: {run_id}"
    );
    // The empty trace the row was distilled from exists where the row says it does — the same
    // relationship every other runs.jsonl row has with its trace.
    let trace = row["trace"].as_str().expect("trace present");
    let meta = std::fs::metadata(trace).expect("the empty trace file must exist");
    assert_eq!(meta.len(), 0, "the skip trace is EMPTY: no model ran");
}

#[test]
fn a_paused_producer_tick_writes_one_skip_row_and_exits_zero() {
    let Some(f) = Fixture::new(
        "producer-pause",
        "campaign-run.sh",
        "campaign-prompt.txt",
        10,
        PAUSE_LINE,
    ) else {
        return;
    };
    let out = f.tick();
    assert_skip_row(&f, &out, "producer");
}

#[test]
fn a_paused_vetter_tick_writes_one_skip_row_and_exits_zero() {
    let Some(f) = Fixture::new(
        "vetter-pause",
        "review-run.sh",
        "review-prompt.txt",
        10,
        PAUSE_LINE,
    ) else {
        return;
    };
    let out = f.tick();
    assert_skip_row(&f, &out, "vetter");
}

/// The conflation guard: a config REFUSAL is NOT a skip. The tick aborts with the gate's own
/// exit code and writes NO row — swapping the two branches (or widening the skip write to every
/// non-zero gate exit) fails here by producing a row, or a zero exit, or both.
#[test]
fn a_refused_producer_tick_aborts_loudly_and_writes_no_row() {
    let Some(f) = Fixture::new(
        "producer-refuse",
        "campaign-run.sh",
        "campaign-prompt.txt",
        2,
        REFUSE_LINE,
    ) else {
        return;
    };
    let out = f.tick();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a refusal propagates the gate's exit — the tick must not run on config the gate refused"
    );
    assert!(
        !f.runs_jsonl().exists(),
        "a refusal writes NO runs.jsonl row: it is a loud abort, not pacing"
    );
}

#[test]
fn a_refused_vetter_tick_aborts_loudly_and_writes_no_row() {
    let Some(f) = Fixture::new(
        "vetter-refuse",
        "review-run.sh",
        "review-prompt.txt",
        2,
        REFUSE_LINE,
    ) else {
        return;
    };
    let out = f.tick();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        !f.runs_jsonl().exists(),
        "a refusal writes NO runs.jsonl row: it is a loud abort, not pacing"
    );
}
