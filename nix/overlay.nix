# `overlays.default`. It never refers to its own attributes through `final`, so the modules can
# apply it to the host's pkgs as a plain function (`overlay pkgs pkgs`) without changing
# `nixpkgs.overlays` or instantiating nixpkgs again.
final: prev: {
  psst = final.callPackage ./packages/psst.nix { };
  psst-server = final.callPackage ./packages/psst-server.nix { };
}
