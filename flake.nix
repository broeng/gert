{
  description = "Gert - a native desktop client for Gerrit Code Review";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        lib = pkgs.lib;

        toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        # Libraries GPUI loads or links at runtime. Keep in sync with the
        # devShell (it uses this same list).
        runtimeLibs = with pkgs; [
          fontconfig
          freetype
          libxkbcommon
          vulkan-loader
          wayland
          libx11
          libxcb
          libxcursor
          libxi
          libxrandr
        ];

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [ pkg-config makeWrapper ];
          buildInputs = runtimeLibs;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        gert = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "gert";
          cargoExtraArgs = "--package gert";
          doCheck = false;
          postFixup = ''
            wrapProgram $out/bin/gert \
              --suffix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
          '';
        });

        # Local development Gerrit (dev/README.md). Kept out of
        # `packages.default` and the devShell: the JDK and the Gerrit war are
        # large and only needed when working on write features.
        mkDevGerrit = name: subcommand: pkgs.writeShellApplication {
          inherit name;
          runtimeInputs = with pkgs; [
            jdk21_headless # Gerrit 3.14 needs Java 21
            openssh # `gerrit init` shells out to ssh-keygen
            git
            curl
            jq
            coreutils
            findutils
            gawk
            gnugrep
            gnused
          ];
          # The scripts are plain bash, also runnable from a checkout.
          checkPhase = "";
          text = ''
            export GERT_GERRIT_WAR=${pkgs.gerrit}/webapps/gerrit-${pkgs.gerrit.version}.war
            exec bash ${./dev}/dev-gerrit.sh ${subcommand} "$@"
          '';
        };
        devGerrit = mkDevGerrit "dev-gerrit" "run";
      in
      {
        packages = {
          default = gert;
          dev-gerrit = devGerrit;
        };

        apps = {
          default = flake-utils.lib.mkApp { drv = gert; };
          dev-gerrit = flake-utils.lib.mkApp { drv = devGerrit; };
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [ pkg-config ] ++ runtimeLibs;
          RUST_BACKTRACE = "1";
          LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
        };
      });
}
