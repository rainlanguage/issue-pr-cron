{
  description = "issue-pr-cron — pipeline tooling (pr-review-report and future subcommands).";

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
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./pr-review-report-rs
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
        };
      in
      {
        packages.pr-review-report = pr-review-report;
        packages.default = pr-review-report;
      }
    );
}
