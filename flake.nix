{
  description = "Mirrors the Japanese Diet record API of the National Diet Library";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
  }: let
    forAll = f: nixpkgs.lib.genAttrs ["x86_64-linux"] (s: f nixpkgs.legacyPackages.${s});

    parts = pkgs: let
      inherit (pkgs) lib;
      craneLib = crane.mkLib pkgs;
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
          ./tests
        ];
      };
      common = {
        inherit src;
        strictDeps = true;
      };
      cargoArtifacts = craneLib.buildDepsOnly common;
    in {
      inherit cargoArtifacts;
      bin = craneLib.buildPackage (common
        // {
          inherit cargoArtifacts;
          cargoExtraArgs = "--locked";
          doCheck = false;
          meta.mainProgram = "kokkai-mirror";
        });
      test = craneLib.cargoTest (common // {inherit cargoArtifacts;});
      clippy = craneLib.cargoClippy (common
        // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- -D warnings";
        });
      fmt = craneLib.cargoFmt {inherit src;};
    };
  in {
    packages = forAll (pkgs: {default = (parts pkgs).bin;});

    checks = forAll (pkgs: let
      p = parts pkgs;
    in {
      inherit (p) clippy fmt test;
      build = p.bin;
    });

    devShells = forAll (pkgs: {
      default = pkgs.mkShell {
        packages = with pkgs; [cargo rustc clippy rustfmt rust-analyzer];
      };
    });
  };
}
