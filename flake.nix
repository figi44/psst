{
  description = "psst: self-hosted dictation for Linux";

  # Pinned to the revision the host config uses, so builds hit derivations already in the local store and
  # in the caches the host config uses (plan section 11).
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/1d060086d60cb76a40cf37109f979d88a5bd6396";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      # nixpkgs as the host config configures it (only `allowUnfree`, for the CUDA engine), plus psst.
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

      packages = forAllSystems (pkgs: {
        inherit (pkgs) psst psst-server psst-engine-faster-whisper-cuda;
        default = pkgs.psst;
      });

      devShells = forAllSystems (pkgs: rec {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.cargo-nextest
            pkgs.sccache
            pkgs.mold
            pkgs.pkg-config
            (pkgs.python3.withPackages (ps: [ ps.pytest ]))
          ];
          nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];
          # The system libraries the crates link: PipeWire and ALSA for audio, Wayland and
          # xkbcommon for the overlay and virtual keyboard, SQLite for the server.
          buildInputs = [
            pkgs.pipewire
            pkgs.alsa-lib
            pkgs.wayland
            pkgs.libxkbcommon
            pkgs.sqlite
          ];
          RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          # sccache shares compiled crates across worktrees (~/.cache/sccache). CI uses its own
          # build cache instead.
          shellHook = ''
            if [ -z "''${CI:-}" ]; then
              export RUSTC_WRAPPER=sccache
            fi
          '';
        };

        # The default shell plus `psst-engine-dev`, which runs this worktree's engine on the GPU.
        # GPU work runs only inside scripts/gpu-session.sh.
        gpu = default.overrideAttrs (old: {
          nativeBuildInputs = old.nativeBuildInputs ++ [
            (pkgs.callPackage ./nix/packages/psst-engine-dev.nix {
              engine = pkgs.psst-engine-faster-whisper-cuda;
            })
          ];
        });
      });

      checks = forAllSystems (pkgs: {
        devShell = self.devShells.${pkgs.stdenv.hostPlatform.system}.default;
        inherit (pkgs) psst psst-server;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
