# The flake exports only the CUDA build; `cudaSupport` stays for other hosts.
{
  lib,
  config,
  python3,
  ctranslate2,
  cudaPackages,
  cudaSupport ? config.cudaSupport,
}:
let
  # With `allowUnfree`, the C++ library comes from the NixOS CUDA and Flox caches instead of
  # compiling. Its Python bindings build locally: the caches build them with `cudaSupport`, whose
  # CUDA torch test input changes their hash.
  python = python3.override {
    packageOverrides =
      final: prev:
      lib.optionalAttrs cudaSupport {
        ctranslate2 = prev.ctranslate2.override {
          ctranslate2-cpp = ctranslate2.override { withCUDA = true; };
        };
      };
  };

  # /run/opengl-driver/lib holds the driver's libcuda.
  libraryPath = lib.optionalString cudaSupport "/run/opengl-driver/lib:${
    lib.makeLibraryPath [
      cudaPackages.libcublas
      cudaPackages.cudnn
    ]
  }";

  dependencies = ps: [
    ps.faster-whisper
    ps.huggingface-hub
  ];

  root = ../../engines/faster-whisper;
  pyproject = lib.importTOML (root + "/pyproject.toml");
in
python.pkgs.buildPythonApplication {
  pname = "psst-engine-faster-whisper";
  inherit (pyproject.project) version;
  pyproject = true;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/pyproject.toml")
      (root + "/psst_engine_faster_whisper")
    ];
  };

  build-system = [ python.pkgs.setuptools ];
  dependencies = dependencies python.pkgs;

  makeWrapperArgs = lib.optionals cudaSupport [
    "--prefix"
    "LD_LIBRARY_PATH"
    ":"
    libraryPath
  ];

  # pytest runs in CI against a stub model; here the package only has to import.
  pythonImportsCheck = [ "psst_engine_faster_whisper" ];

  passthru = {
    inherit python libraryPath cudaSupport;
    # Without the engine itself, for running a worktree's copy of it (`psst-engine-dev`).
    pythonEnv = python.withPackages dependencies;
  };

  meta = {
    description = "faster-whisper engine for the psst dictation server";
    homepage = "https://github.com/figi44/psst";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "psst-engine-faster-whisper";
  };
}
