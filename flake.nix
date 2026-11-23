{
  description = "CodeRabbit OSS budget tracker, scheduler, coverage map, and findings steward";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      nixFiles = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./flake.nix
          ./nix
        ];
      };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          coderabbit-steward = pkgs.callPackage ./nix/package.nix { };
        in
        {
          default = coderabbit-steward;
          inherit coderabbit-steward;
          inherit (coderabbit-steward.passthru) web;
        }
      );

      checks = forAllSystems (
        pkgs:
        let
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) coderabbit-steward web;
        in
        {
          inherit coderabbit-steward web;
          inherit (coderabbit-steward.passthru.tests) daemon-smoke;
          nix-lint =
            pkgs.runCommand "coderabbit-steward-nix-lint"
              {
                nativeBuildInputs = [
                  pkgs.nixfmt
                  pkgs.statix
                  pkgs.deadnix
                ];
              }
              ''
                cd ${nixFiles}
                nixfmt --check flake.nix nix/package.nix nix/module.nix
                statix check .
                deadnix --fail .
                touch "$out"
              '';
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.sqlite
            pkgs.git
            pkgs.nodejs
            pkgs.just
            pkgs.nixfmt
            pkgs.statix
            pkgs.deadnix
          ];
          env = {
            LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
          };
        };
      });

      overlays.default = final: _prev: {
        coderabbit-steward = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      nixosModules = {
        default = {
          imports = [ ./nix/module.nix ];
          nixpkgs.overlays = [ self.overlays.default ];
        };
        coderabbit-steward = self.nixosModules.default;
      };
    };
}
