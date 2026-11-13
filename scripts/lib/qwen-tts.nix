# The Python that renders the Qwen3-TTS eval clips (`scripts/synth-evals.sh --qwen`). qwen-tts pins
# transformers 4.57.3, which nixpkgs carried only until January 2026, so the nixpkgs is pinned
# separately from the flake's.
let
  nixpkgs = builtins.getFlake "github:NixOS/nixpkgs/3ecb4acfab0e44aa78fc3080c79698ff3c2ec714";
  # torch-bin is the prebuilt CUDA wheel, which is unfree.
  pkgs = import nixpkgs {
    system = builtins.currentSystem;
    config.allowUnfree = true;
  };
  python = pkgs.python3.override {
    packageOverrides = final: prev: {
      # The prebuilt wheels, so that nothing compiles CUDA and accelerate gets the same torch.
      torch = final.torch-bin;
      torchaudio = final.torchaudio-bin;
      # Only qwen-tts's unused 25 Hz tokenizer calls it, but the package imports it.
      sox = final.buildPythonPackage rec {
        pname = "sox";
        version = "1.5.0";
        pyproject = true;
        src = final.fetchPypi {
          inherit pname version;
          hash = "sha256-Ese+W7H1SNiR/hHoLAjPXxoddOIlKY9gCC5a6yRpraA=";
        };
        build-system = [ final.setuptools ];
        dependencies = [
          final.numpy
          final.typing-extensions
        ];
        doCheck = false;
      };
      qwen-tts = final.buildPythonPackage rec {
        pname = "qwen-tts";
        version = "0.1.1";
        format = "wheel";
        src = final.fetchPypi {
          pname = "qwen_tts";
          inherit version format;
          dist = "py3";
          python = "py3";
          hash = "sha256-EaKQ2Nq8fvkakMVEeMirGbPtsdhcCIIxNyGJK9xK8V0=";
        };
        dependencies = [
          final.transformers
          final.accelerate
          final.librosa
          final.torch-bin
          final.torchaudio-bin
          final.soundfile
          final.sox
          final.onnxruntime
          final.einops
        ];
        # Only for its web demo.
        pythonRemoveDeps = [ "gradio" ];
        # librosa's numba functions cache their compiled code, which fails in the read-only store.
        preConfigure = ''
          export NUMBA_CACHE_DIR=$TMPDIR/numba
        '';
        pythonImportsCheck = [ "qwen_tts" ];
      };
    };
  };
in
pkgs.buildEnv {
  name = "psst-synth-qwen";
  paths = [
    pkgs.flac
    (python.withPackages (ps: [
      ps.qwen-tts
      ps.huggingface-hub
      ps.numpy
      ps.soxr
    ]))
  ];
}
