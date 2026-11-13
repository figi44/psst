# `psst-engine-dev` in `devShells.gpu`: runs engines/faster-whisper from a worktree, with the
# CUDA engine package's Python and library path. A dev server points `model.command` at it.
{
  writeShellApplication,
  git,
  engine,
}:
writeShellApplication {
  name = "psst-engine-dev";
  runtimeInputs = [ git ];
  text = ''
    # The worktree holding the current directory, unless PSST_WORKTREE names one.
    root=''${PSST_WORKTREE:-$(git rev-parse --show-toplevel)}
    export PYTHONPATH="$root/engines/faster-whisper''${PYTHONPATH:+:$PYTHONPATH}"
    export LD_LIBRARY_PATH="${engine.libraryPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    exec ${engine.pythonEnv.interpreter} -m psst_engine_faster_whisper "$@"
  '';
}
