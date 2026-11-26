{
  description = "Development environment for mutator";

  inputs = {
    # Following the published flake's nixpkgs is what keeps the dev shell's Rust
    # the same as the one that builds `packages.default`.
    root.url = "path:..";

    nixpkgs.follows = "root/nixpkgs";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      flake-parts,
      git-hooks,
      treefmt-nix,
      crane,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        git-hooks.flakeModule
        treefmt-nix.flakeModule
      ];

      perSystem =
        { pkgs, ... }:
        let
          craneLib = crane.mkLib pkgs;
          src = craneLib.cleanCargoSource ./..;
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
        in
        {
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
            # `nixpkgs` follows the root flake, but a root bump does not
            # re-resolve this lock. Without this check the dev shell would keep
            # an older rustc than `packages.default` and nothing would say so.
            nixpkgs-aligned =
              let
                rootRev = (builtins.fromJSON (builtins.readFile ../flake.lock)).nodes.nixpkgs.locked.rev;
                devRev = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked.rev;
              in
              pkgs.runCommandLocal "nixpkgs-aligned" { } (
                if rootRev == devRev then
                  "touch $out"
                else
                  ''
                    echo "dev/flake.lock pins nixpkgs ${devRev}, but flake.lock has ${rootRev}." >&2
                    echo "Run 'nix flake update' at the root, then 'nix flake update --flake ./dev'." >&2
                    exit 1
                  ''
              );
          };

          devShells.default = pkgs.mkShellNoCC {
            packages = with pkgs; [
              actionlint
              cargo
              cargo-mutants
              cargo-nextest
              clippy
              deadnix
              jq
              linkerPackages
              nixfmt
              nixfmt-tree
              rustc
              rustfmt
              sccache
              skills
              statix
            ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ${linkerEnv}
            '';
          };

          # The hooks and the formatter both cover the whole repository, not just
          # this directory. `rootSrc` is what the hook run copies, and it defaults
          # to this flake's own source.
          pre-commit.settings = {
            rootSrc = pkgs.lib.mkForce ./..;
            hooks = {
              actionlint.enable = true;
              deadnix.enable = true;
              statix.enable = true;
            };
            package = pkgs.prek;
          };

          treefmt = {
            projectRoot = ./..;
            # `flake.nix` names both this directory and the repository root, so the
            # wrapper would stop at whichever it meets first. `Cargo.toml` exists
            # only at the root, so `nix fmt` from either directory formats all of it.
            projectRootFile = "Cargo.toml";
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
