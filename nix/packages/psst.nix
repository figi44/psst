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

  # As in the dev shell: PipeWire and ALSA for audio, libopus for audio on the wire, D-Bus for
  # asking RTKit for real-time priority, Wayland and xkbcommon for the overlay and the virtual
  # keyboard.
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

  # The workspace's tests run in CI and the dev shell (cargo nextest); here the binary only has
  # to start.
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
