{
  lib,
  rustPlatform,
  pkg-config,
  libopus,
  sqlite,
  versionCheckHook,
}:
let
  workspace = import ./workspace.nix { inherit lib; };
in
rustPlatform.buildRustPackage {
  pname = "psst-server";
  inherit (workspace) version src cargoLock;

  cargoBuildFlags = [
    "--package"
    "psst-server"
  ];

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];
  buildInputs = [
    libopus
    sqlite
  ];

  # The workspace's tests run in CI (cargo nextest); here the binary only has to start.
  doCheck = false;
  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];
  versionCheckProgramArg = "--version";

  meta = {
    description = "The psst dictation server: sessions, the engine supervisor and the text pipeline";
    homepage = "https://github.com/figi44/psst";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "psst-server";
  };
}
