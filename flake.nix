{
  description = "A flake for talos9s, a Talos TUI for real-time node monitoring.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
    }:
    let
      inherit (nixpkgs) lib;
      forEachPkgs = f: lib.genAttrs (import systems) (system: f nixpkgs.legacyPackages.${system});

      package =
        {
          lib,
          rustPlatform,
          protobuf,
        }:
        let
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        in
        rustPlatform.buildRustPackage rec {
          inherit (manifest.workspace.package) version;

          pname = "talos9s";
          src = ./.;

          cargoDeps = rustPlatform.importCargoLock {
            lockFile = src + "/Cargo.lock";
          };

          nativeBuildInputs = [
            protobuf
          ];

          meta = {
            description = "Talos TUI for real-time node monitoring, log streaming, etcd health, and diagnostics";
            homepage = "https://github.com/olexiyb/talos9s";
            license = with lib.licenses; [ mit ];
            mainProgram = "talos9s";
          };
        };
    in
    {
      packages = forEachPkgs (pkgs: rec {
        talos9s = pkgs.callPackage package { inherit (pkgs) protobuf; };
        default = talos9s;
      });
      devShells = forEachPkgs (pkgs: {
        default = pkgs.mkShell {
          # automatically pulls nativeBuildInputs + buildInputs
          inputsFrom = [ (pkgs.callPackage package { inherit (pkgs) protobuf; }) ];
        };
      });
      overlays.default = final: _: {
        talos9s = final.callPackage package { };
      };
    };
}
