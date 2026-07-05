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
        # The pipeline's deterministic tooling (queue, report, --commit-closes,
        # and the migrating recipe subcommands). Tests run in-build via doCheck;
        # invoked directly as `pr-review-report …` on PATH — no wrapper.
        pr-review-report = pkgs.rustPlatform.buildRustPackage {
          pname = "pr-review-report";
          version = "0.1.0";
          src = ./pr-review-report-rs;
          cargoLock.lockFile = ./Cargo.lock;
        };
      in
      {
        packages.pr-review-report = pr-review-report;
        packages.default = pr-review-report;
      }
    );
}
