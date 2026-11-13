{
  writeShellApplication,
  git,
  engine,
}:
writeShellApplication {
  name = "psst-engine-dev";
  runtimeInputs = [ git ];
  text = ''
    root=''${PSST_WORKTREE:-$(git rev-parse --show-toplevel)}
    export PYTHONPATH="$root/engines/faster-whisper''${PYTHONPATH:+:$PYTHONPATH}"
    export LD_LIBRARY_PATH="${engine.libraryPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    exec ${engine.pythonEnv.interpreter} -m psst_engine_faster_whisper "$@"
  '';
}
