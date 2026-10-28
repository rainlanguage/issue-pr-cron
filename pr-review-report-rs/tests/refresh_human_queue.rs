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

use std::ffi::OsStr;
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
        std::fs::create_dir_all(root.join("tmp")).expect("create fixture root and its TMPDIR");
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
             \x20 human-queue) cat {snap}; exit \"$(cat {gen_rc} 2>/dev/null || echo 0)\" ;;\n\
             \x20 queue-history-line)\n\
             \x20   # Stdin form (the backfill): echo the snapshot it was fed, stamped with its --ts.\n\
             \x20   if [ \"${{2:-}}\" = --ts ]; then printf '{{\"ts\":\"%s\",\"snap\":%s}}\\n' \"$3\" \"$(cat)\"; exit 0; fi\n\
             \x20   rc=\"$(cat {hist_rc} 2>/dev/null || echo 0)\"; [ \"$rc\" -eq 0 ] || exit \"$rc\"\n\
             \x20   printf '{{\"ts\":\"stub\",\"counts\":{{}}}}\\n' ;;\n\
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
            gen_rc = self.root.join("generator-rc").display(),
            hist_rc = self.root.join("history-rc").display(),
            prev =self.root.join("landed-prev-operand").display(),
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

    /// At push time, another publisher lands exactly the install's data (plus a file of its own)
    /// on `dashboard-data`, so the rejected push finds its data already published. Fires once.
    fn race_with_our_own_data(&self) {
        let copies: String = [
            "human-queue.json",
            "human-queue-history.jsonl",
            "landed-history.jsonl",
            "metrics/runs.jsonl",
        ]
        .iter()
        .map(|rel| {
            format!(
                "cp {} {}/{rel}\n",
                self.install.join(rel).display(),
                self.other.display()
            )
        })
        .collect();
        let hook = format!(
            "#!/usr/bin/env bash\n\
             set -uo pipefail\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
             [ -e {marker} ] && exit 0\n\
             touch {marker}\n\
             git -C {other} fetch --quiet origin {branch} >/dev/null 2>&1\n\
             git -C {other} switch --quiet -C {branch} FETCH_HEAD >/dev/null 2>&1\n\
             mkdir -p {other}/metrics\n\
             {copies}\
             printf 'raced\\n' > {other}/racer.txt\n\
             git -C {other} add -A >/dev/null 2>&1\n\
             git -C {other} commit --quiet -m 'raced' >/dev/null 2>&1\n\
             git -C {other} push --quiet origin {branch} >/dev/null 2>&1\n\
             git -C {other} switch --quiet main >/dev/null 2>&1\n\
             exit 0\n",
            marker = self.root.join(".raced").display(),
            other = self.other.display(),
            branch = DATA_BRANCH,
        );
        write_exec(&self.install.join(".git/hooks/pre-push"), &hook);
    }

    /// At push time, `dashboard-data` is replaced by an unrelated history — a force-push the tick
    /// did not see. Fires once.
    fn rewrite_the_data_branch_at_push(&self) {
        let hook = format!(
            "#!/usr/bin/env bash\n\
             set -euo pipefail\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
             [ -e {marker} ] && exit 0\n\
             touch {marker}\n\
             blob=\"$(printf 'rewritten\\n' | git -C {origin} hash-object -w --stdin)\"\n\
             tree=\"$(printf '100644 blob %s\\trewritten.txt\\n' \"$blob\" | git -C {origin} mktree)\"\n\
             c=\"$(git -C {origin} commit-tree \"$tree\" -m rewritten)\"\n\
             git -C {origin} update-ref refs/heads/{branch} \"$c\"\n\
             exit 0\n",
            marker = self.root.join(".rewritten").display(),
            origin = self.origin.display(),
            branch = DATA_BRANCH,
        );
        write_exec(&self.install.join(".git/hooks/pre-push"), &hook);
    }

    /// From its `from`-th invocation on, `git <subcommand>` run by the script fails as a broken
    /// network or repository would. The fixture's own git calls do not go through this.
    fn fail_git(&self, subcommand: &str, from: u32) {
        let real = Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("locate git");
        let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
        assert!(!real.is_empty(), "git is not on PATH");
        let wrapper = format!(
            "#!/usr/bin/env bash\n\
             for a in \"$@\"; do\n\
             \x20 if [ \"$a\" = {subcommand} ]; then\n\
             \x20   n=$(( $(cat {count} 2>/dev/null || echo 0) + 1 )); echo \"$n\" > {count}\n\
             \x20   if [ \"$n\" -ge {from} ]; then echo 'injected: git {subcommand} fails' >&2; exit 128; fi\n\
             \x20   break\n\
             \x20 fi\n\
             done\n\
             exec {real} \"$@\"\n",
            count = self.root.join(format!("git-calls-{subcommand}")).display(),
        );
        write_exec(&self.root.join("bin/git"), &wrapper);
    }

    /// `main` gains a remote commit AND an unpushed local one: a divergence only a human resolves.
    fn diverge_main(&self) {
        self.advance_main();
        git(
            &self.install,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "unpushed local work",
            ],
        );
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
        self.run(&[], &self.script)
    }

    /// One tick while another process holds the tick lock.
    fn tick_while_locked(&self) -> Output {
        let lock = self.install.join(".refresh-human-queue.lock");
        self.run(&[OsStr::new("flock"), lock.as_os_str()], &self.script)
    }

    /// A repo-root script run the way its flake runner would: the stub first on PATH, the install
    /// dir as CRON_DIR.
    fn run_repo_script(&self, name: &str) -> Output {
        self.run(&[], &repo_root().join(name))
    }

    fn run(&self, prefix: &[&OsStr], script: &Path) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let (program, rest) = match prefix.split_first() {
            Some((p, rest)) => (*p, rest),
            None => (OsStr::new("bash"), &[][..]),
        };
        let mut cmd = Command::new(program);
        cmd.args(rest);
        if !prefix.is_empty() {
            cmd.arg("bash");
        }
        cmd.args(["-euo", "pipefail"])
            .arg(script)
            .env("PATH", path)
            .env("HOME", &self.root)
            // The script's temp files land here, so a test can see one left behind.
            .env("TMPDIR", self.root.join("tmp"))
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
    for rel in [
        "human-queue.json",
        "human-queue-history.jsonl",
        "landed-history.jsonl",
        "metrics/runs.jsonl",
    ] {
        assert!(
            git(&f.origin, &["ls-tree", DATA_BRANCH, "--", rel]).starts_with("100644 blob "),
            "{rel} is published as a plain, non-executable file"
        );
    }
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
    assert_eq!(
        stderr(&refused)
            .lines()
            .filter(|l| l.contains(": git push ") && l.contains(" failed (rc="))
            .count(),
        3,
        "a refused publish is attempted exactly three times: {}",
        stderr(&refused)
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

/// Nothing a tick did is visible on the remote or in the install's data: the branch tip is where
/// it was and the install still holds the seed snapshot and history.
fn assert_untouched(f: &Fixture, tip_before: &Option<String>, out: &Output) {
    assert_eq!(
        &f.data_tip(),
        tip_before,
        "nothing may be published: {}",
        stderr(out)
    );
    assert_eq!(
        f.local("human-queue.json"),
        SEED_SNAPSHOT,
        "the snapshot must not be replaced"
    );
    assert_eq!(
        f.local("human-queue-history.jsonl"),
        SEED_HISTORY,
        "no history line may be appended"
    );
}

/// Overlapping ticks never stack: a tick that finds the lock held is a quiet, successful skip
/// that neither regenerates nor publishes.
#[test]
fn a_tick_that_finds_the_lock_held_skips() {
    let Some(f) = Fixture::new("locked") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick_while_locked();
    assert!(
        out.status.success(),
        "a skipped tick is not a failure: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("skipped: a previous tick still holds the lock"),
        "the skip must be on the record: {}",
        stderr(&out)
    );
    assert_untouched(&f, &tip_before, &out);
}

/// An install on a branch with no upstream has no remote to publish to: it says so and stops.
#[test]
fn an_install_without_an_upstream_stops_before_generating() {
    let Some(f) = Fixture::new("no-upstream") else {
        return;
    };
    let tip_before = f.data_tip();
    git(&f.install, &["switch", "--quiet", "-c", "local-only"]);
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(!out.status.success(), "no upstream must fail the tick");
    assert!(
        stderr(&out).contains("ERROR: branch 'local-only' has no upstream"),
        "the log has to name it: {}",
        stderr(&out)
    );
    assert_untouched(&f, &tip_before, &out);
}

/// A remote that cannot be fetched stops the tick before a snapshot is generated.
#[test]
fn an_unfetchable_remote_stops_before_generating() {
    let Some(f) = Fixture::new("unfetchable") else {
        return;
    };
    let tip_before = f.data_tip();
    let missing = f.root.join("missing.git");
    git(
        &f.install,
        &["remote", "set-url", "origin", missing.to_str().unwrap()],
    );
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(
        !out.status.success(),
        "an unfetchable remote must fail the tick"
    );
    assert!(
        stderr(&out).contains("ERROR: fetch failed; not generating a snapshot this tick"),
        "the log has to name it: {}",
        stderr(&out)
    );
    assert_untouched(&f, &tip_before, &out);
}

/// `main` vanishing from the remote leaves nothing to compare HEAD with: the code sync fails the
/// tick, but only after the data is published.
#[test]
fn an_uncomparable_main_fails_the_tick_after_publishing() {
    let Some(f) = Fixture::new("uncomparable") else {
        return;
    };
    git(&f.install, &["config", "fetch.prune", "true"]);
    git(&f.origin, &["update-ref", "-d", "refs/heads/main"]);
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(
        !out.status.success(),
        "an uncomparable main must fail the tick"
    );
    assert!(
        stderr(&out).contains("ERROR: cannot compare HEAD with origin/main"),
        "the log has to name it: {}",
        stderr(&out)
    );
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
}

/// A fast-forward git refuses (local edits in the way) fails the tick, after publishing, and
/// leaves the local edits and HEAD exactly as they were.
#[test]
fn a_refused_fast_forward_fails_the_tick_after_publishing() {
    let Some(f) = Fixture::new("ff-refused") else {
        return;
    };
    f.advance_main();
    let head_before = f.install_head();
    std::fs::write(f.install.join("unrelated.txt"), "a local edit\n").unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(
        !out.status.success(),
        "a failed fast-forward must fail the tick"
    );
    assert!(
        stderr(&out).contains("ERROR: fast-forward to origin/main failed"),
        "the log has to name it: {}",
        stderr(&out)
    );
    assert_eq!(f.install_head(), head_before, "HEAD must not move");
    assert_eq!(
        f.local("unrelated.txt"),
        "a local edit\n",
        "local edits are kept"
    );
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
}

/// A force-push to `dashboard-data` between the tick's read and its push is still a branch tip
/// to build on: the tick re-reads it and publishes on top of the rewritten history.
#[test]
fn a_data_branch_rewritten_mid_tick_is_published_onto() {
    let Some(f) = Fixture::new("rewritten") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.rewrite_the_data_branch_at_push();

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    let tip = f.data_tip().unwrap();
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", &format!("{tip}^")]),
        "rewritten",
        "the publish must sit on the rewritten tip"
    );
    assert_eq!(f.published("rewritten.txt"), "rewritten");
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
}

/// A remote whose `dashboard-data` cannot be read is not mistaken for one without the branch:
/// the tick stops before generating, and nothing is built or pushed.
#[test]
fn an_unreadable_data_branch_stops_the_tick() {
    let Some(f) = Fixture::new("unreadable") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.fail_git("ls-remote", 1);

    let out = f.tick();
    assert!(
        !out.status.success(),
        "an unreadable branch must fail the tick"
    );
    let err = stderr(&out);
    assert!(
        err.contains("ERROR: cannot read dashboard-data from origin"),
        "{err}"
    );
    assert!(!err.contains(" git push "), "nothing may be pushed: {err}");
    assert_untouched(&f, &tip_before, &out);
}

/// A rejected push whose follow-up read of the branch fails stops retrying at once.
#[test]
fn a_failed_re_read_after_a_rejected_push_stops_retrying() {
    let Some(f) = Fixture::new("re-read") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.refuse_pushes();
    f.fail_git("ls-remote", 2);

    let out = f.tick();
    assert!(!out.status.success(), "the tick must fail");
    let err = stderr(&out);
    assert!(
        err.contains("ERROR: re-reading dashboard-data after a rejected push failed"),
        "{err}"
    );
    assert!(
        !err.contains("attempt 1:"),
        "no rebuild may follow a failed re-read: {err}"
    );
    assert_eq!(f.data_tip(), tip_before);
}

/// A generator that exits non-zero is a failed generation even when it printed something.
#[test]
fn a_failing_generator_keeps_the_previous_snapshot() {
    let Some(f) = Fixture::new("gen-fails") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    std::fs::write(f.root.join("generator-rc"), "1\n").unwrap();

    let out = f.tick();
    assert!(
        !out.status.success(),
        "a failed generation must fail the tick"
    );
    assert!(
        stderr(&out).contains("generation failed (rc=1)"),
        "{}",
        stderr(&out)
    );
    assert_untouched(&f, &tip_before, &out);
}

/// An empty snapshot is never published, even from a generator that exited 0.
#[test]
fn an_empty_snapshot_is_never_published() {
    let Some(f) = Fixture::new("gen-empty") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("");

    let out = f.tick();
    assert!(
        !out.status.success(),
        "an empty snapshot must fail the tick"
    );
    assert!(
        stderr(&out).contains("generation failed (rc=0)"),
        "{}",
        stderr(&out)
    );
    assert_untouched(&f, &tip_before, &out);
}

/// A history line that cannot be produced costs one plot point, not the snapshot: the snapshot
/// still publishes and the append-only history gains nothing, not even an empty line.
#[test]
fn a_failed_history_line_publishes_the_snapshot_without_one() {
    let Some(f) = Fixture::new("hist-fails") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    std::fs::write(f.root.join("history-rc"), "1\n").unwrap();

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("history line failed (rc=1)"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
    assert_eq!(f.local("human-queue-history.jsonl"), SEED_HISTORY);
}

/// With no branch yet, every present data path is unpublished — whatever the install's own index
/// holds for those paths.
#[test]
fn the_first_publish_ignores_the_installs_index() {
    let Some(f) = Fixture::bare("first-indexed") else {
        return;
    };
    git(
        &f.install,
        &[
            "add",
            "-f",
            "human-queue.json",
            "human-queue-history.jsonl",
            "landed-history.jsonl",
            "metrics/runs.jsonl",
        ],
    );

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(f.data_tip().is_some(), "the branch must be created");
    assert_eq!(f.published("human-queue.json"), SEED_SNAPSHOT.trim_end());
}

/// A data file git cannot hash fails the tick before anything is built.
#[test]
fn an_unhashable_data_file_fails_the_tick() {
    let Some(f) = Fixture::new("unhashable") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.fail_git("hash-object", 1);

    let out = f.tick();
    assert!(!out.status.success(), "the tick must fail");
    assert!(
        stderr(&out).contains("ERROR: cannot hash the data files"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.data_tip(), tip_before);
}

/// A commit that cannot be built is never pushed: an empty source in the push refspec would
/// DELETE the branch.
#[test]
fn a_failed_commit_build_never_pushes() {
    let Some(f) = Fixture::new("build-fails") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.fail_git("commit-tree", 1);

    let out = f.tick();
    assert!(!out.status.success(), "the tick must fail");
    assert!(
        stderr(&out).contains("building the dashboard-data commit failed"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        f.data_tip(),
        tip_before,
        "the branch must still be there, unmoved"
    );
}

/// Nothing to publish does not hide a diverged main: the tick still fails.
#[test]
fn an_unchanged_tick_on_a_diverged_main_still_fails() {
    let Some(f) = Fixture::new("unchanged-diverged") else {
        return;
    };
    f.diverge_main();

    let out = f.tick();
    assert!(!out.status.success(), "a diverged main must fail the tick");
    assert!(
        stderr(&out).contains("nothing to publish"),
        "{}",
        stderr(&out)
    );
}

/// A tick that moves both the snapshot and the run metrics names both.
#[test]
fn a_snapshot_and_metrics_move_get_the_combined_subject() {
    let Some(f) = Fixture::new("both") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.append_local(
        "metrics/runs.jsonl",
        "{\"runId\":\"20260801T000000Z\",\"role\":\"vetter\",\"outcome\":\"ok\"}\n",
    );

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", DATA_BRANCH]),
        "chore(dashboard): refresh human-queue.json snapshot + run metrics"
    );
}

/// A tick where only a ledger without a subject of its own moved gets the generic subject.
#[test]
fn a_landed_only_move_gets_the_generic_subject() {
    let Some(f) = Fixture::new("landed-only") else {
        return;
    };
    f.append_local(
        "landed-history.jsonl",
        "{\"ts\":\"2026-08-01T00:00:00Z\",\"observedAt\":\"2026-08-01T00:00:00Z\",\
         \"kind\":\"pr\",\"repo\":\"o/healed\",\"number\":2}\n",
    );

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(f.published("landed-history.jsonl").contains("o/healed"));
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", DATA_BRANCH]),
        "chore(dashboard): publish accrued dashboard data"
    );
}

/// The install's own commit-signing config cannot break the publish: the data commit is unsigned.
#[test]
fn an_install_that_signs_commits_still_publishes() {
    let Some(f) = Fixture::new("signing") else {
        return;
    };
    git(&f.install, &["config", "commit.gpgsign", "true"]);
    git(&f.install, &["config", "gpg.program", "false"]);
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
}

/// A push rejected because another publisher already landed this exact data is success, and
/// adds no commit of its own.
#[test]
fn a_rejected_push_whose_data_is_already_published_is_success() {
    let Some(f) = Fixture::new("already") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.race_with_our_own_data();

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("already carries this data"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", DATA_BRANCH]),
        "raced",
        "no commit may be added on top of the racer's"
    );
}

/// The already-published path still reports a diverged main.
#[test]
fn an_already_published_tick_on_a_diverged_main_still_fails() {
    let Some(f) = Fixture::new("already-diverged") else {
        return;
    };
    f.diverge_main();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.race_with_our_own_data();

    let out = f.tick();
    assert!(!out.status.success(), "a diverged main must fail the tick");
    assert!(
        stderr(&out).contains("already carries this data"),
        "{}",
        stderr(&out)
    );
}

/// Three snapshots on `dashboard-data`, published as a history the install has never fetched
/// and that replaced the one it has: the backfills must read the remote branch as it is now.
fn publish_snapshot_history(f: &Fixture) -> [&'static str; 3] {
    git(&f.install, &["fetch", "--quiet", "origin"]);
    let snaps = [
        "{\"counts\":{\"s\":1}}",
        "{\"counts\":{\"s\":2}}",
        "{\"counts\":{\"s\":3}}",
    ];
    git(&f.other, &["switch", "--quiet", "--orphan", "rewritten"]);
    for s in snaps {
        std::fs::write(f.other.join("human-queue.json"), format!("{s}\n")).unwrap();
        git(&f.other, &["add", "human-queue.json"]);
        git(&f.other, &["commit", "--quiet", "-m", s]);
    }
    git(
        &f.other,
        &[
            "push",
            "--quiet",
            "--force",
            "origin",
            &format!("rewritten:{DATA_BRANCH}"),
        ],
    );
    git(&f.install, &["switch", "--quiet", "--detach"]);
    snaps
}

/// The history backfill walks `dashboard-data` on the remote, oldest first — from a detached
/// install too, where the remote falls back to origin.
#[test]
fn the_history_backfill_walks_the_published_branch() {
    let Some(f) = Fixture::new("backfill-history") else {
        return;
    };
    let snaps = publish_snapshot_history(&f);

    let out = f.run_repo_script("backfill-human-queue-history.sh");
    assert!(out.status.success(), "backfill failed: {}", stderr(&out));
    let lines: Vec<String> = f
        .local("human-queue-history.jsonl")
        .lines()
        .map(|l| {
            l.split_once(",\"snap\":")
                .map(|(_, s)| s.strip_suffix('}').unwrap_or(s).to_string())
                .unwrap_or_default()
        })
        .collect();
    let expected: Vec<String> = snaps.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        lines, expected,
        "one line per published snapshot, oldest first"
    );
}

/// The landed backfill diffs each consecutive pair of `dashboard-data` snapshots on the remote.
#[test]
fn the_landed_backfill_walks_the_published_branch() {
    let Some(f) = Fixture::new("backfill-landed") else {
        return;
    };
    let snaps = publish_snapshot_history(&f);
    let row = "{\"ts\":\"2026-08-11T10:00:00Z\",\"observedAt\":\"stub\",\
               \"kind\":\"pr\",\"repo\":\"o/r\",\"number\":7}";
    std::fs::write(f.root.join("landed-lines"), format!("{row}\n")).unwrap();

    let out = f.run_repo_script("backfill-landed-history.sh");
    assert!(out.status.success(), "backfill failed: {}", stderr(&out));
    assert_eq!(
        f.local("landed-history.jsonl").lines().count(),
        3,
        "the seed row plus one per consecutive pair (two pairs from three snapshots)"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("landed-prev-operand")).unwrap(),
        format!("{}\n", snaps[1]),
        "the last pair's previous snapshot is the second one"
    );
}

/// An install only AHEAD of `main` (local commits, nothing new upstream) has not diverged: the
/// tick publishes and succeeds, and the local commits stay unpushed.
#[test]
fn an_install_only_ahead_of_main_publishes_and_succeeds() {
    let Some(f) = Fixture::new("ahead") else {
        return;
    };
    let main_before = f.origin_main();
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
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert!(!stderr(&out).contains("diverged"), "{}", stderr(&out));
    assert_eq!(f.origin_main(), main_before, "main must never be pushed");
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
}

/// A failed fetch of `dashboard-data` itself stops the tick: a stale tip is never built on.
#[test]
fn a_failed_data_branch_fetch_stops_the_tick() {
    let Some(f) = Fixture::new("data-fetch") else {
        return;
    };
    let tip_before = f.data_tip();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.fail_git("fetch", 2);

    let out = f.tick();
    assert!(!out.status.success(), "the tick must fail");
    assert!(
        stderr(&out)
            .contains("+refs/heads/dashboard-data:refs/remotes/origin/dashboard-data failed"),
        "{}",
        stderr(&out)
    );
    assert_untouched(&f, &tip_before, &out);
}

/// A changed snapshot whose landed diff found nothing appends nothing to the landed ledger.
#[test]
fn an_empty_landed_diff_appends_nothing() {
    let Some(f) = Fixture::new("landed-empty") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(f.local("landed-history.jsonl"), SEED_LANDED);
    assert_eq!(f.published("landed-history.jsonl"), SEED_LANDED.trim_end());
}

/// The install dir as a fresh box (or a checkout that never held the data) sees it: none of the
/// data files, and no `metrics/` directory either.
fn wipe_local_data(f: &Fixture) {
    for rel in [
        "human-queue.json",
        "human-queue-history.jsonl",
        "landed-history.jsonl",
    ] {
        std::fs::remove_file(f.install.join(rel)).unwrap();
    }
    std::fs::remove_dir_all(f.install.join("metrics")).unwrap();
}

/// Add a file to the published branch from the other clone, as an earlier publisher would have.
fn publish_from_other(f: &Fixture, rel: &str, text: &str) {
    git(&f.other, &["fetch", "--quiet", "origin", DATA_BRANCH]);
    git(
        &f.other,
        &["switch", "--quiet", "-C", DATA_BRANCH, "FETCH_HEAD"],
    );
    std::fs::write(f.other.join(rel), text).unwrap();
    git(&f.other, &["add", "-A"]);
    git(&f.other, &["commit", "--quiet", "-m", "earlier publish"]);
    git(&f.other, &["push", "--quiet", "origin", DATA_BRANCH]);
    git(&f.other, &["switch", "--quiet", "main"]);
}

/// THE regression test for a fresh install. The data files left `main`, so a new box has none of
/// them. Its first tick restores every feed the branch carries before it appends to any of them:
/// the published history keeps its earlier lines and gains the new one, rather than being
/// replaced by it. The restored snapshot is the previous one, so the landed diff is taken against
/// it.
#[test]
fn a_fresh_install_restores_the_published_feeds_before_appending() {
    let Some(f) = Fixture::new("fresh-install") else {
        return;
    };
    let touches = "{\"actor\":\"producer\",\"item\":\"o/r#1\"}\n";
    publish_from_other(&f, "fsm-touches.jsonl", touches);
    wipe_local_data(&f);
    let row = "{\"ts\":\"2026-08-11T10:00:00Z\",\"observedAt\":\"stub\",\
               \"kind\":\"pr\",\"repo\":\"o/r\",\"number\":7}";
    std::fs::write(f.root.join("landed-lines"), format!("{row}\n")).unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(f.root.join("landed-prev-operand")).unwrap(),
        SEED_SNAPSHOT,
        "the landed diff must be taken against the published snapshot"
    );
    let history = f.published("human-queue-history.jsonl");
    assert_eq!(
        history.lines().collect::<Vec<_>>(),
        [SEED_HISTORY.trim_end(), "{\"ts\":\"stub\",\"counts\":{}}"],
        "the history keeps its published lines and gains this tick's"
    );
    assert_eq!(
        f.published("landed-history.jsonl")
            .lines()
            .collect::<Vec<_>>(),
        [SEED_LANDED.trim_end(), row],
        "the landed ledger keeps its published rows and gains this tick's"
    );
    assert_eq!(f.published("metrics/runs.jsonl"), SEED_RUNS.trim_end());
    assert_eq!(f.published("fsm-touches.jsonl"), touches.trim_end());
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
    assert_eq!(f.local("metrics/runs.jsonl"), SEED_RUNS);
    assert_eq!(f.local("fsm-touches.jsonl"), touches);
    for rel in [
        "human-queue.json",
        "human-queue-history.jsonl",
        "landed-history.jsonl",
        "metrics/runs.jsonl",
        "fsm-touches.jsonl",
    ] {
        assert!(
            stderr(&out).contains(&format!("restored {rel} from dashboard-data")),
            "the restore of {rel} must be on the record: {}",
            stderr(&out)
        );
    }
    for line in stderr(&out).lines().filter(|l| !l.trim().is_empty()) {
        assert!(is_stamped(line), "unstamped line: {line:?}");
    }
    assert_eq!(
        git(&f.install, &["status", "--porcelain"]),
        "",
        "restored files are data, ignored on main"
    );
}

/// A fresh install whose generator reproduces the published snapshot has nothing to publish: the
/// restored snapshot is the previous one, so it has not moved and no history line is appended.
#[test]
fn a_fresh_install_with_an_unchanged_snapshot_is_a_no_op() {
    let Some(f) = Fixture::new("fresh-unchanged") else {
        return;
    };
    let tip_before = f.data_tip();
    wipe_local_data(&f);

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(f.data_tip(), tip_before, "nothing moved: {}", stderr(&out));
    assert_eq!(f.local("human-queue-history.jsonl"), SEED_HISTORY);
    assert_eq!(f.local("landed-history.jsonl"), SEED_LANDED);
    assert!(
        !f.install.join("fsm-touches.jsonl").exists(),
        "a ledger the branch never carried must not be invented"
    );
}

/// An empty feed is as good as absent: it is restored, not published over the history.
#[test]
fn an_empty_local_feed_is_restored_from_the_branch() {
    let Some(f) = Fixture::new("empty-feed") else {
        return;
    };
    std::fs::write(f.install.join("human-queue-history.jsonl"), "").unwrap();
    std::fs::write(f.install.join("metrics/runs.jsonl"), "").unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    assert_eq!(
        f.published("human-queue-history.jsonl").lines().next(),
        Some(SEED_HISTORY.trim_end()),
        "the published history must keep its first line"
    );
    assert_eq!(f.published("human-queue-history.jsonl").lines().count(), 2);
    assert_eq!(f.published("metrics/runs.jsonl"), SEED_RUNS.trim_end());
}

/// A feed that cannot be restored is never published over: the tick fails before it generates.
#[test]
fn a_failed_restore_stops_the_tick() {
    let Some(f) = Fixture::new("restore-fails") else {
        return;
    };
    let tip_before = f.data_tip();
    std::fs::remove_file(f.install.join("human-queue-history.jsonl")).unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    f.fail_git("cat-file", 1);

    let out = f.tick();
    assert!(!out.status.success(), "the tick must fail");
    assert!(
        stderr(&out)
            .contains("ERROR: cannot restore human-queue-history.jsonl from dashboard-data"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.data_tip(), tip_before, "nothing may be published");
    assert_eq!(
        f.local("human-queue.json"),
        SEED_SNAPSHOT,
        "nothing may be generated"
    );
    assert!(
        !f.install.join("human-queue-history.jsonl").exists(),
        "a partial restore must not land"
    );
    assert_eq!(
        std::fs::read_dir(f.root.join("tmp")).unwrap().count(),
        0,
        "the failed restore's temp file must be removed"
    );
}

/// A restore that cannot even get a temp file fails the same way, on a stamped line: the log
/// must still answer when and why the tick stopped.
#[test]
fn a_restore_without_a_temp_file_fails_on_a_stamped_line() {
    let Some(f) = Fixture::new("restore-no-tmp") else {
        return;
    };
    let tip_before = f.data_tip();
    std::fs::remove_file(f.install.join("human-queue-history.jsonl")).unwrap();
    std::fs::remove_dir(f.root.join("tmp")).unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert!(!out.status.success(), "the tick must fail");
    assert!(
        stderr(&out)
            .contains("ERROR: cannot restore human-queue-history.jsonl from dashboard-data"),
        "{}",
        stderr(&out)
    );
    for line in stderr(&out).lines().filter(|l| !l.trim().is_empty()) {
        assert!(is_stamped(line), "unstamped line: {line:?}");
    }
    assert_eq!(f.data_tip(), tip_before, "nothing may be published");
}

const FEEDS: [&str; 4] = [
    "human-queue-history.jsonl",
    "landed-history.jsonl",
    "metrics/runs.jsonl",
    "fsm-touches.jsonl",
];

/// A refused feed fails the tick with exit 1 on exactly one stamped ERROR line, which names it.
fn assert_refused(out: &Output, rel: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
    let errors: Vec<String> = stderr(out)
        .lines()
        .filter(|l| l.contains("ERROR"))
        .map(str::to_string)
        .collect();
    assert_eq!(errors.len(), 1, "one ERROR line: {}", stderr(out));
    assert!(is_stamped(&errors[0]), "unstamped: {:?}", errors[0]);
    assert!(
        errors[0].contains(&format!(
            "ERROR: {rel} does not extend its copy on dashboard-data"
        )),
        "the ERROR must name {rel}: {:?}",
        errors[0]
    );
}

/// At push time, another publisher appends `line` to `rel` on `dashboard-data`. Fires once.
fn race_a_feed_line(f: &Fixture, rel: &str, line: &str) {
    let hook = format!(
        "#!/usr/bin/env bash\n\
         set -uo pipefail\n\
         unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
         [ -e {marker} ] && exit 0\n\
         touch {marker}\n\
         git -C {other} fetch --quiet origin {branch} >/dev/null 2>&1\n\
         git -C {other} switch --quiet -C {branch} FETCH_HEAD >/dev/null 2>&1\n\
         printf '%s\\n' '{line}' >> {other}/{rel}\n\
         git -C {other} add -A >/dev/null 2>&1\n\
         git -C {other} commit --quiet -m 'raced' >/dev/null 2>&1\n\
         git -C {other} push --quiet origin {branch} >/dev/null 2>&1\n\
         git -C {other} switch --quiet main >/dev/null 2>&1\n\
         exit 0\n",
        marker = f.root.join(".raced").display(),
        other = f.other.display(),
        branch = DATA_BRANCH,
    );
    write_exec(&f.install.join(".git/hooks/pre-push"), &hook);
}

/// THE regression test for a feed born on a new box. A runner appends to `metrics/runs.jsonl`
/// before the first refresh tick, so the restore finds it non-empty and keeps it. Publishing it
/// would replace the branch's run history with one line, so the tick refuses and publishes
/// nothing at all, not even the changed snapshot.
#[test]
fn a_new_boxs_own_run_feed_never_replaces_the_published_history() {
    let Some(f) = Fixture::new("new-box-runs") else {
        return;
    };
    let tip_before = f.data_tip();
    wipe_local_data(&f);
    std::fs::create_dir_all(f.install.join("metrics")).unwrap();
    let own = "{\"runId\":\"20261028T000000Z\",\"role\":\"vetter\",\"outcome\":\"ok\"}\n";
    std::fs::write(f.install.join("metrics/runs.jsonl"), own).unwrap();
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");

    let out = f.tick();
    assert_refused(&out, "metrics/runs.jsonl");
    assert_eq!(f.data_tip(), tip_before, "nothing may be published");
    assert_eq!(f.published("metrics/runs.jsonl"), SEED_RUNS.trim_end());
    assert_eq!(f.published("human-queue.json"), SEED_SNAPSHOT.trim_end());
    assert_eq!(f.local("metrics/runs.jsonl"), own, "the local feed is kept");
}

/// Every append-only feed is held to the rule: a local copy that rewrites the published one is
/// refused, whichever feed it is.
#[test]
fn every_feed_that_rewrites_the_published_one_is_refused() {
    for (i, rel) in FEEDS.iter().enumerate() {
        let Some(f) = Fixture::new(&format!("rewritten-feed-{i}")) else {
            return;
        };
        if *rel == "fsm-touches.jsonl" {
            publish_from_other(&f, rel, "{\"item\":\"o/r#1\"}\n");
        }
        let tip_before = f.data_tip();
        std::fs::write(f.install.join(rel), "{\"rewritten\":true}\n").unwrap();

        let out = f.tick();
        assert_refused(&out, rel);
        assert_eq!(f.data_tip(), tip_before, "{rel}: nothing may be published");
    }
}

/// A local feed SHORTER than the published one is a prefix of it, not an extension: publishing
/// it would drop the published tail.
#[test]
fn a_feed_shorter_than_the_published_one_is_refused() {
    let Some(f) = Fixture::new("shortened-feed") else {
        return;
    };
    let row = "{\"ts\":\"2026-08-11T10:00:00Z\",\"kind\":\"pr\",\"repo\":\"o/r\",\"number\":7}\n";
    publish_from_other(&f, "landed-history.jsonl", &format!("{SEED_LANDED}{row}"));
    let tip_before = f.data_tip();

    let out = f.tick();
    assert_refused(&out, "landed-history.jsonl");
    assert_eq!(f.data_tip(), tip_before, "nothing may be published");
    assert_eq!(f.local("landed-history.jsonl"), SEED_LANDED);
}

/// The rule is a byte prefix, nothing stricter: every feed that only appends to its published
/// copy publishes, including onto a published copy that is empty or lacks its final newline.
#[test]
fn every_feed_that_extends_the_published_one_publishes() {
    for (i, rel) in FEEDS.iter().enumerate() {
        for (j, published) in ["", "{\"partial\":"].iter().enumerate() {
            let Some(f) = Fixture::new(&format!("extended-feed-{i}-{j}")) else {
                return;
            };
            publish_from_other(&f, rel, published);
            let local = format!("{published}1}}\n{{\"next\":2}}\n");
            std::fs::write(f.install.join(rel), &local).unwrap();

            let out = f.tick();
            assert!(out.status.success(), "{rel}: {}", stderr(&out));
            assert_eq!(f.published(rel), local.trim_end(), "{rel} published as-is");
        }
    }
}

/// The rule is re-checked against the tip each push is rebuilt onto: a publisher that appends to
/// a feed mid-tick leaves the install's copy no longer an extension, so the rebuild is refused
/// rather than published over the racer's line.
#[test]
fn a_feed_appended_on_the_branch_mid_tick_is_refused_on_the_rebuild() {
    let Some(f) = Fixture::new("raced-feed") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    race_a_feed_line(&f, "human-queue-history.jsonl", "{\"ts\":\"raced\"}");

    let out = f.tick();
    assert_refused(&out, "human-queue-history.jsonl");
    let tip = f.data_tip().unwrap();
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", &tip]),
        "raced",
        "the racer's commit stays the tip: {}",
        stderr(&out)
    );
    assert_eq!(
        f.published("human-queue-history.jsonl"),
        format!("{}{}", SEED_HISTORY, "{\"ts\":\"raced\"}")
    );
    assert!(
        stderr(&out).contains("attempt 1: dashboard-data moved"),
        "{}",
        stderr(&out)
    );
}

/// A feed the install does not hold is not published, so the branch's copy cannot be shortened by
/// it: a feed another publisher adds to the branch mid-tick, while the install lacks it, is no
/// reason to refuse the rebuild, and the branch keeps it.
#[test]
fn a_feed_born_on_the_branch_mid_tick_is_carried_forward_by_the_rebuild() {
    let Some(f) = Fixture::new("raced-born-feed") else {
        return;
    };
    f.set_next_snapshot("{\"counts\":{\"a\":2}}\n");
    let touches = "{\"actor\":\"producer\",\"item\":\"o/r#1\"}";
    race_a_feed_line(&f, "fsm-touches.jsonl", touches);

    let out = f.tick();
    assert!(out.status.success(), "tick failed: {}", stderr(&out));
    let tip = f.data_tip().unwrap();
    let raced = git(&f.origin, &["rev-parse", &format!("{tip}^")]);
    assert_eq!(
        git(&f.origin, &["log", "-1", "--format=%s", &raced]),
        "raced"
    );
    assert_eq!(f.published("fsm-touches.jsonl"), touches);
    assert_eq!(f.published("human-queue.json"), "{\"counts\":{\"a\":2}}");
    assert!(!f.install.join("fsm-touches.jsonl").exists());
}
