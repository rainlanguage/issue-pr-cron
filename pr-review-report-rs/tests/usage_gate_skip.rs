//! Behavioural tests for the runners' usage-gate paths (#160) and the one-off manual force (#245).
//!
//! A gate PAUSE (exit 10) must leave one skip row in `metrics/runs.jsonl` — before this, a paused
//! stretch left nothing at all and the dashboard drew nine consecutive gated ticks as a dead
//! cron. A config REFUSAL (exit 2) must stay a loud abort that writes NO row: dressing broken
//! config up as pacing is the conflation the typed fields exist to prevent. `--force` (#245) turns
//! the PAUSE — and ONLY the pause — into a run, marks the row it writes, and streams the trail to
//! stdout. All of it lives in the SHELL of `campaign-run.sh` / `review-run.sh`, which no unit test
//! in the binary can see, so these drive the real scripts as processes.
//!
//! What is stubbed, and why each one is not the thing under test:
//!
//!   * `usage-gate` — its verdict is the fixture's INPUT. The gate is inert when it cannot read
//!     usage (it prints OK and the tick runs), so a test that let the real gate decide would
//!     exercise the inert path and never the pause the force exists for.
//!   * `preflight` — a probe of the BOX (gh's token scopes, whether nix can realise a Solidity
//!     shell). Its answer is about the machine the test happens to run on, not about forcing.
//!   * `claude` and `jq` — only in the two whole-run fixtures. `claude` stands in for the model
//!     (which would cost money and need credentials); `jq` builds the worker brief, whose bytes go
//!     straight to the stubbed `claude` and are read by nothing this file asserts on.
//!
//! Everything else is the REAL binary cargo just built — `run-metrics` above all, because the
//! row's shape is exactly what must not be asserted against a stub's belief: "there is no second
//! place that knows what a runs.jsonl line looks like" is the invariant under test.
//!
//! Like `refresh_human_queue.rs`, these return early when the checkout is absent (the nix build
//! sandbox filters the scripts out of the crate's source); the `rainix-rs-test` gate runs against
//! a full checkout, which is where they execute. Six of them return early on a second condition
//! too — see [`runner_runtime_available`].

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// The binary cargo just built. Never a PATH lookup — that would test whatever
/// `pr-review-report` happens to be installed on the box.
const REAL_BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

/// The gate's real ceiling-pause line, verbatim — the fixture feeds it through the runner's
/// `$_ug` capture and the row must return it byte-identical.
const PAUSE_LINE: &str =
    "PAUSE: 91% of the weekly budget used (endpoint) — at/over the 90% ceiling";

/// The gate's real RUN line. Used by the unforced whole-run fixtures, which need the gate to say
/// yes so that what they measure is the absence of a force rather than a pause.
const OK_LINE: &str = "OK: 41% used vs 63% linear-by-now toward reset 2026-08-14T00:00:00Z \
                       (endpoint) — at least 5% behind pace";

/// The refusal's first line (the real one is longer; one line is enough to prove it is carried).
const REFUSE_LINE: &str = "REFUSED: USAGE_SLACK_PCT=3 is set, but that knob is retired (#158)";

/// The repo root, one level up from the crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate always has a parent directory")
        .to_path_buf()
}

/// Everything that differs between the two runners, in one place.
///
/// The force must behave IDENTICALLY in both — "a force that works on the producer and silently
/// does nothing on the vetter is worse than none" — so every property below is asserted against
/// this table rather than against one script, and a role-shaped divergence fails the pair.
struct Role {
    /// The `role` field on a runs.jsonl row.
    name: &'static str,
    script: &'static str,
    /// The install-dir file each runner probes for before doing anything else.
    prompt: &'static str,
    /// The kill switch. Deliberately DIFFERENT files: the two crons stop independently.
    disabled: &'static str,
    lock: &'static str,
    log: &'static str,
    /// The word the runner logs when it declines for the lock.
    lock_skip: &'static str,
}

const PRODUCER: Role = Role {
    name: "producer",
    script: "campaign-run.sh",
    prompt: "campaign-prompt.txt",
    disabled: "DISABLED",
    lock: "campaign.lock",
    log: "campaign.log",
    lock_skip: "previous run still holding the lock",
};

const VETTER: Role = Role {
    name: "vetter",
    script: "review-run.sh",
    prompt: "review-prompt.txt",
    disabled: "review-DISABLED",
    lock: "review.lock",
    log: "review.log",
    lock_skip: "previous review run still holding the lock",
};

struct Fixture {
    root: PathBuf,
    install: PathBuf,
    script: PathBuf,
    role: &'static Role,
}

impl Fixture {
    /// `None` when the checkout is not there (nix build sandbox) — enforced by the rs-test gate.
    fn new(role: &'static Role, name: &str, gate_rc: i32, gate_line: &str) -> Option<Self> {
        let script = repo_root().join(role.script);
        if !script.is_file() {
            return None;
        }
        let root = std::env::temp_dir()
            .join("usage-gate-skip-tests")
            .join(format!("{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&root);
        let install = root.join("install");
        std::fs::create_dir_all(&install).expect("create install dir");
        // The runner's install-dir probe. On a gated run the gate exits long before it is read; on
        // a whole-run fixture it IS read, substituted, and handed to the stubbed `claude`.
        std::fs::write(install.join(role.prompt), "prompt body\n").unwrap();
        // The producer refuses to start without a non-empty worker brief (#200).
        std::fs::write(install.join("campaign-worker-prompt.txt"), "worker brief\n").unwrap();

        // The stub: `usage-gate` answers with the fixture's verdict and `preflight` succeeds;
        // EVERYTHING else is the real binary, so the emitted row is the real contract and not this
        // test's opinion of it.
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("create stub bin dir");
        let stub = format!(
            "#!/usr/bin/env bash\n\
             set -uo pipefail\n\
             if [ \"${{1:-}}\" = usage-gate ]; then\n\
             \x20 printf '%s\\n' {line}\n\
             \x20 exit {rc}\n\
             fi\n\
             if [ \"${{1:-}}\" = preflight ]; then\n\
             \x20 echo 'preflight: every harness dependency resolved'\n\
             \x20 exit 0\n\
             fi\n\
             exec {real} \"$@\"\n",
            line = shell_quote(gate_line),
            rc = gate_rc,
            real = REAL_BIN,
        );
        write_exe(&bin.join("pr-review-report"), &stub);
        Some(Fixture {
            root,
            install,
            script,
            role,
        })
    }

    /// Add the two stubs a WHOLE run needs: the model, and the worker-brief builder.
    ///
    /// Kept off the gated fixtures on purpose — a test that proves the run stopped at the lock
    /// proves more when a model stub was standing right there and still never ran.
    fn with_model(self) -> Self {
        let bin = self.root.join("bin");
        // Two events: enough for `[ -s "$RUNLOG" ]`, for `trace-outcome` to classify `ok`, and for
        // `run-metrics` to emit a complete row.
        write_exe(
            &bin.join("claude"),
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \
             '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"forced\"}' \
             '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\
             \"duration_ms\":1200,\"total_cost_usd\":0.02,\"usage\":{\"input_tokens\":100,\
             \"output_tokens\":20,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}'\n",
        );
        // The producer's `--agents` brief. Its content reaches only the stubbed model.
        write_exe(
            &bin.join("jq"),
            "#!/usr/bin/env bash\n\
             printf '%s\\n' '{\"pr-worker\":{\"description\":\"stub\",\"prompt\":\"stub\"}}'\n",
        );
        self
    }

    fn write_install(&self, name: &str, content: &str) {
        std::fs::write(self.install.join(name), content).expect("write install file");
    }

    /// Run one cron tick. `bash -euo pipefail` is the prelude `writeShellApplication` puts above
    /// the script text, so the shell options match the packaged runner exactly.
    fn tick(&self) -> Output {
        self.tick_with(&[], &[])
    }

    fn tick_forced(&self) -> Output {
        self.tick_with(&["--force"], &[])
    }

    fn tick_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("bash");
        cmd.args(["-euo", "pipefail"])
            .arg(&self.script)
            .args(args)
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("CRON_DIR", &self.install)
            // Never inherited: whoever runs `cargo test` must not be able to make the force guard's
            // tests pass or fail from their own shell.
            .env_remove("CRON_FORCE");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("run the runner script")
    }

    /// Take the runner's own flock and hold it until the returned child is killed.
    ///
    /// The fd is opened by a shell that then `exec`s `sleep`: an fd without CLOEXEC survives the
    /// exec, so the lock is genuinely held by a live process rather than by a shell that has
    /// already gone. Returns only once the holder has confirmed acquisition, so the runner under
    /// test cannot win a race with it.
    fn hold_lock(&self) -> Child {
        let lock = self.install.join(self.role.lock);
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "exec 9>{lock}; flock -n 9 || exit 9; echo held; exec sleep 300",
                lock = shell_quote(lock.to_str().expect("utf-8 lock path")),
            ))
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the lock holder");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("read the holder's confirmation");
        assert_eq!(
            line.trim(),
            "held",
            "the lock holder never acquired the lock, so this test would prove nothing"
        );
        child
    }

    fn runs_jsonl(&self) -> PathBuf {
        self.install.join("metrics/runs.jsonl")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.install.join(self.role.log)).unwrap_or_default()
    }

    /// Every parsed row, in file order.
    fn rows(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.runs_jsonl())
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("every row must be one valid JSON object"))
            .collect()
    }

    /// The run's `stage: final` row — the one the dashboard's run series is built from. A whole
    /// run also writes `boot`/`usage` rows from inside the live pipe, so the stage is selected
    /// rather than assumed.
    fn final_row(&self) -> serde_json::Value {
        self.rows()
            .into_iter()
            .rev()
            .find(|r| r["stage"] == "final")
            .expect("the run must leave a stage:final row")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Single-quote a string for embedding in a generated script.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The tools a test needs in order to drive the real script's real behaviour: `flock`
/// (util-linux) and `timeout` (coreutils).
///
/// Both are in the runners' `runtimeInputs`, so in production they always resolve — the CI job
/// `each runner's closure is self-contained` asserts exactly that. A test, though, runs against the
/// AMBIENT path, and a bare macOS runner supplies neither. Neither can be stood in for: stub
/// `flock` and the lock test asserts a coincidence instead of the lock (worse, the script's own
/// `! flock -n 9` succeeds on a missing binary, so the skip message appears for the wrong reason),
/// and with no `timeout` the model invocation never runs, so there is no row to assert on. So where
/// the platform cannot supply the runner's own runtime, these return rather than prove something
/// else. The Linux job — the platform the crons actually run on — executes every one of them.
fn runner_runtime_available() -> bool {
    ["flock", "timeout"].iter().all(|t| {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {t} >/dev/null"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    }
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
    let Some(f) = Fixture::new(&PRODUCER, "producer-pause", 10, PAUSE_LINE) else {
        return;
    };
    let out = f.tick();
    assert_skip_row(&f, &out, "producer");
}

#[test]
fn a_paused_vetter_tick_writes_one_skip_row_and_exits_zero() {
    let Some(f) = Fixture::new(&VETTER, "vetter-pause", 10, PAUSE_LINE) else {
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
    let Some(f) = Fixture::new(&PRODUCER, "producer-refuse", 2, REFUSE_LINE) else {
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
    let Some(f) = Fixture::new(&VETTER, "vetter-refuse", 2, REFUSE_LINE) else {
        return;
    };
    let out = f.tick();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        !f.runs_jsonl().exists(),
        "a refusal writes NO runs.jsonl row: it is a loud abort, not pacing"
    );
}

// ---------------------------------------------------------------------------------------------
// The one-off manual force (#245).
//
// The whole design is in what `--force` does and does NOT bypass, so each guarantee gets a test
// that fails if the bypass widens by one step. Every one of them drives a gate that positively
// decided to PAUSE — the inert path (gate cannot read usage, prints OK, tick runs) never exercises
// the force at all.
// ---------------------------------------------------------------------------------------------

/// The feature itself: a forced tick runs past the PAUSE, and the row it leaves says so.
///
/// This is one test rather than three because the three claims are one behaviour: the run happened
/// (there is a `final` row over a non-empty trace, and no skip row), it is marked as forced with
/// the gate's own line, and it was watched (the trail reached stdout, not only the log).
fn a_forced_run_past_a_pause_runs_marked_and_streamed(role: &'static Role, name: &str) {
    if !runner_runtime_available() {
        return;
    }
    let Some(f) = Fixture::new(role, name, 10, PAUSE_LINE).map(Fixture::with_model) else {
        return;
    };
    let out = f.tick_forced();
    assert!(
        out.status.success(),
        "the forced run must complete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = f.final_row();
    assert_eq!(
        row["forced"], "usage-gate",
        "the row must name the gate the run was forced past"
    );
    assert_eq!(
        row["forceReason"], PAUSE_LINE,
        "the gate's own line, verbatim — this row is the only durable copy of what it said"
    );
    assert!(
        row.get("skipped").is_none(),
        "a forced run RAN: it is the opposite of a skip, never both"
    );
    assert_eq!(row["role"], role.name);
    assert_eq!(
        row["outcome"], "ok",
        "forcing marks how the run was started; it must not touch how the run is judged"
    );
    let trace = row["trace"].as_str().expect("trace present");
    assert!(
        std::fs::metadata(trace)
            .expect("the trace must exist")
            .len()
            > 0,
        "the forced run's trace carries the model's events — this is a run, not a skip"
    );
    // Live observability is the point of the feature: the same bytes the log gets also reach
    // stdout, as they are produced.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let log = f.log();
    for line in ["FORCED past the usage-gate PAUSE", "run START"] {
        assert!(
            stdout.contains(line),
            "a forced run streams its trail to stdout; {line:?} missing from: {stdout}"
        );
        assert!(
            log.contains(line),
            "streaming is ADDITIVE — the log must still get everything: {line:?} missing"
        );
    }
}

#[test]
fn a_forced_producer_run_past_a_pause_runs_marked_and_streamed() {
    a_forced_run_past_a_pause_runs_marked_and_streamed(&PRODUCER, "producer-forced");
}

#[test]
fn a_forced_vetter_run_past_a_pause_runs_marked_and_streamed() {
    a_forced_run_past_a_pause_runs_marked_and_streamed(&VETTER, "vetter-forced");
}

/// The other side of the same contract, and the reason the marker is worth anything: an ordinary
/// tick the gate let through is byte-identical to what it always was — no force fields, and not
/// one byte on stdout, which is what keeps `metrics/runs.jsonl` a record of the SCHEDULE.
fn an_unforced_run_is_unmarked_and_silent(role: &'static Role, name: &str) {
    if !runner_runtime_available() {
        return;
    }
    let Some(f) = Fixture::new(role, name, 0, OK_LINE).map(Fixture::with_model) else {
        return;
    };
    let out = f.tick();
    assert!(
        out.status.success(),
        "an un-gated tick runs: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = f.final_row();
    assert!(
        row.get("forced").is_none(),
        "forced must be ABSENT — not null, not false — on a tick the schedule produced"
    );
    assert!(row.get("forceReason").is_none(), "forceReason likewise");
    assert!(
        out.stdout.is_empty(),
        "a scheduled tick has no terminal and must write nothing to stdout, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        f.log().contains("run START"),
        "…while the log still receives the whole trail"
    );
}

#[test]
fn an_unforced_producer_run_is_unmarked_and_silent() {
    an_unforced_run_is_unmarked_and_silent(&PRODUCER, "producer-unforced");
}

#[test]
fn an_unforced_vetter_run_is_unmarked_and_silent() {
    an_unforced_run_is_unmarked_and_silent(&VETTER, "vetter-unforced");
}

/// GUARANTEE 1 — the force reaches exit 10 and nothing else.
///
/// A config REFUSAL means the gate could not read its config; forcing past that would run the
/// pipeline on config nobody validated, which is a different thing entirely from running past a
/// budget ceiling on purpose. A model stub is present and must still never be reached.
fn force_does_not_bypass_a_gate_refusal(role: &'static Role, name: &str) {
    let Some(f) = Fixture::new(role, name, 2, REFUSE_LINE).map(Fixture::with_model) else {
        return;
    };
    let out = f.tick_forced();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a refusal still propagates the gate's exit, forced or not"
    );
    assert!(
        !f.runs_jsonl().exists(),
        "a refusal writes NO row, forced or not — it is an abort, not a run"
    );
    assert!(
        f.log().contains("refused its config"),
        "the abort names the refusal: {}",
        f.log()
    );
}

#[test]
fn force_does_not_bypass_a_producer_gate_refusal() {
    force_does_not_bypass_a_gate_refusal(&PRODUCER, "producer-force-refuse");
}

#[test]
fn force_does_not_bypass_a_vetter_gate_refusal() {
    force_does_not_bypass_a_gate_refusal(&VETTER, "vetter-force-refuse");
}

/// GUARANTEE 2 — the DISABLED kill switch outranks the force.
///
/// The file is a deliberate stop with a human behind it. A force that walked through it would turn
/// the one unambiguous off-switch into a suggestion. The gate is set to PAUSE so that a force which
/// merely ran the gate early — rather than obeying the switch — would still be caught.
fn force_does_not_bypass_the_kill_switch(role: &'static Role, name: &str) {
    let Some(f) = Fixture::new(role, name, 10, PAUSE_LINE).map(Fixture::with_model) else {
        return;
    };
    f.write_install(role.disabled, "");
    let out = f.tick_forced();
    assert!(
        out.status.success(),
        "a disabled runner exits 0: the stop is deliberate, not an error"
    );
    assert!(
        !f.runs_jsonl().exists(),
        "a disabled runner writes no row at all — forced or not, it never got as far as the gate"
    );
    assert!(
        f.log().contains("SKIP:") && f.log().contains(role.disabled),
        "the log names the kill switch: {}",
        f.log()
    );
}

#[test]
fn force_does_not_bypass_the_producer_kill_switch() {
    force_does_not_bypass_the_kill_switch(&PRODUCER, "producer-force-disabled");
}

#[test]
fn force_does_not_bypass_the_vetter_kill_switch() {
    force_does_not_bypass_the_kill_switch(&VETTER, "vetter-force-disabled");
}

/// GUARANTEE 3 — the flock outranks the force.
///
/// Two runs of one role collide on the same clones and the same GitHub state, so the answer to "I
/// want to watch a run" is to watch the one already going. This also carries the positive half of
/// guarantee 1 in the same process: reaching the lock at all is only possible PAST the gate's
/// pause, so the absence of a skip row here is proof the force worked, and the lock message is
/// proof it stopped there.
fn force_does_not_bypass_the_lock(role: &'static Role, name: &str) {
    if !runner_runtime_available() {
        return;
    }
    let Some(f) = Fixture::new(role, name, 10, PAUSE_LINE).map(Fixture::with_model) else {
        return;
    };
    let mut holder = f.hold_lock();
    let out = f.tick_forced();
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(
        out.status.success(),
        "a lock skip exits 0, as it always did"
    );
    let log = f.log();
    assert!(
        log.contains(role.lock_skip),
        "the forced run skips with the EXISTING lock message: {log}"
    );
    assert!(
        log.contains("FORCED past the usage-gate PAUSE"),
        "…having got past the pause first, which is what makes this a lock test: {log}"
    );
    assert!(
        !f.runs_jsonl().exists(),
        "the forced run wrote no row: it neither skipped on the gate nor ran"
    );
}

#[test]
fn force_does_not_bypass_the_producer_lock() {
    force_does_not_bypass_the_lock(&PRODUCER, "producer-force-lock");
}

#[test]
fn force_does_not_bypass_the_vetter_lock() {
    force_does_not_bypass_the_lock(&VETTER, "vetter-force-lock");
}

/// GUARANTEE 4 — the force cannot be left switched on.
///
/// `CRON_FORCE` is the obvious wrong guess, and it is the shape that would be dangerous: a variable
/// in `cron.env` forces EVERY scheduled tick, silently and for ever. So it is REFUSED rather than
/// ignored — the posture `usage-gate` already takes toward the retired `USAGE_SLACK_PCT` — and
/// refused whether or not `--force` was also passed, because the point is that the setting must
/// surface, not that this particular invocation was legitimate.
fn a_force_variable_is_refused_however_it_arrives(role: &'static Role, name: &str) {
    let Some(f) = Fixture::new(role, name, 10, PAUSE_LINE).map(Fixture::with_model) else {
        return;
    };
    // The case that matters: a stale cron.env, sourced by a SCHEDULED tick with no argument.
    f.write_install("cron.env", "CRON_FORCE=1\n");
    let out = f.tick();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a cron.env force is a config refusal, not a run and not a skip"
    );
    assert!(
        !f.runs_jsonl().exists(),
        "a refused run writes no row — it never reached the gate"
    );
    assert!(
        f.log().contains("CRON_FORCE") && f.log().contains("REFUSED"),
        "the refusal names the variable and why it cannot be honoured: {}",
        f.log()
    );

    // And from the environment, which is how it would reach a crontab line.
    let out = f.tick_with(&[], &[("CRON_FORCE", "1")]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an exported CRON_FORCE is refused the same way"
    );
    // Passing --force as well does not make the stale setting acceptable.
    let out = f.tick_with(&["--force"], &[("CRON_FORCE", "1")]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "--force does not excuse a force VARIABLE: the setting is what must surface"
    );
}

/// …and the kill switch outranks the refusal, which is where the guard SITS.
///
/// `usage-gate`'s own refusal of the retired `USAGE_SLACK_PCT` has always sat after the kill
/// switch, so a config refusal has always been subordinate to a deliberate human stop. The
/// `CRON_FORCE` guard is written to the same precedent it cites. Order it the other way and a
/// paused pipeline with a stale `cron.env` exits 2 on every scheduled tick — recurring error noise
/// about a setting that cannot affect anything while the switch is on, from a cron somebody
/// deliberately turned off.
fn the_kill_switch_outranks_the_force_variable(role: &'static Role, name: &str) {
    let Some(f) = Fixture::new(role, name, 10, PAUSE_LINE) else {
        return;
    };
    f.write_install(role.disabled, "");
    f.write_install("cron.env", "CRON_FORCE=1\n");
    let out = f.tick();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a disabled runner exits 0 quietly; the stale setting is not news while it is off"
    );
    assert!(
        !f.runs_jsonl().exists(),
        "nothing ran and nothing was recorded"
    );
    let log = f.log();
    assert!(
        log.contains(role.disabled),
        "the log names the kill switch, which is the reason it stopped: {log}"
    );
    assert!(
        !log.contains("REFUSED"),
        "and says nothing about CRON_FORCE — the switch decided this, not the setting: {log}"
    );
}

#[test]
fn the_producer_kill_switch_outranks_the_force_variable() {
    the_kill_switch_outranks_the_force_variable(&PRODUCER, "producer-disabled-cron-force");
}

#[test]
fn the_vetter_kill_switch_outranks_the_force_variable() {
    the_kill_switch_outranks_the_force_variable(&VETTER, "vetter-disabled-cron-force");
}

#[test]
fn a_producer_force_variable_is_refused_however_it_arrives() {
    a_force_variable_is_refused_however_it_arrives(&PRODUCER, "producer-cron-force");
}

#[test]
fn a_vetter_force_variable_is_refused_however_it_arrives() {
    a_force_variable_is_refused_however_it_arrives(&VETTER, "vetter-cron-force");
}

/// An unrecognised argument is REFUSED, not ignored.
///
/// A typo'd flag that fell through would run the tick UNFORCED — and so skip on the very pause the
/// human invoked it to run past — while reporting nothing about why. That is the silent-degradation
/// shape this whole feature is written against, so it fails loudly instead.
fn an_unknown_argument_is_refused(role: &'static Role, name: &str) {
    let Some(f) = Fixture::new(role, name, 10, PAUSE_LINE).map(Fixture::with_model) else {
        return;
    };
    let out = f.tick_with(&["--frce"], &[]);
    assert_eq!(out.status.code(), Some(2), "an unknown argument aborts");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--force"),
        "the refusal says what the only argument is: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !f.runs_jsonl().exists(),
        "nothing ran and nothing was recorded — not even the skip a silent fall-through would leave"
    );
}

#[test]
fn an_unknown_producer_argument_is_refused() {
    an_unknown_argument_is_refused(&PRODUCER, "producer-bad-arg");
}

#[test]
fn an_unknown_vetter_argument_is_refused() {
    an_unknown_argument_is_refused(&VETTER, "vetter-bad-arg");
}
