{
  description = "Diff-driven mutation testing for Rust, built on cargo-mutants";

  # The published flake carries the package and the module, so it depends on
  # nixpkgs alone. Development tooling lives in ./dev.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";

  outputs =
    {
      self,
      nixpkgs,
      ...
    }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      mutator =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "mutator";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = false;
          meta = {
            description = "Diff-driven mutation testing for Rust on top of cargo-mutants";
            license = pkgs.lib.licenses.mit;
            mainProgram = "mutator";
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          package = mutator (import nixpkgs { inherit system; });
        in
        {
          default = package;
          mutator = package;
        }
      );

      apps = forAllSystems (
        system:
        let
          app = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/mutator";
            meta.description = "Diff-driven mutation testing for Rust";
          };
        in
        {
          default = app;
          mutator = app;
        }
      );

      flakeModules.default = ./nix/mutator-module.nix;
    };
}
