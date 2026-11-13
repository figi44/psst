# `overlays.default`. It never refers to its own attributes through `final`, so the modules can
# apply it to the host's pkgs as a plain function (`overlay pkgs pkgs`) without changing
# `nixpkgs.overlays` or instantiating nixpkgs again.
final: prev:
let
  engine = final.callPackage ./packages/psst-engine-faster-whisper.nix { };
in
{
  psst = final.callPackage ./packages/psst.nix { };
  psst-server = final.callPackage ./packages/psst-server.nix { };
  # CUDA when the host sets `config.cudaSupport`.
  psst-engine-faster-whisper = engine;
  # Needs `allowUnfree`.
  psst-engine-faster-whisper-cuda = engine.override { cudaSupport = true; };
}
