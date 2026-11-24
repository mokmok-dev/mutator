# A flake-parts module: import it and configure `perSystem.mutator`.
#
#   imports = [ inputs.mutator.flakeModules.default ];
#   perSystem.mutator = { enable = true; check = true; };
#
# `enable` adds the binary as `packages.mutator`, `apps.mutator`, and a package of
# the default dev shell. `check` adds `checks.mutator`, which `nix flake check`
# then runs.
{
  lib,
  flake-parts-lib,
  inputs,
  self,
  ...
}:
let
  inherit (lib) mkEnableOption mkOption types;
  inherit (flake-parts-lib) mkPerSystemOption;
in
{
  options.perSystem = mkPerSystemOption (
    { system, ... }:
    {
      options.mutator = {
        enable = mkEnableOption "the mutator mutation-testing binary";

        check = mkOption {
          type = types.bool;
          default = true;
          description = ''
            Add `checks.mutator`, a full mutation run of the project inside the
            Nix sandbox. It runs every mutant, so it costs minutes; disable it
            when CI already runs the gate with a warm `--cache` directory.
          '';
        };

        package = mkOption {
          type = types.package;
          default = inputs.mutator.packages.${system}.default;
          defaultText = lib.literalExpression "inputs.mutator.packages.\${system}.default";
          description = "The `mutator` binary to use.";
        };

        source = mkOption {
          type = types.path;
          default = self;
          defaultText = lib.literalExpression "self";
          description = "The project to test. Its `Cargo.lock` must be committed.";
        };

        args = mkOption {
          type = types.listOf types.str;
          default = [
            "--all"
            "--fail-on-survived"
          ];
          description = "Arguments for `mutator` in the check.";
        };

        jobs = mkOption {
          type = types.ints.positive;
          default = 4;
          description = "Parallel `cargo-mutants` jobs, passed as `--jobs`.";
        };
      };
    }
  );

  config.perSystem =
    {
      config,
      pkgs,
      system,
      ...
    }:
    let
      cfg = config.mutator;
      # Only cargo sources, so a local `source` does not drag `target/` in.
      src = lib.cleanSourceWith {
        src = cfg.source;
        filter =
          path: type:
          type == "directory"
          || lib.hasSuffix ".rs" path
          || lib.hasSuffix ".toml" path
          || lib.hasSuffix "Cargo.lock" path;
      };
      vendor = pkgs.rustPlatform.importCargoLock { lockFile = cfg.source + "/Cargo.lock"; };
      # `cargo-mutants` shells out to `cargo`, which reads source replacement from
      # `CARGO_HOME` rather than from the copied project.
      cargoConfig = pkgs.writeText "cargo-config.toml" ''
        [source.crates-io]
        replace-with = "vendored-sources"

        [source.vendored-sources]
        directory = "${vendor}"
      '';
    in
    lib.mkIf cfg.enable {
      packages.mutator = cfg.package;
      apps.mutator = {
        type = "app";
        program = "${cfg.package}/bin/mutator";
        meta.description = "Diff-driven mutation testing for Rust";
      };
      packages.default = cfg.package;
      apps.default = config.apps.mutator;

      checks.mutator = lib.mkIf cfg.check (
        pkgs.runCommandLocal "mutator-check-${toString system}"
          {
            nativeBuildInputs = [
              pkgs.cargo
              pkgs.cargo-mutants
              pkgs.rustc
              pkgs.stdenv.cc
            ];
            # `cargo-mutants` edits the source it mutates, so work on a copy.
            # `cargo` writes a global cache, so give it a writable home.
            CARGO_NET_OFFLINE = "true";
            RUST_BACKTRACE = "1";
            meta.description = "Every mutant in ${cfg.source} is killed";
          }
          ''
            cp -r ${src} work
            chmod -R u+w work
            cd work

            export HOME=$TMPDIR
            export CARGO_HOME=$TMPDIR/cargo
            mkdir -p $CARGO_HOME
            cp ${cargoConfig} $CARGO_HOME/config.toml

            ${cfg.package}/bin/mutator \
              ${
                lib.escapeShellArgs (
                  cfg.args
                  ++ [
                    "--jobs"
                    (toString cfg.jobs)
                  ]
                )
              } \
              > report.txt || {
              cat report.txt
              for log in target/mutator/mutants-*/mutants.out/log/*; do
                echo "--- $log"
                cat "$log"
              done
              exit 1
            }
            cat report.txt
            touch $out
          ''
      );
    };
}
