{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      nixpkgs,
      flake-parts,
      treefmt-nix,
      rust-overlay,
      crane,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        treefmt-nix.flakeModule
      ];

      flake = {
        # `imports = [ inputs.mutator.flakeModules.default ]` then
        # `perSystem.mutator = { enable = true; check = true; };`
        flakeModules.default = ./nix/mutator-module.nix;
      };

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = craneLib.cleanCargoSource ./.;
          commonArgs = {
            inherit src;
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          linkerPackages = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
            with pkgs;
            [
              clang
              mold
            ]
          );
          linkerEnv = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            export RUSTFLAGS="-C link-arg=-fuse-ld=mold"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="clang"
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="clang"
          '';

          # The binary a consumer gets from `inputs.mutator.packages.<system>.default`.
          mutator = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoBuildCommand = "cargo build --release --package mutator";
              cargoExtraArgs = "--package mutator";
              doCheck = false;
              meta = {
                description = "Diff-driven mutation testing for Rust on top of cargo-mutants";
                license = pkgs.lib.licenses.mit;
                mainProgram = "mutator";
              };
            }
          );
        in
        {
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          apps = {
            default = {
              type = "app";
              program = "${mutator}/bin/mutator";
              meta.description = "Diff-driven mutation testing for Rust";
            };
            mutator = {
              type = "app";
              program = "${mutator}/bin/mutator";
              meta.description = "Diff-driven mutation testing for Rust";
            };
          };

          checks = {
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              }
            );
            fmt = craneLib.cargoFmt (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );
            test = craneLib.cargoTest (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );

            # One check per linter, so a failure names the tool. They come from the
            # same set the formatter runs, so `nix fmt` and CI cannot disagree.
            actionlint = pkgs.runCommandLocal "actionlint" { nativeBuildInputs = [ pkgs.actionlint ]; } ''
              actionlint -color ${./.}/.github/workflows/*.yaml
              touch $out
            '';
            deadnix = pkgs.runCommandLocal "deadnix" { nativeBuildInputs = [ pkgs.deadnix ]; } ''
              deadnix --fail ${./.}
              touch $out
            '';
            statix = pkgs.runCommandLocal "statix" { nativeBuildInputs = [ pkgs.statix ]; } ''
              statix check ${./.}
              touch $out
            '';
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.treefmt.build.devShell ];

            packages = with pkgs; [
              actionlint
              cargo-mutants
              cargo-nextest
              config.treefmt.build.wrapper
              deadnix
              linkerPackages
              rustToolchain
              sccache
              skills
              statix
            ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ${linkerEnv}
            '';
          };

          packages = {
            default = mutator;
            inherit mutator;
          };

          # Formatting only. The linters live in `checks`, so `nix fmt` never
          # rewrites code and a lint failure names the tool.
          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
            };
          };
        };

      systems = [
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-linux"
      ];
    };
}
