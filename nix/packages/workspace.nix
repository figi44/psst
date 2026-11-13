# What the Rust packages share: the Cargo workspace's sources, its version and its lock file.
# Only the files cargo reads are included, so editing docs, the engine or the Nix files doesn't
# rebuild the binaries. Crates come from Cargo.lock (importCargoLock), so adding a dependency
# needs no hash here.
{ lib }:
let
  root = ../..;
in
{
  inherit ((lib.importTOML (root + "/Cargo.toml")).workspace.package) version;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/crates")
      # The web UI's templates and assets, embedded in psst-server.
      (lib.fileset.maybeMissing (root + "/web"))
    ];
  };

  cargoLock.lockFile = root + "/Cargo.lock";
}
