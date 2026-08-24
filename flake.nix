{
  description = "Run a local development server behind an embedded ngrok endpoint";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          nserve = pkgs.rustPlatform.buildRustPackage {
            pname = "nserve";
            version = "0.1.0";

            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.lock
                ./Cargo.toml
                ./src
              ];
            };

            cargoLock.lockFile = ./Cargo.lock;

            nativeCheckInputs = [ pkgs.python3 ];

            meta = {
              description = "Run a local development server behind an embedded ngrok endpoint";
              homepage = "https://github.com/euank/nserve";
              license = with pkgs.lib.licenses; [
                asl20
                mit
              ];
              mainProgram = "nserve";
              platforms = pkgs.lib.platforms.linux;
            };
          };

          default = nserve;
        }
      );

      apps = forAllSystems (
        system:
        let
          app = {
            type = "app";
            program = nixpkgs.lib.getExe self.packages.${system}.nserve;
            meta = self.packages.${system}.nserve.meta;
          };
        in
        {
          nserve = app;
          default = app;
        }
      );

      checks = forAllSystems (system: {
        inherit (self.packages.${system}) nserve;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
