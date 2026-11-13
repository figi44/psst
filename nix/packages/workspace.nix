{ lib }:
let
  root = ../..;
in
{
  inherit ((lib.importTOML (root + "/Cargo.toml")).workspace.package) version;

  # Only the files cargo reads, so editing docs, the engine or the Nix files doesn't rebuild the
  # binaries.
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
