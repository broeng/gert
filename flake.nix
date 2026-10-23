{
  description = "Gert - a native desktop client for Gerrit Code Review";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Only used by the `nixgl` app, for Nix on non-NixOS Linux hosts.
    nixGL = {
      url = "github:nix-community/nixGL";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay, nixGL, ... }:
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
        devGerritSeed = mkDevGerrit "dev-gerrit-seed" "seed";

        # gert under nixGL's Intel wrappers. Nix-built GPU apps
        # on non-NixOS hosts cannot find the host's Vulkan/GL drivers; see the
        # README. `nixVulkanIntel`/`nixGLIntel` need no --impure; the NVIDIA
        # variants do and are not covered here.
        gertNixGL = pkgs.writeShellScriptBin "gert-nixgl" ''
          exec ${nixGL.packages.${system}.nixVulkanIntel}/bin/nixVulkanIntel \
            ${nixGL.packages.${system}.nixGLIntel}/bin/nixGLIntel \
            ${gert}/bin/gert "$@"
        '';
      in
      {
        packages = {
          default = gert;
          dev-gerrit = devGerrit;
          dev-gerrit-seed = devGerritSeed;
        };

        apps = {
          default = flake-utils.lib.mkApp { drv = gert; };
          dev-gerrit = flake-utils.lib.mkApp { drv = devGerrit; };
          dev-gerrit-seed = flake-utils.lib.mkApp { drv = devGerritSeed; };
        } // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          nixgl = flake-utils.lib.mkApp { drv = gertNixGL; };
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [ pkg-config ] ++ runtimeLibs;
          RUST_BACKTRACE = "1";
          LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
        };
      });
}
