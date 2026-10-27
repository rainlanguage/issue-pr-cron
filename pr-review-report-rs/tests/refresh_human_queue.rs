//! Behavioural tests for `refresh-human-queue.sh`.
//!
//! The script publishes the dashboard's data to the `dashboard-data` branch — `main` is
//! ruleset-protected, and a cron that committed its snapshots there froze the dashboard the day
//! the ruleset landed. What keeps it working is its relationship with the remote, which a static
//! read of the file cannot check, so these tests drive a real git remote and assert what actually
//! happened to both branches.
//!
//! `pr-review-report` itself is stubbed. The subject under test is the git choreography, and the
//! real binary needs `gh` auth and network; the stub reads its output from a file the test
//! controls, which also makes "the snapshot did not change" an exact condition rather than a race.
//!
//! These live in `tests/` rather than in `src/main.rs`'s `#[cfg(test)]` module because they need
//! no access to the binary's internals, and because the crate's unit tests are one large file.
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

const DATA_BRANCH: &str = "dashboard-data";

/// The seed every fixture's install dir starts with — and, unless a test says otherwise, exactly
/// what the published branch already carries, so an idle tick has nothing to publish.
const SEED_SNAPSHOT: &str = "{\"counts\":{\"a\":1}}\n";
const SEED_HISTORY: &str = "{\"ts\":\"2026-07-01T00:00:00Z\",\"counts\":{\"a\":1}}\n";
const SEED_LANDED: &str =
    "{\"ts\":\"2026-06-30T00:00:00Z\",\"observedAt\":\"2026-07-01T00:00:00Z\",\
                           \"kind\":\"pr\",\"repo\":\"o/seed\",\"number\":1}\n";
const SEED_RUNS: &str =
    "{\"runId\":\"20260701T000000Z\",\"role\":\"producer\",\"outcome\":\"ok\"}\n";

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
/// script reaches for (git, mktemp, date, tr, cmp) exists on both CI platforms.
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

fn write_exec(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
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
    /// An install whose remote already has a `dashboard-data` branch carrying the seed data.
    fn new(name: &str) -> Option<Self> {
        let f = Self::bare(name)?;
        f.seed_data_branch();
        Some(f)
    }

    /// An install whose remote has never had a `dashboard-data` branch.
    fn bare(name: &str) -> Option<Self> {
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
        // main carries code only. The data paths are ignored there, as in the real repo, so the
        // install's own tree never shows them.
        std::fs::write(
            f.install.join(".gitignore"),
            "/human-queue.json\n/human-queue-history.jsonl\n/landed-history.jsonl\n/metrics/\n\
             /fsm-touches.jsonl\n/.refresh-human-queue.lock\n",
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

        // The install dir's data files: the source of truth the tick publishes.
        std::fs::write(f.install.join("human-queue.json"), SEED_SNAPSHOT).unwrap();
        std::fs::write(f.install.join("human-queue-history.jsonl"), SEED_HISTORY).unwrap();
        std::fs::write(f.install.join("landed-history.jsonl"), SEED_LANDED).unwrap();
        std::fs::create_dir_all(f.install.join("metrics")).unwrap();
        std::fs::write(f.install.join("metrics/runs.jsonl"), SEED_RUNS).unwrap();

        f.set_next_snapshot(SEED_SNAPSHOT);
        f.write_stub();
        Some(f)
    }

    /// Publish the seed data to `dashboard-data` from the other clone, plus a file the tick does
    /// not own — which every publish must carry forward untouched.
    fn seed_data_branch(&self) {
        git(&self.other, &["switch", "--quiet", "--orphan", DATA_BRANCH]);
        for (rel, text) in [
            ("human-queue.json", SEED_SNAPSHOT),
            ("human-queue-history.jsonl", SEED_HISTORY),
            ("landed-history.jsonl", SEED_LANDED),
            ("metrics/runs.jsonl", SEED_RUNS),
            ("not-ours.txt", "kept\n"),
        ] {
            let p = self.other.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        git(&self.other, &["add", "-A"]);
        git(&self.other, &["commit", "--quiet", "-m", "seed data"]);
        git(&self.other, &["push", "--quiet", "origin", DATA_BRANCH]);
        git(&self.other, &["switch", "--quiet", "main"]);
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
             \x20 landed-history-lines)\n\
             \x20   # The dedup flag is what makes a replayed tick append-safe; a call without it\n\
             \x20   # is a script regression this stub refuses so the append tests catch it.\n\
             \x20   case \"$*\" in *--existing*) ;; *) echo 'stub: no --existing' >&2; exit 2 ;; esac\n\
             \x20   # The diff's FIRST operand must be the previous snapshot; recorded so a test can\n\
             \x20   # assert which snapshot the departures were taken against.\n\
             \x20   cp \"$2\" {prev}\n\
             \x20   [ -f {lines} ] && cat {lines}\n\
             \x20   exit \"$(cat {rc} 2>/dev/null || echo 0)\" ;;\n\
             \x20 *) echo \"stub: unexpected subcommand ${{1:-}}\" >&2; exit 2 ;;\n\
             esac\n",
            snap = self.root.join("snapshot.json").display(),
            prev = self.root.join("landed-prev-operand").display(),
            lines = self.root.join("landed-lines").display(),
            rc = self.root.join("landed-rc").display()
        );
        write_exec(&bin.join("pr-review-report"), &stub);
    }

    /// Move `main` on, behind the install's back — a PR merging.
    fn advance_main(&self) -> String {
        git(&self.other, &["pull", "--quiet", "--ff-only"]);
        let mut unrelated = std::fs::read_to_string(self.other.join("unrelated.txt")).unwrap();
        unrelated.push_str("a merged PR landed this\n");
        std::fs::write(self.other.join("unrelated.txt"), &unrelated).unwrap();
        git(
            &self.other,
            &["commit", "--quiet", "-am", "Merge pull request #999"],
        );
        git(&self.other, &["push", "--quiet", "origin", "main"]);
        git(&self.other, &["rev-parse", "HEAD"])
    }

    /// Land a commit on `dashboard-data` at push time, after the tick has read the tip. Fires once.
    /// `snapshot` also rewrites the snapshot on the branch, as a stale publisher would.
    fn race_the_data_push(&self, snapshot: Option<&str>) {
        let hook = format!(
            "#!/usr/bin/env bash\n\
             set -uo pipefail\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
             [ -e {marker} ] && exit 0\n\
             touch {marker}\n\
             git -C {other} fetch --quiet origin {branch} >/dev/null 2>&1\n\
             git -C {other} switch --quiet -C {branch} FETCH_HEAD >/dev/null 2>&1\n\
             printf 'raced\\n' > {other}/racer.txt\n\
             {snap}\
             git -C {other} add -A >/dev/null 2>&1\n\
             git -C {other} commit --quiet -m 'raced' >/dev/null 2>&1\n\
             git -C {other} push --quiet origin {branch} >/dev/null 2>&1\n\
             git -C {other} switch --quiet main >/dev/null 2>&1\n\
             exit 0\n",
            marker = self.root.join(".raced").display(),
            other = self.other.display(),
            branch = DATA_BRANCH,
            snap = match snapshot {
                Some(s) => format!(
                    "printf '%s' '{}' > {}/human-queue.json\n",
                    s,
                    self.other.display()
                ),
                None => String::new(),
            },
        );
        write_exec(&self.install.join(".git/hooks/pre-push"), &hook);
    }

    /// Every push from the install fails while this is in place — a remote that refuses.
    fn refuse_pushes(&self) {
        write_exec(
            &self.install.join(".git/hooks/pre-push"),
            "#!/usr/bin/env bash\nexit 1\n",
        );
    }

    fn accept_pushes(&self) {
        std::fs::remove_file(self.install.join(".git/hooks/pre-push")).expect("remove hook");
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

    fn origin_main(&self) -> String {
        git(&self.origin, &["rev-parse", "main"])
    }

    /// The remote's `dashboard-data` tip, or `None` while the branch does not exist.
    fn data_tip(&self) -> Option<String> {
        let r = format!("refs/heads/{DATA_BRANCH}");
        git_ok(&self.origin, &["rev-parse", "--verify", "--quiet", &r])
            .then(|| git(&self.origin, &["rev-parse", &r]))
    }

    /// A file as the remote's `dashboard-data` branch publishes it.
    fn published(&self, rel: &str) -> String {
        git(&self.origin, &["show", &format!("{DATA_BRANCH}:{rel}")])
    }

    fn published_has(&self, rel: &str) -> bool {
        git_ok(
            &self.origin,
            &["cat-file", "-e", &format!("{DATA_BRANCH}:{rel}")],
        )
    }

    fn local(&self, rel: &str) -> String {
        std::fs::read_to_string(self.install.join(rel)).unwrap()
    }

    fn append_local(&self, rel: &str, line: &str) {
        let mut text = self.local(rel);
        text.push_str(line);
        std::fs::write(self.install.join(rel), text).unwrap();
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// THE regression test for the frozen dashboard. A changed snapshot reaches the remote on
/// `dashboard-data`, as a fast-forward of the old tip, and `main` is not written at all — the
/// ruleset rejects every direct push there, which is what stranded hundreds of snapshot commits.
#[test]
fn a_changed_snapshot_is_published_to_the_data_branch_and_never_to_main() {
    let Some(f) = Fixture::new("publish") else {
        return;
    };
    let main_before = f.origin_main();
    let tip_before = f.data_tip().expect("seeded");
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));

    assert_eq!(f.origin_main(), main_before, "main must never be pushed");
    assert_eq!(
        f.install_head(),
        main_before,
        "nothing may be committed to the install's main"
    );
    let tip = f.data_tip().expect("still there");
    assert_eq!(
        git(&f.origin, &["rev-parse", &format!("{tip}^")]),
        tip_before,
        "the publish is ONE commit on top of the old tip — a plain fast-forward"
    );
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
    assert_eq!(
        f.published("human-queue-history.jsonl").lines().count(),
        2,
        "the new history line rides the same publish"
    );
    assert_eq!(
        f.published("not-ours.txt"),
        "kept",
        "the rest of the branch's tree is carried forward"
    );
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", DATA_BRANCH]),
        "chore(dashboard): refresh human-queue.json snapshot"
    );
}

/// The install's own index and working tree are never touched: the data files stay ignored and
/// unstaged, so nothing can ever block the code sync's fast-forward.
#[test]
fn a_publish_leaves_the_install_tree_clean() {
    let Some(f) = Fixture::new("clean") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(
        git(&f.install, &["status", "--porcelain"]),
        "",
        "the install tree must be clean after a publish"
    );
    assert_eq!(f.local("human-queue.json"), "{\"counts\":{\"a\":2}}\n");
}

/// The code sync still runs: an install behind `main` is fast-forwarded, so the flake the cron
/// runs from stays current — and the data publishes in the same tick.
#[test]
fn an_install_behind_main_is_fast_forwarded_and_still_publishes() {
    let Some(f) = Fixture::new("behind") else {
        return;
    };
    let remote_commit = f.advance_main();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(
        f.install_head(),
        remote_commit,
        "main must be fast-forwarded"
    );
    assert_eq!(f.origin_main(), remote_commit, "main must not be pushed");
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
}

/// The first publish creates the branch when the remote has none: every data file the install
/// holds lands on it, and a path the install has not produced yet is not invented.
#[test]
fn the_first_publish_creates_the_data_branch() {
    let Some(f) = Fixture::bare("first") else {
        return;
    };
    assert!(f.data_tip().is_none(), "setup: no branch yet");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(f.data_tip().is_some(), "the branch must be created");
    assert_eq!(f.published("human-queue.json"), SEED_SNAPSHOT.trim_end());
    assert_eq!(
        f.published("human-queue-history.jsonl"),
        SEED_HISTORY.trim_end()
    );
    assert_eq!(f.published("landed-history.jsonl"), SEED_LANDED.trim_end());
    assert_eq!(f.published("metrics/runs.jsonl"), SEED_RUNS.trim_end());
    assert!(
        !f.published_has("fsm-touches.jsonl"),
        "a ledger the install never wrote must not appear"
    );
    assert!(
        !f.published_has("unrelated.txt"),
        "the branch carries data, not main's code"
    );
}

/// The branch moves between the tick's read and its push — another publisher, or a human. The
/// rejected push is answered by rebuilding the install's files onto the new tip: the racer's
/// commit survives in history and its unrelated file in the tree, and the install's data wins
/// on the data paths, because the install dir is the source of truth.
#[test]
fn a_data_branch_that_moves_mid_tick_is_rebuilt_onto_and_republished() {
    let Some(f) = Fixture::new("race") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":3}}\n");
    f.race_the_data_push(Some("{\"counts\":{\"a\":31337}}"));

    let out = f.tick();
    assert!(
        out.status.success(),
        "a push lost to a race must be rebuilt onto the new tip: {}",
        stderr(&out)
    );
    let tip = f.data_tip().unwrap();
    let raced = git(&f.origin, &["rev-parse", &format!("{tip}^")]);
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", &raced]),
        "raced",
        "the publish must sit directly on the racer's commit"
    );
    assert_eq!(f.published("racer.txt"), "raced");
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":3}}");
    assert!(
        stderr(&out).contains("rebuilding onto it"),
        "the retry must be on the record: {}",
        stderr(&out)
    );
}

/// A diverged `main` is a human's problem — reported loudly, nothing merged, reset or forced —
/// but it no longer holds the dashboard hostage: the data still publishes, and THEN the tick
/// fails so the divergence is not mistaken for health.
#[test]
fn a_diverged_main_is_reported_and_fails_the_tick_but_the_data_still_publishes() {
    let Some(f) = Fixture::new("diverged") else {
        return;
    };
    let remote_commit = f.advance_main();
    git(
        &f.install,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "unpushed local work",
        ],
    );
    let head_before = f.install_head();
    f.set_next_snapshot("{\"counts\":{\"a\":8}}\n");

    let out = f.tick();
    assert!(
        !out.status.success(),
        "a diverged main must fail the tick, not pass silently"
    );
    let err = stderr(&out);
    assert!(err.contains("diverged"), "the log has to name it: {err}");
    assert_eq!(f.install_head(), head_before, "nothing may be rewritten");
    assert_eq!(f.origin_main(), remote_commit, "main may not be moved");
    assert_eq!(
        git(&f.install, &["rev-list", "--count", "--merges", "HEAD"]),
        "0",
        "no merge may be fabricated to paper over the divergence"
    );
    assert_eq!(
        f.published("human-queue.json"),
        "{\"counts\":{\"a\":8}}",
        "the dashboard must not freeze on a code-sync problem"
    );
}

/// The pause-visibility half of #160: a usage-gate skip row appended to `metrics/runs.jsonl` by a
/// gated runner tick must be published on the NEXT hourly refresh, even though the queue snapshot
/// did not move. The history ledger gains no line: one line per CHANGED snapshot.
#[test]
fn a_metrics_only_append_publishes_without_a_history_line() {
    let Some(f) = Fixture::new("metrics-only") else {
        return;
    };
    let tip_before = f.data_tip().unwrap();
    f.append_local(
        "metrics/runs.jsonl",
        "{\"runId\":\"20260731T090001Z\",\"role\":\"producer\",\"exitCode\":10,\
         \"outcome\":\"skipped\",\"skipped\":\"usage-gate\"}\n",
    );

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_ne!(
        f.data_tip().unwrap(),
        tip_before,
        "the skip row must publish"
    );
    assert!(f.published("metrics/runs.jsonl").contains("usage-gate"));
    assert_eq!(
        f.local("human-queue-history.jsonl").lines().count(),
        1,
        "an unchanged snapshot must not append a history line"
    );
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", DATA_BRANCH]),
        "chore(metrics): publish accrued run metrics"
    );
}

/// The touch ledger is published once it exists, under its own subject when it is the only mover.
#[test]
fn a_born_touch_ledger_is_published() {
    let Some(f) = Fixture::new("touches") else {
        return;
    };
    std::fs::write(
        f.install.join("fsm-touches.jsonl"),
        "{\"actor\":\"producer\",\"item\":\"o/r#1\"}\n",
    )
    .unwrap();

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(f.published("fsm-touches.jsonl").contains("o/r#1"));
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", DATA_BRANCH]),
        "chore(metrics): publish accrued fsm touch records"
    );
}

/// A tick whose push was refused leaves its data unpublished in the install dir. The next tick
/// publishes it — and does not append a second history line for the same snapshot move, because
/// "moved" is judged against the install's previous snapshot, not the stale branch.
#[test]
fn a_refused_publish_is_carried_by_the_next_tick_without_a_duplicate_history_line() {
    let Some(f) = Fixture::new("refused") else {
        return;
    };
    let tip_before = f.data_tip().unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.refuse_pushes();
    let refused = f.tick();
    assert!(
        !refused.status.success(),
        "a refused publish must fail the tick"
    );
    assert_eq!(
        f.data_tip().unwrap(),
        tip_before,
        "setup: nothing published"
    );
    assert_eq!(f.local("human-queue-history.jsonl").lines().count(), 2);

    f.accept_pushes();
    let out = f.tick(); // same snapshot again
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
    assert_eq!(
        f.published("human-queue-history.jsonl").lines().count(),
        2,
        "one history line per snapshot move, however many ticks it takes to publish"
    );
}

/// An unchanged snapshot and unchanged ledgers publish nothing and succeed.
#[test]
fn an_unchanged_snapshot_stays_a_no_op() {
    let Some(f) = Fixture::new("unchanged") else {
        return;
    };
    let tip_before = f.data_tip().unwrap();

    let out = f.tick();
    assert!(
        out.status.success(),
        "no-op tick must succeed: {}",
        stderr(&out)
    );
    assert_eq!(
        f.data_tip().unwrap(),
        tip_before,
        "an unchanged snapshot must not produce a commit"
    );
    assert_eq!(
        f.local("human-queue-history.jsonl").lines().count(),
        1,
        "an unchanged snapshot must not append a history line"
    );
}

/// The landed-history append (rain-org-health tokens-per-landed-item): a changed-snapshot tick
/// diffs the PREVIOUS snapshot against the fresh one, appends what it emitted, and the file is
/// published with the snapshot. The diff itself is the binary's (unit-tested); this is the
/// choreography.
#[test]
fn a_changed_snapshot_appends_and_publishes_landed_rows() {
    let Some(f) = Fixture::new("landed-append") else {
        return;
    };
    let row = "{\"ts\":\"2026-08-11T10:00:00Z\",\"observedAt\":\"stub\",\
               \"kind\":\"pr\",\"repo\":\"o/r\",\"number\":7}";
    std::fs::write(f.root.join("landed-lines"), format!("{row}\n")).unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(
        !stderr(&out).contains("landed-history incomplete"),
        "a clean landed diff must be recorded as one (the stub refuses a call without \
         --existing, so this also pins the dedup flag): {}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("landed-prev-operand")).unwrap(),
        SEED_SNAPSHOT,
        "the departures must be taken against the previous snapshot"
    );
    let published = f.published("landed-history.jsonl");
    assert!(
        published.contains("\"number\":7"),
        "the landed row must be published: {published}"
    );
    assert_eq!(
        published.lines().count(),
        2,
        "append-only: the seed row stays, the new row follows it"
    );
}

/// A partially-resolved landed diff (exit 3: an API failure left items unresolved) still
/// publishes the rows that DID resolve — withholding them would turn a healable gap into a
/// permanent under-count — and leaves a stamped trail naming the incompleteness.
#[test]
fn a_partial_landed_resolution_still_publishes_what_resolved() {
    let Some(f) = Fixture::new("landed-partial") else {
        return;
    };
    let row = "{\"ts\":\"2026-08-11T10:00:00Z\",\"observedAt\":\"stub\",\
               \"kind\":\"issue\",\"repo\":\"o/r\",\"number\":9}";
    std::fs::write(f.root.join("landed-lines"), format!("{row}\n")).unwrap();
    std::fs::write(f.root.join("landed-rc"), "3\n").unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":3}}\n");

    let out = f.tick();
    assert!(
        out.status.success(),
        "an incomplete landed diff must not fail the tick: {}",
        stderr(&out)
    );
    assert!(
        f.published("landed-history.jsonl").contains("\"number\":9"),
        "the resolved row must still be published"
    );
    assert!(
        stderr(&out).contains("landed-history incomplete"),
        "the incompleteness must be on the record: {}",
        stderr(&out)
    );
}

/// The landed diff is gated on the SNAPSHOT having moved, exactly as the history line is: two
/// identical snapshots hold identical tracked sets, so a metrics-only tick has no departure to
/// verify and must not spend API calls looking for one.
#[test]
fn a_metrics_only_tick_takes_no_landed_diff() {
    let Some(f) = Fixture::new("landed-metrics-only") else {
        return;
    };
    // A row the stub WOULD emit — proving the block never ran, not merely that it found nothing.
    let row = "{\"ts\":\"2026-08-11T10:00:00Z\",\"observedAt\":\"stub\",\
               \"kind\":\"pr\",\"repo\":\"o/r\",\"number\":8}";
    std::fs::write(f.root.join("landed-lines"), format!("{row}\n")).unwrap();
    f.append_local(
        "metrics/runs.jsonl",
        "{\"runId\":\"20260812T010001Z\",\"role\":\"producer\",\"exitCode\":10}\n",
    );

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(
        f.local("landed-history.jsonl").lines().count(),
        1,
        "a metrics-only tick must not take a landed diff"
    );
}

/// Every line the script emits carries a UTC stamp, so a two-day-old failure cannot be read as a
/// current one. Checked on real output, across a success and a failure, because "silent on
/// success" was half the problem.
#[test]
fn every_emitted_line_is_timestamped() {
    let Some(f) = Fixture::new("stamps") else {
        return;
    };

    f.advance_main();
    f.set_next_snapshot("{\"counts\":{\"a\":4}}\n");
    let ok = f.tick();
    assert!(ok.status.success(), "setup tick failed: {}", stderr(&ok));

    git(
        &f.install,
        &["commit", "--quiet", "--allow-empty", "-m", "unpushed"],
    );
    f.advance_main();
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
