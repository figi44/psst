{
  description = "psst: self-hosted dictation for Linux";

  # Pinned, so builds hit derivations the binary caches already have.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/1d060086d60cb76a40cf37109f979d88a5bd6396";
  # Only for evaluating the Home Manager module in `checks`.
  inputs.home-manager = {
    url = "github:nix-community/home-manager/b787c48e2ec57a9d098f5c2721882a3bc1e16f97";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  # Only for the android shell, whose Rust needs std for the Android targets: nixpkgs' rustc is
  # built from source with the host's std alone, and official std tarballs don't load into it.
  # rust-overlay over fenix: it carries every release's hashes, so the toolchain follows nixpkgs'
  # rustc version with no hash to update, and it builds on our nixpkgs instead of importing its own.
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay/b70538330f20adc9665afa2976471eb5b85ebd1f";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      home-manager,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      # `allowUnfree` for the CUDA engine.
      pkgsFor = lib.genAttrs systems (
        system:
        import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [ self.overlays.default ];
        }
      );
      forAllSystems = f: lib.genAttrs systems (system: f pkgsFor.${system});
    in
    {
      overlays.default = import ./nix/overlay.nix;
      nixosModules.psst-server = import ./nix/modules/nixos.nix self.overlays.default;
      homeManagerModules.psst = import ./nix/modules/home-manager.nix self.overlays.default;

      packages = forAllSystems (pkgs: {
        inherit (pkgs) psst psst-server psst-engine-faster-whisper-cuda;
        default = pkgs.psst;
      });

      devShells = forAllSystems (
        pkgs:
        rec {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.cargo-nextest
              pkgs.just
              pkgs.sccache
              pkgs.mold
              pkgs.pkg-config
              (pkgs.python3.withPackages (ps: [ ps.pytest ]))
            ];
            nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];
            buildInputs = [
              pkgs.pipewire
              pkgs.alsa-lib
              pkgs.libopus
              pkgs.dbus
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.sqlite
            ];
            RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # bindgenHook hands bindgen NIX_CFLAGS_COMPILE, whose -frandom-seed is this shell's own
            # store hash, and the PipeWire bindings rebuild whenever bindgen's flags change: without
            # dropping it, any change to the shell rebuilds them (and misses CI's cache).
            #
            # sccache shares compiled crates across worktrees; CI has its own build cache.
            shellHook = ''
              export BINDGEN_EXTRA_CLANG_ARGS="$(sed -E 's/ ?-frandom-seed=[^ ]+//' <<<"$BINDGEN_EXTRA_CLANG_ARGS")"
              if [ -z "''${CI:-}" ]; then
                export RUSTC_WRAPPER=sccache
              fi
            '';
          };

          # GPU work runs only inside scripts/gpu-session.sh.
          gpu = default.overrideAttrs (old: {
            nativeBuildInputs = old.nativeBuildInputs ++ [
              (pkgs.callPackage ./nix/packages/psst-engine-dev.nix {
                engine = pkgs.psst-engine-faster-whisper-cuda;
              })
            ];
          });
        }
        # Google ships the NDK for x86_64 hosts only.
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isx86_64 {
          # For the Android app: `just android`.
          android =
            let
              default = self.devShells.${pkgs.stdenv.hostPlatform.system}.default;
              # The versions AGP defaults to: the SDK is read-only, so AGP can't fetch others.
              buildTools = "36.0.0";
              ndk = "28.2.13676358";
              sdk = (pkgs.androidenv.override { licenseAccepted = true; }).composeAndroidPackages {
                platformVersions = [ "36" ];
                buildToolsVersions = [ buildTools ];
                includeNDK = true;
                ndkVersion = ndk;
                # nixpkgs' cmake comes from the binary cache, and the SDK's would be another download.
                includeCmake = false;
                toolsVersion = null;
              };
              androidHome = "${sdk.androidsdk}/libexec/android-sdk";
              rust = (rust-overlay.lib.mkRustBin { } pkgs).stable.${pkgs.rustc.version}.minimal.override {
                extensions = [
                  "clippy"
                  "rustfmt"
                ];
                targets = [
                  "aarch64-linux-android"
                  "x86_64-linux-android"
                ];
              };
              rustTools = [
                pkgs.cargo
                pkgs.rustc
                pkgs.clippy
                pkgs.rustfmt
              ];
            in
            default.overrideAttrs (old: {
              nativeBuildInputs = [
                rust
                pkgs.cargo-ndk
                pkgs.gradle_9
                pkgs.gradle_9.jdk
                pkgs.cmake
              ]
              ++ lib.subtractLists rustTools old.nativeBuildInputs;
              # mold for the host only: the NDK's clang links the Android libraries.
              RUSTFLAGS = "";
              CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = old.RUSTFLAGS;
              ANDROID_HOME = androidHome;
              # AGP refuses to build when this names another SDK, as it does on GitHub's runners.
              ANDROID_SDK_ROOT = androidHome;
              ANDROID_NDK_HOME = "${androidHome}/ndk/${ndk}";
              JAVA_HOME = pkgs.gradle_9.jdk.home;
              # AGP otherwise downloads its own aapt2, which can't run on NixOS.
              GRADLE_OPTS = "-Dorg.gradle.project.android.aapt2FromMavenOverride=${androidHome}/build-tools/${buildTools}/aapt2";
            });
        }
      );

      # The CUDA engine is left out: it's unfree, and checked with
      # `nix build --dry-run .#psst-engine-faster-whisper-cuda` instead.
      checks = forAllSystems (pkgs: {
        devShell = self.devShells.${pkgs.stdenv.hostPlatform.system}.default;
        inherit (pkgs) psst psst-server;
        modules = import ./nix/tests/modules.nix {
          inherit
            self
            nixpkgs
            home-manager
            pkgs
            ;
        };
        vm = import ./nix/tests/vm.nix { inherit self pkgs; };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
