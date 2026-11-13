# The faster-whisper engine (engines/faster-whisper). The flake exports only the CUDA build,
# `psst-engine-faster-whisper-cuda`; `cudaSupport` stays for other hosts.
{
  lib,
  config,
  python3,
  ctranslate2,
  cudaPackages,
  cudaSupport ? config.cudaSupport,
}:
let
  # ctranslate2 with CUDA. With the pinned nixpkgs and `allowUnfree`, it's the derivation the
  # Flox cache serves, so it is substituted rather than compiled.
  python = python3.override {
    packageOverrides =
      final: prev:
      lib.optionalAttrs cudaSupport {
        ctranslate2 = prev.ctranslate2.override {
          ctranslate2-cpp = ctranslate2.override { withCUDA = true; };
        };
      };
  };

  # The driver's libcuda, then cuBLAS and cuDNN.
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

  # pytest runs in CI and the dev shell against a stub model; here the package only has to
  # import.
  pythonImportsCheck = [ "psst_engine_faster_whisper" ];

  passthru = {
    inherit python libraryPath cudaSupport;
    # The engine's dependencies without the engine itself, for running a worktree's copy of
    # it (`psst-engine-dev` in `devShells.gpu`).
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
