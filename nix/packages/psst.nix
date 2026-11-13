{
  lib,
  rustPlatform,
  pkg-config,
  alsa-lib,
  dbus,
  libopus,
  pipewire,
  wayland,
  libxkbcommon,
  versionCheckHook,
}:
let
  workspace = import ./workspace.nix { inherit lib; };
in
rustPlatform.buildRustPackage {
  pname = "psst";
  inherit (workspace) version src cargoLock;

  cargoBuildFlags = [
    "--package"
    "psst"
  ];

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];
  buildInputs = [
    alsa-lib
    dbus
    libopus
    pipewire
    wayland
    libxkbcommon
  ];

  # The workspace's tests run in CI (cargo nextest); here the binary only has to start.
  doCheck = false;
  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];
  versionCheckProgramArg = "--version";

  meta = {
    description = "Dictation for Wayland, transcribed by a psst server";
    homepage = "https://github.com/figi44/psst";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "psst";
  };
}
