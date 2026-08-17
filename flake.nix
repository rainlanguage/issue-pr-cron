{
  description = "issue-pr-cron — pipeline tooling and the cron runners that execute it.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;
        # The crate is a workspace member, so the Cargo.lock lives at the repo
        # root, not in pr-review-report-rs/. buildRustPackage needs the lock
        # inside src, so src is the workspace root — but filtered to just the
        # manifests + crate. Without the filter the whole repo (churning runs/,
        # metrics/, logs) would enter the build and bust the cache every tick.
        #
        # `target/` must be excluded explicitly. The crons resolve the flake as
        # `path:$DIR#pr-review-report`, and a `path:` ref copies the working
        # directory as-is — gitignored files included — so cargo's build output
        # sitting inside the crate dir lands in the source and changes its hash.
        # The install dir accumulates ~100MB there, which is why a cron tick
        # could rebuild the crate (and re-run its release-profile test suite)
        # before the model got a single token. `.gitignore` does not save us
        # here: it is not consulted for a `path:` ref.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            # Subtract rather than whitelist src/: a whitelist silently drops
            # anything the crate gains later (tests/, benches/, build.rs), which
            # fails as a missing test gate rather than a loud error.
            (lib.fileset.difference ./pr-review-report-rs (
              lib.fileset.maybeMissing ./pr-review-report-rs/target
            ))
          ];
        };
        # The pipeline's deterministic tooling (queue, report, --commit-closes,
        # --run-metrics, and the migrating recipe subcommands). Tests run in-build
        # via doCheck; invoked directly as `pr-review-report …` on PATH — no wrapper.
        pr-review-report = pkgs.rustPlatform.buildRustPackage {
          pname = "pr-review-report";
          version = "0.1.0";
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;
          # `git` is needed by the CHECK phase only, so it stays out of the runtime closure. The
          # clone-lifecycle and `pr_checkout` tests drive real repositories through real `git`
          # invocations against `file://` remotes — the shallow-clone behaviour of #81 is a property
          # of git's refspec handling, and a fake would have asserted our own belief about it rather
          # than what git does. No network: every fixture is local.
          nativeCheckInputs = [ pkgs.git ];
        };

        # Lift a repo script into a real package: nix builds its PATH from runtimeInputs,
        # and shellcheck runs at build time so a broken runner fails the build rather than
        # the 03:00 cron.
        #
        # `set +o errexit` is restored in each body. writeShellApplication forces
        # `set -euo pipefail`, but these runners are written WITHOUT errexit on purpose —
        # they test `rc` explicitly, use `grep -q … &&` as a conditional, and tolerate
        # best-effort steps. Inheriting errexit would abort a run on the first `grep` that
        # legitimately finds nothing. Each script re-disables it at the top, so the change
        # is visible in the diff rather than implied by the builder.
        runner =
          {
            name,
            file,
            runtimeInputs,
          }:
          pkgs.writeShellApplication {
            inherit name runtimeInputs;
            text = builtins.readFile file;
          };

        # `pdftoppm` + `pdfinfo`, for the HARNESS's own PDF read path — not for anything the
        # scripts or the prompts name. Claude Code's Read tool renders a PDF by shelling out to
        # `pdftoppm -jpeg -r 100 …` (page count via `pdfinfo`) and, when the binary is absent,
        # returns a tool result of `isError: pdftoppm is not installed…` instead of pages. That is
        # a SILENT failure mode for this pipeline: the model records a verdict or opens a PR
        # anyway, the run exits 0, and the only trace is one tool result nothing asserts on.
        #
        # The org's external audit evidence lives at `audit/protofire/*.pdf` — 110 such files
        # across the work clones on this box — so both runners read PDFs as a matter of course:
        # the vetter through its audit lens, the producer through the `audit`-labelled backlog it
        # is told to drain first (e.g. raindex#2619, whose stated source IS
        # `audit/protofire/raindex.e686b4d.apr-2026.pdf`).
        pdf-tools = pkgs.poppler-utils;

        # The PRODUCER's screenshot path (#251). `pr-review-report render-component`
        # execs these on the model's behalf, so — exactly like pdf-tools above —
        # nothing in this repo's scripts names them and no walk over script text
        # could find them. They are declared in `RENDER_TOOLS` and asserted by
        # `closure-preflight`.
        #
        # `nodejs` runs the CHECKOUT's own vite (`node node_modules/vite/bin/…`),
        # never a vendored copy: a render has to be built by the same bundler and
        # svelte version the app ships, or the picture is of a component nobody
        # deploys. `npm` is how a fresh clone gets that `node_modules` at all.
        #
        # `dejavu_fonts` carries no binary and is here for its STORE PATH: headless
        # chromium with no fonts draws every glyph blank — an image that looks like
        # a render and shows nothing that was written — and the font search reads
        # the store. In the closure, a font package is GC-rooted, which is the part
        # the trace harnesses' hand-pasted `/nix/store/…-dejavu-fonts-2.37` literals
        # could not give themselves.
        #
        # PRODUCER ONLY, and that is a fact about the role rather than an oversight:
        # review-settings.json denies `Bash` outright, so the vetter cannot exec any
        # of these. Each binary they add is in `DECLARED_ASYMMETRY`.
        render-tools = [
          pkgs.nodejs_22
          pkgs.chromium
          pkgs.dejavu_fonts
        ];

        # The producer. `gh` and `jq` are here because the MODEL shells out to them from its
        # Bash tool — bare, so campaign-settings.json's deny-list (which matches the literal
        # command text) still applies. `git` likewise: the producer commits and pushes.
        campaign-run = runner {
          name = "campaign-run";
          file = ./campaign-run.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.jq
            pkgs.git
            pkgs.coreutils
            pkgs.findutils
            pkgs.gnugrep
            pkgs.gnused
            pkgs.util-linux # flock, for the single-run lock
            pkgs.getent # resolves HOME when cron starts without it
            pdf-tools # audit evidence is PDF; see pdf-tools above
          ]
          ++ render-tools; # step 5's screenshots are rendered here; see render-tools above
        };

        # The vetter. `gh` is for the MCP SERVER, which shells out to it for every GitHub read
        # and its one write; the vetter model itself has no Bash. `jq` is NOT included: the
        # vetter's own jq use was the trace distiller and the metrics merge, both now
        # `pr-review-report` subcommands, and the model cannot invoke anything anyway.
        review-run = runner {
          name = "review-run";
          file = ./review-run.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            # Builds the `--agents` JSON that briefs every dispatched auditor (#257), exactly as it
            # does on the producer side. The SCRIPT's, never the model's: `Bash` is denied outright
            # in review-settings.json, so the vetter has nothing to invoke it from.
            pkgs.jq
            pkgs.git
            pkgs.coreutils
            pkgs.findutils
            pkgs.gnugrep
            pkgs.gnused # {{…}} substitution into the prompt template
            pkgs.util-linux # flock, for the single-run lock
            pkgs.getent # resolves HOME when cron starts without it
            pdf-tools # audit evidence is PDF; see pdf-tools above
          ];
        };

        # Data-only refresher on its own cron. Needs git (it commits the snapshot) and
        # util-linux for flock; no jq any more.
        refresh-human-queue = runner {
          name = "refresh-human-queue";
          file = ./refresh-human-queue.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.git
            pkgs.coreutils
            pkgs.util-linux # flock
            pkgs.getent # resolves HOME when cron starts without it
          ];
        };

        # The FSM doctor for the design lane (#241): routes every ai:design PR with no live
        # trusted design question back to ai:needs-work, daily. Writes labels + the trusted work
        # order via gh, so gh rides in the closure like the other runners'.
        design-doctor = runner {
          name = "design-doctor";
          file = ./design-doctor.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.coreutils
            pkgs.util-linux # flock
            pkgs.getent # resolves HOME when cron starts without it
          ];
        };

        # One-time history backfill; walks git history of the snapshot.
        backfill-human-queue-history = runner {
          name = "backfill-human-queue-history";
          file = ./backfill-human-queue-history.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.git
            pkgs.coreutils
          ];
        };

        # Seed + heal landed-history.jsonl; same pair-walk the live refresh appends with. Needs gh:
        # every departure is verified against GitHub, not inferred from the snapshots alone.
        backfill-landed-history = runner {
          name = "backfill-landed-history";
          file = ./backfill-landed-history.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.git
            pkgs.coreutils
          ];
        };

        # Human entry points. These were bash only because a bare script could not assume
        # `gh` was present and had to build the binary itself; both problems are gone, so
        # what remains is a one-line delegation kept for the existing invocation names.
        review-queue = runner {
          name = "sort-review-queue";
          file = ./sort-review-queue.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.coreutils
          ];
        };
        pr-review-report-sh = runner {
          name = "pr-review-report-sh";
          file = ./pr-review-report.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.coreutils
          ];
        };

        # The exact runtime a human gets when debugging the pipeline by hand: the same
        # pinned gh/jq/pr-review-report the runners execute with.
        #
        # mkShellNoCC, not mkShell: nothing here compiles, and a cc stdenv would put
        # gcc/binutils on PATH for no reason. The Rust toolchain does not enter the closure
        # either — pr-review-report is consumed as a built package.
        #
        # There is deliberately no shellHook: the runners tee stdout into a JSONL trace and
        # anything printed on shell entry corrupts the first line.
        cron-shell = pkgs.mkShellNoCC {
          name = "issue-pr-cron";
          packages = [
            pr-review-report
            pkgs.gh
            pkgs.jq
          ];
        };

        # Building the crate by hand. This exists because the scripts used to run
        # `nix shell nixpkgs#cargo nixpkgs#rustc --command cargo build --release` — an
        # unpinned toolchain, from the registry, producing a binary with no lock
        # relationship to the one `nix build` produces. That is deleted; this shell is
        # where a human iterates instead, on the toolchain the flake already pins.
        rust-shell = pkgs.mkShell {
          name = "issue-pr-cron-rust";
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      in
      {
        packages = {
          inherit
            pr-review-report
            campaign-run
            review-run
            refresh-human-queue
            design-doctor
            backfill-human-queue-history
            backfill-landed-history
            review-queue
            ;
          pr-review-report-sh = pr-review-report-sh;
          default = pr-review-report;
        };

        # `nix run .#campaign-run` / `.#review-run` — what the crontab invokes.
        apps = {
          campaign-run = {
            type = "app";
            program = "${campaign-run}/bin/campaign-run";
          };
          review-run = {
            type = "app";
            program = "${review-run}/bin/review-run";
          };
          refresh-human-queue = {
            type = "app";
            program = "${refresh-human-queue}/bin/refresh-human-queue";
          };
          design-doctor = {
            type = "app";
            program = "${design-doctor}/bin/design-doctor";
          };
        };

        devShells.cron = cron-shell;
        devShells.rust = rust-shell;
        # A bare `nix develop` otherwise falls back to the default *package's* build
        # environment, where jq and pr-review-report are absent and gh leaks in from the
        # user's ambient profile — i.e. `nix develop` gave LESS pinning than `nix shell`,
        # which is why the runners avoided it (#76 item 4).
        devShells.default = cron-shell;
      }
    );
}
