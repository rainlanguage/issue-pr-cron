//! Behavioural tests for `refresh-human-queue.sh`.
//!
//! The script publishes the dashboard snapshot by committing straight to `main`, so the only
//! thing that keeps it working is its relationship with the remote — and that is not something
//! a static read of the file can check. Both defects these tests cover shipped green: the script
//! *contained* a `pull --ff-only`, it just ran after the working tree was already dirty, where
//! git refuses it. So the tests here drive a real git remote and assert what actually happened
//! to the commit graph.
//!
//! `pr-review-report` itself is stubbed. The subject under test is the git choreography, and the
//! real binary needs `gh` auth and network; the stub reads its output from a file the test
//! controls, which also makes "the snapshot did not change" an exact condition rather than a race.
//!
//! These live in `tests/` rather than in `src/main.rs`'s `#[cfg(test)]` module because they need
//! no access to the binary's internals, and because the crate's unit tests are one 12k-line file.
//!
//! In the nix build sandbox the repo root is not part of the derivation's source (the fileset is
//! the manifests plus the crate), so the script is absent and every test here returns early —
//! the same convention `read_text` uses in `src/main.rs`. The `rainix-rs-test` gate runs `cargo
//! test` against a full checkout, which is where these actually execute.
//!
//! `refresh-human-queue` is a Linux cron runner: it locks with `flock(1)`, which is util-linux and
//! is why the flake package lists `pkgs.util-linux`. macOS has no such binary, the script's very
//! first action there is to conclude another tick holds the lock, and there is nothing to drive.
//! So these return early where flock is absent too — but *only* there: on Linux a missing flock is
//! a broken environment rather than a reason to skip, so it is asserted instead of tolerated.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repo root, one level up from the crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate always has a parent directory")
        .to_path_buf()
}

/// `None` when the checkout is not there (nix build sandbox) — enforced by the rs-test gate.
fn script_under_test() -> Option<PathBuf> {
    let p = repo_root().join("refresh-human-queue.sh");
    p.is_file().then_some(p)
}

fn read_repo_file(rel: &str) -> Option<String> {
    std::fs::read_to_string(repo_root().join(rel)).ok()
}

/// Is the script's own runtime present? Only `flock(1)` is in question — everything else the
/// script reaches for (git, mktemp, date, tr) exists on both CI platforms.
fn flock_available() -> bool {
    let have = Command::new("flock")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        have || !cfg!(target_os = "linux"),
        "flock(1) is missing on a Linux host: refresh-human-queue.sh locks with it, so this is a \
         broken environment, not a test that may be skipped"
    );
    have
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(isolated_git_env(dir))
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}{}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Exit status only — for the queries whose failure is an answer (`merge-base --is-ancestor`).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(isolated_git_env(dir))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The fixture owns its git identity so a developer's (or CI's) global config, commit signing
/// and hooks cannot reach into the test.
fn isolated_git_env(dir: &Path) -> Vec<(String, String)> {
    let root = fixture_root_of(dir);
    vec![
        (
            "GIT_CONFIG_GLOBAL".into(),
            root.join("gitconfig").display().to_string(),
        ),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

/// Every fixture path lives under `<tmp>/refresh-human-queue-tests/<name>/…`, so the root is
/// recoverable from any path inside it.
fn fixture_root_of(dir: &Path) -> PathBuf {
    let mut p = dir;
    loop {
        if p.join("gitconfig").is_file() {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => return dir.to_path_buf(),
        }
    }
}

struct Fixture {
    root: PathBuf,
    origin: PathBuf,
    install: PathBuf,
    /// A second clone, used to move the remote behind the install's back.
    other: PathBuf,
    script: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Option<Self> {
        // Script first: in the nix sandbox there is no checkout AND no flock, and the missing
        // checkout is the reason to skip — the flock assertion must not fire there.
        let script = script_under_test()?;
        if !flock_available() {
            return None;
        }
        let root = std::env::temp_dir()
            .join("refresh-human-queue-tests")
            .join(format!("{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create fixture root");
        std::fs::write(
            root.join("gitconfig"),
            "[user]\n\tname = fixture\n\temail = fixture@example.invalid\n\
             [init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n",
        )
        .expect("write fixture gitconfig");

        let f = Fixture {
            origin: root.join("origin.git"),
            install: root.join("install"),
            other: root.join("other"),
            script,
            root,
        };

        git(&f.root, &["init", "--quiet", "--bare", "origin.git"]);
        git(
            &f.root,
            &["clone", "--quiet", f.origin.to_str().unwrap(), "install"],
        );
        std::fs::write(
            f.install.join("human-queue.json"),
            "{\"counts\":{\"a\":1}}\n",
        )
        .unwrap();
        std::fs::write(
            f.install.join("human-queue-history.jsonl"),
            "{\"ts\":\"2026-07-01T00:00:00Z\",\"counts\":{\"a\":1}}\n",
        )
        .unwrap();
        // The run-metrics ledger is TRACKED in the real repo, and the script publishes it
        // alongside the snapshot (#160) — the fixture mirrors that so `git add` has the same
        // tracked file to stage.
        std::fs::create_dir_all(f.install.join("metrics")).unwrap();
        std::fs::write(
            f.install.join("metrics/runs.jsonl"),
            "{\"runId\":\"20260701T000000Z\",\"role\":\"producer\",\"outcome\":\"ok\"}\n",
        )
        .unwrap();
        std::fs::write(f.install.join("unrelated.txt"), "seed\n").unwrap();
        git(&f.install, &["add", "-A"]);
        git(&f.install, &["commit", "--quiet", "-m", "seed"]);
        git(&f.install, &["push", "--quiet", "-u", "origin", "main"]);
        git(
            &f.root,
            &["clone", "--quiet", f.origin.to_str().unwrap(), "other"],
        );

        f.set_next_snapshot("{\"counts\":{\"a\":1}}\n");
        f.write_stub();
        Some(f)
    }

    /// What the stubbed `pr-review-report human-queue --json` will print on the next tick.
    fn set_next_snapshot(&self, json: &str) {
        std::fs::write(self.root.join("snapshot.json"), json).expect("write next snapshot");
    }

    fn write_stub(&self) {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).expect("create stub bin dir");
        let stub = format!(
            "#!/usr/bin/env bash\n\
             set -uo pipefail\n\
             case \"${{1:-}}\" in\n\
             \x20 human-queue) cat {snap} ;;\n\
             \x20 queue-history-line) printf '{{\"ts\":\"stub\",\"counts\":{{}}}}\\n' ;;\n\
             \x20 *) echo \"stub: unexpected subcommand ${{1:-}}\" >&2; exit 2 ;;\n\
             esac\n",
            snap = self.root.join("snapshot.json").display()
        );
        let path = bin.join("pr-review-report");
        std::fs::write(&path, stub).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
    }

    /// Move the remote on, behind the install's back. `touch_snapshot` reproduces the case the
    /// old code could not survive: the incoming commit edits the very file the tick rewrites, so
    /// a fast-forward attempted *after* the rewrite is refused.
    fn advance_remote(&self, touch_snapshot: bool) -> String {
        git(&self.other, &["pull", "--quiet", "--ff-only"]);
        let mut unrelated = std::fs::read_to_string(self.other.join("unrelated.txt")).unwrap();
        unrelated.push_str("a merged PR landed this\n");
        std::fs::write(self.other.join("unrelated.txt"), &unrelated).unwrap();
        if touch_snapshot {
            std::fs::write(
                self.other.join("human-queue.json"),
                "{\"counts\":{\"a\":999}}\n",
            )
            .unwrap();
        }
        git(
            &self.other,
            &["commit", "--quiet", "-am", "Merge pull request #999"],
        );
        git(&self.other, &["push", "--quiet", "origin", "main"]);
        git(&self.other, &["rev-parse", "HEAD"])
    }

    /// Land a commit on the remote at push time — a PR merging mid-tick, after the script has
    /// already synced. Fires once.
    fn race_the_push(&self, touch_snapshot: bool) {
        let hook = format!(
            "#!/usr/bin/env bash\n\
             set -uo pipefail\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
             [ -e {marker} ] && exit 0\n\
             touch {marker}\n\
             git -C {other} pull --quiet --ff-only >/dev/null 2>&1\n\
             printf 'raced\\n' >> {other}/unrelated.txt\n\
             {snap}\
             git -C {other} commit --quiet -am 'Merge pull request #1000' >/dev/null 2>&1\n\
             git -C {other} push --quiet origin main >/dev/null 2>&1\n\
             exit 0\n",
            marker = self.root.join(".raced").display(),
            other = self.other.display(),
            snap = if touch_snapshot {
                format!(
                    "printf '{{\"counts\":{{\"a\":31337}}}}\\n' > {}/human-queue.json\n",
                    self.other.display()
                )
            } else {
                String::new()
            },
        );
        let path = self.install.join(".git/hooks/pre-push");
        std::fs::write(&path, hook).expect("write pre-push hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod hook");
        }
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
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("run refresh-human-queue.sh")
    }

    fn install_head(&self) -> String {
        git(&self.install, &["rev-parse", "HEAD"])
    }

    fn origin_head(&self) -> String {
        git(&self.origin, &["rev-parse", "main"])
    }

    /// Merge commits between `from` and the install's HEAD. The recovery is only allowed to
    /// fast-forward or replay, never to synthesise a merge.
    fn merges_since(&self, from: &str) -> usize {
        git(
            &self.install,
            &["rev-list", "--count", "--merges", &format!("{from}..HEAD")],
        )
        .parse()
        .expect("rev-list --count prints a number")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// THE regression test for the push that could not recover.
///
/// The install is one commit behind a remote whose commit rewrote `human-queue.json`. Before the
/// fix the tick regenerated the snapshot first, so the `pull --ff-only` further down was refused
/// ("your local changes would be overwritten"), its error was discarded by `|| true`, and the
/// commit went onto the stale HEAD — a guaranteed non-fast-forward push, repeated identically
/// every tick after. Recovery is asserted on the commit graph, not on a push having been issued.
#[test]
fn a_tick_that_starts_behind_the_remote_recovers_and_publishes() {
    let Some(f) = Fixture::new("behind") else {
        return;
    };
    let remote_commit = f.advance_remote(true);
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(
        out.status.success(),
        "a tick that starts behind must recover on its own: {}",
        stderr(&out)
    );

    let head = f.install_head();
    assert_eq!(
        f.origin_head(),
        head,
        "the snapshot must reach the remote; the push is still stuck\n{}",
        stderr(&out)
    );
    assert!(
        git_ok(
            &f.install,
            &["merge-base", "--is-ancestor", &remote_commit, &head]
        ),
        "the remote's commit must be built on, never discarded"
    );
    assert_eq!(
        f.merges_since(&remote_commit),
        0,
        "recovery must fast-forward, not fabricate a merge"
    );
    assert!(
        std::fs::read_to_string(f.install.join("unrelated.txt"))
            .unwrap()
            .contains("a merged PR landed this"),
        "the remote's content must survive the recovery"
    );
    assert_eq!(
        std::fs::read_to_string(f.install.join("human-queue.json")).unwrap(),
        "{\"counts\":{\"a\":2}}\n",
        "the published snapshot must be the freshly generated one"
    );
}

/// Same recovery, driven the way it happens in production: several ticks in a row, each of which
/// found the remote had moved. The old code failed identically every time — "next tick retries"
/// was a claim about a retry that could not work.
#[test]
fn consecutive_ticks_each_recover_rather_than_repeating_a_rejected_push() {
    let Some(f) = Fixture::new("consecutive") else {
        return;
    };
    for i in 0..3 {
        f.advance_remote(true);
        f.set_next_snapshot(&format!("{{\"counts\":{{\"a\":{i}}}}}\n"));
        let out = f.tick();
        assert!(
            out.status.success(),
            "tick {i} did not recover: {}",
            stderr(&out)
        );
        assert_eq!(
            f.origin_head(),
            f.install_head(),
            "tick {i} left the snapshot unpublished: {}",
            stderr(&out)
        );
    }
}

/// The remote moves *during* the tick, after the sync and before the push — a PR merging while
/// the snapshot is being generated. Losing that race must not leave a commit stranded on a
/// diverged branch, because nothing downstream can undo that without a human.
#[test]
fn a_remote_that_moves_mid_tick_is_replayed_onto_and_republished() {
    let Some(f) = Fixture::new("race") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":3}}\n");
    f.race_the_push(false);

    let out = f.tick();
    assert!(
        out.status.success(),
        "a push lost to a mid-tick merge must be retried onto the new tip: {}",
        stderr(&out)
    );
    assert_eq!(
        f.origin_head(),
        f.install_head(),
        "the replayed snapshot must reach the remote\n{}",
        stderr(&out)
    );
    assert!(
        std::fs::read_to_string(f.install.join("unrelated.txt"))
            .unwrap()
            .contains("raced"),
        "the commit that won the race must survive the replay"
    );
}

/// A branch that has genuinely diverged is a human's problem. The tick must say so and stop —
/// not merge over it, not reset it away, not force past it.
#[test]
fn divergence_is_reported_loudly_and_nothing_is_merged_or_discarded() {
    let Some(f) = Fixture::new("diverged") else {
        return;
    };
    let remote_commit = f.advance_remote(false);
    // An unpushed local commit, as a lost push would leave behind.
    std::fs::write(
        f.install.join("human-queue.json"),
        "{\"counts\":{\"a\":7}}\n",
    )
    .unwrap();
    git(
        &f.install,
        &[
            "commit",
            "--quiet",
            "-am",
            "chore(dashboard): refresh human-queue.json snapshot",
        ],
    );
    let head_before = f.install_head();
    let origin_before = f.origin_head();
    f.set_next_snapshot("{\"counts\":{\"a\":8}}\n");

    let out = f.tick();
    assert!(
        !out.status.success(),
        "a diverged branch must fail the tick, not pass silently"
    );
    let err = stderr(&out);
    assert!(
        err.contains("diverged"),
        "the log has to name the condition; got: {err}"
    );
    assert_eq!(f.install_head(), head_before, "nothing may be rewritten");
    assert_eq!(
        f.origin_head(),
        origin_before,
        "the remote may not be moved"
    );
    assert!(
        git_ok(&f.install, &["cat-file", "-e", &remote_commit]),
        "the remote's commit must still be reachable"
    );
    assert_eq!(
        git(&f.install, &["rev-list", "--count", "--merges", "HEAD"]),
        "0",
        "no merge may be fabricated to paper over the divergence"
    );
}

/// The pause-visibility half of #160: a usage-gate skip row appended to `metrics/runs.jsonl` by a
/// gated runner tick must reach the remote on the NEXT hourly refresh, even though the queue
/// snapshot did not move — during a pause, nothing else runs to move it. Publishing is asserted
/// on the remote's own copy of the file; the history ledger must not gain a rollup line, because
/// its contract is one line per CHANGED snapshot and the snapshot did not change.
#[test]
fn a_metrics_only_append_publishes_without_a_history_line() {
    let Some(f) = Fixture::new("metrics-only") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":1}}\n"); // identical to the seed
    let skip_row = "{\"runId\":\"20260731T090001Z\",\"role\":\"producer\",\"exitCode\":10,\
                    \"outcome\":\"skipped\",\"skipped\":\"usage-gate\",\
                    \"skipReason\":\"PAUSE: 91% of the weekly budget used (endpoint)\"}\n";
    let mut runs = std::fs::read_to_string(f.install.join("metrics/runs.jsonl")).unwrap();
    runs.push_str(skip_row);
    std::fs::write(f.install.join("metrics/runs.jsonl"), &runs).unwrap();
    let head_before = f.install_head();

    let out = f.tick();
    assert!(
        out.status.success(),
        "a metrics-only tick must publish: {}",
        stderr(&out)
    );
    let head = f.install_head();
    assert_ne!(head, head_before, "the skip row must be committed");
    assert_eq!(
        f.origin_head(),
        head,
        "the skip row must reach the remote — visibility DURING the pause is the point\n{}",
        stderr(&out)
    );
    assert!(
        git(&f.install, &["show", "HEAD:metrics/runs.jsonl"]).contains("usage-gate"),
        "the committed ledger must carry the skip row"
    );
    assert_eq!(
        std::fs::read_to_string(f.install.join("human-queue-history.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "an unchanged snapshot must not append a history line, whatever the metrics did"
    );
}

/// The change probes compare against HEAD, not the index. A tick that staged its files and then
/// failed to commit (lock contention, a full disk) leaves the change STAGED — and `git diff`
/// without `HEAD` reads staged-only content as unchanged, so every later tick would skip the
/// publish and the skip rows would sit there until unrelated queue churn rescued them. Driven the
/// way the failure leaves the tree: the row already staged before the tick runs.
#[test]
fn a_staged_but_uncommitted_append_is_still_published() {
    let Some(f) = Fixture::new("staged-metrics") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":1}}\n"); // identical to the seed
    let skip_row = "{\"runId\":\"20260731T110001Z\",\"role\":\"vetter\",\"exitCode\":10,\
                    \"outcome\":\"skipped\",\"skipped\":\"usage-gate\",\
                    \"skipReason\":\"PAUSE: 91% of the weekly budget used (endpoint)\"}\n";
    let mut runs = std::fs::read_to_string(f.install.join("metrics/runs.jsonl")).unwrap();
    runs.push_str(skip_row);
    std::fs::write(f.install.join("metrics/runs.jsonl"), &runs).unwrap();
    git(&f.install, &["add", "metrics/runs.jsonl"]);
    let head_before = f.install_head();

    let out = f.tick();
    assert!(
        out.status.success(),
        "a staged-only change must still publish: {}",
        stderr(&out)
    );
    assert_ne!(
        f.install_head(),
        head_before,
        "a staged-only change must still be committed — the probe must compare against HEAD"
    );
    assert_eq!(
        f.origin_head(),
        f.install_head(),
        "the staged row must reach the remote\n{}",
        stderr(&out)
    );
    assert!(
        git(&f.install, &["show", "HEAD:metrics/runs.jsonl"]).contains("usage-gate"),
        "the committed ledger must carry the staged skip row"
    );
}

/// The early exit predates both fixes and has to keep holding: an unchanged snapshot commits
/// nothing, pushes nothing and succeeds — and an unchanged METRICS ledger (#160) is part of that
/// stillness: this fixture's runs.jsonl has no new rows, so nothing may publish.
#[test]
fn an_unchanged_snapshot_stays_a_no_op() {
    let Some(f) = Fixture::new("unchanged") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":1}}\n"); // identical to the seed
    let head_before = f.install_head();

    let out = f.tick();
    assert!(
        out.status.success(),
        "no-op tick must succeed: {}",
        stderr(&out)
    );
    assert_eq!(
        f.install_head(),
        head_before,
        "an unchanged snapshot must not produce a commit"
    );
    assert_eq!(
        std::fs::read_to_string(f.install.join("human-queue-history.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "an unchanged snapshot must not append a history line"
    );
}

/// Defect 2. Every line the script emits carries a UTC stamp, so a two-day-old failure cannot be
/// read as a current one — which is what happened during triage. Checked on real output, across
/// a success and a failure, because "silent on success" was half the problem.
#[test]
fn every_emitted_line_is_timestamped() {
    let Some(f) = Fixture::new("stamps") else {
        return;
    };

    // A success (publishes) and a failure (diverged) — both must be stamped, and the success
    // must actually say something.
    f.advance_remote(true);
    f.set_next_snapshot("{\"counts\":{\"a\":4}}\n");
    let ok = f.tick();
    assert!(ok.status.success(), "setup tick failed: {}", stderr(&ok));

    git(
        &f.install,
        &["commit", "--quiet", "--allow-empty", "-m", "unpushed"],
    );
    f.advance_remote(false);
    f.set_next_snapshot("{\"counts\":{\"a\":5}}\n");
    let bad = f.tick();
    assert!(!bad.status.success(), "setup: expected a diverged tick");

    for (label, out) in [("success", &ok), ("failure", &bad)] {
        let err = stderr(out);
        assert!(
            !err.trim().is_empty(),
            "the {label} path must leave a trail: the script was silent"
        );
        for line in err.lines().filter(|l| !l.trim().is_empty()) {
            assert!(
                is_stamped(line),
                "{label} line is not stamped `YYYY-MM-DDTHH:MM:SSZ `: {line:?}"
            );
        }
    }
}

/// `2026-07-27T13:40:02Z ` — the exact shape `date -u +%FT%TZ` produces.
fn is_stamped(line: &str) -> bool {
    let b = line.as_bytes();
    if b.len() < 21 {
        return false;
    }
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let dashes = [4, 7];
    digits.iter().all(|&i| b[i].is_ascii_digit())
        && dashes.iter().all(|&i| b[i] == b'-')
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z'
        && b[20] == b' '
}

/// The stamp is only useful if it is the SAME stamp the other two cron logs use — reading
/// `refresh-human-queue.log` next to `campaign.log` is the whole point. One format string,
/// asserted across all three producers rather than described in a comment.
#[test]
fn the_stamp_matches_campaign_log_and_review_log() {
    let Some(refresh) = read_repo_file("refresh-human-queue.sh") else {
        return;
    };
    for (file, text) in [
        ("refresh-human-queue.sh", Some(refresh)),
        ("campaign-run.sh", read_repo_file("campaign-run.sh")),
        ("review-run.sh", read_repo_file("review-run.sh")),
    ] {
        let Some(text) = text else { continue };
        assert!(
            text.contains("date -u +%FT%TZ"),
            "{file} must stamp its log lines with `date -u +%FT%TZ`, like the other cron logs"
        );
    }
}
