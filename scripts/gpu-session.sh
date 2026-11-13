#!/usr/bin/env bash
# Run a command with the GPU to itself.
#
#   scripts/gpu-session.sh <command> [args...]
#
# - Takes an exclusive lock on ~/.cache/psst/gpu.lock, so GPU jobs from parallel worktrees run one at a time.
# - Unloads localwhisper's model with `localwhisper model unload` if it's loaded, and never reloads it. That happens once,
#   by hand (`localwhisper model reload`), when the work ends. No other localwhisper command is ever run.
# - Unloads a psst server's model if one is loaded, and loads it again when the command ends, also on Ctrl-C,
#   SIGTERM or SIGHUP.
# - Records what it unloaded in a marker file next to the lock. A run that was killed outright is repaired by the next
#   one, from the marker.
# - Exits with the command's status. A nested call (from inside a session) just runs the command.
#
# `scripts/gpu-session.sh true` repairs a killed run without doing anything else.
#
# Environment:
#   PSST_CACHE_DIR           where the lock and marker live (default: ${XDG_CACHE_HOME:-~/.cache}/psst)
#   PSST_GPU_LOCK_TIMEOUT    seconds to wait for the lock (default: wait as long as it takes)
#   PSST_GPU_UNLOAD_TIMEOUT  seconds to wait for localwhisper to confirm the unload (default: 30)
# shellcheck disable=SC2329 # cleanup and on_signal run from traps
set -euo pipefail

log() { printf 'gpu-session: %s\n' "$*" >&2; }

if (($# == 0)); then
    echo "usage: $0 <command> [args...]" >&2
    exit 64
fi

# A command started by a session may call gpu-session.sh again; it already has the GPU.
if [[ -n ${PSST_GPU_SESSION:-} ]] && kill -0 "$PSST_GPU_SESSION" 2>/dev/null; then
    exec "$@"
fi

cache_dir=${PSST_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/psst}
lock_file=$cache_dir/gpu.lock
marker=$cache_dir/gpu-session.marker

# --- localwhisper ----------------------------------------------------------------------------------------------------
# localwhisper keeps its runtime files in $XDG_RUNTIME_DIR/localwhisper (or $TMPDIR/localwhisper-<uid>). It creates
# `model_unloaded` there once `localwhisper model unload` has finished, and deletes it on reload or restart.

localwhisper_dir() {
    if [[ -n ${XDG_RUNTIME_DIR:-} ]]; then
        printf '%s/localwhisper' "$XDG_RUNTIME_DIR"
    else
        printf '%s/localwhisper-%s' "${TMPDIR:-/tmp}" "$(id -u)"
    fi
}

localwhisper_loaded() {
    local dir
    dir=$(localwhisper_dir)
    command -v localwhisper >/dev/null || return 1
    [[ -p $dir/recording_control ]] || return 1 # its control FIFO: the service has run in this session
    [[ ! -e $dir/model_unloaded ]] || return 1  # already unloaded (by an earlier session, or by hand)
    pgrep -u "$(id -u)" -f 'localwhisper/lib/main\.py' >/dev/null # and the service is running now
}

localwhisper_unload() {
    local dir deadline
    dir=$(localwhisper_dir)
    log "unloading localwhisper's model; it stays unloaded until you run 'localwhisper model reload'"
    localwhisper model unload >&2 || return 1
    # The service refuses (with only a desktop notification) while it's recording or transcribing.
    deadline=$((SECONDS + ${PSST_GPU_UNLOAD_TIMEOUT:-30}))
    until [[ -e $dir/model_unloaded ]]; do
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

# --- psst-server --------------------------------------------------------------------------------------------------
# psst-server doesn't exist yet (Phase 1), so no psst model is ever loaded and these hooks do nothing. Everything
# around them (marker, restore on exit, repair) is already in place: wire them to `psst-server model status`,
# `model unload` and `model load` (plan 6.3) and the rest works unchanged.

psst_model_loaded() { return 1; }
psst_model_unload() { return 0; }
psst_model_load() { return 0; }

# --- marker -------------------------------------------------------------------------------------------------------

psst_unloaded=0      # this session (or a killed one before it) unloaded the psst model, so it must be loaded again
localwhisper_unloaded=0 # recorded for the record only: localwhisper is never reloaded here

write_marker() {
    {
        echo "# gpu-session.sh: what this session unloaded. Removed when the session ends."
        echo "pid=$$"
        echo "started=$started"
        echo "command=${*@Q}"
        echo "psst=$( ((psst_unloaded)) && echo unloaded || echo untouched)"
        echo "localwhisper=$( ((localwhisper_unloaded)) && echo unloaded || echo untouched)"
    } >"$marker.tmp"
    mv -f "$marker.tmp" "$marker"
}

marker_get() { sed -n "s/^$1=//p" "$marker" | tail -n 1; }

# --- lock ---------------------------------------------------------------------------------------------------------

mkdir -p "$cache_dir"
exec {lock_fd}>>"$lock_file"
if ! flock -n "$lock_fd"; then
    owner=$( [[ -e $marker ]] && echo "pid $(marker_get pid): $(marker_get command)" || echo unknown)
    log "waiting for the GPU lock (held by $owner)"
    if ! flock ${PSST_GPU_LOCK_TIMEOUT:+-w "$PSST_GPU_LOCK_TIMEOUT"} "$lock_fd"; then
        log "error: timed out waiting for the GPU lock"
        exit 75
    fi
fi
# The lock fd is inherited by the command on purpose: if this script is killed while the command still runs, the
# GPU stays locked until the command ends too.

started=$(date --iso-8601=seconds)

if [[ -e $marker ]]; then
    log "repairing after a session that was killed (pid $(marker_get pid), started $(marker_get started))"
    # Carry the unload over: this session needs the GPU anyway, and loads the psst model again when it ends.
    [[ $(marker_get psst) == unloaded ]] && psst_unloaded=1
    [[ $(marker_get localwhisper) == unloaded ]] && localwhisper_unloaded=1
fi
write_marker "$@"

child=

cleanup() {
    local rc=$?
    trap - EXIT INT TERM HUP
    if [[ -n $child ]] && kill -0 "$child" 2>/dev/null; then
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
    fi
    if ((psst_unloaded)); then
        log "loading the psst model again"
        if psst_model_load; then
            psst_unloaded=0
        else
            write_marker "${cmd[@]}"
            log "warning: couldn't load the psst model; the next gpu-session.sh run retries"
            exit "$rc"
        fi
    fi
    rm -f "$marker"
    if [[ -e $(localwhisper_dir)/model_unloaded ]]; then
        log "localwhisper's model is still unloaded; run 'localwhisper model reload' when the GPU work is done"
    fi
    exit "$rc"
}

on_signal() {
    log "got SIG$1, stopping the command"
    exit $((128 + $2))
}

cmd=("$@")
trap cleanup EXIT
trap 'on_signal INT 2' INT
trap 'on_signal TERM 15' TERM
trap 'on_signal HUP 1' HUP

# --- unload -------------------------------------------------------------------------------------------------------

if psst_model_loaded; then
    psst_unloaded=1
    write_marker "$@" # before unloading, so a kill in between still gets it restored
    log "unloading the psst model"
    psst_model_unload || {
        log "error: couldn't unload the psst model"
        exit 75
    }
fi

if localwhisper_loaded; then
    if ! localwhisper_unload; then
        log "error: localwhisper didn't unload its model (is it recording or transcribing?)"
        exit 75
    fi
    localwhisper_unloaded=1
    write_marker "$@"
fi

if command -v nvidia-smi >/dev/null; then
    log "GPU memory in use before the command: $(nvidia-smi --query-gpu=memory.used,memory.total \
        --format=csv,noheader 2>/dev/null | head -n 1)"
fi

# --- run ----------------------------------------------------------------------------------------------------------
# In the background, so a signal to this script is handled at once instead of after the command exits. Background
# jobs ignore SIGINT in a non-interactive shell, so Ctrl-C reaches the command as the SIGTERM sent by cleanup.

export PSST_GPU_SESSION=$$
"$@" <&0 &
child=$!
rc=0
wait "$child" || rc=$?
child=
exit "$rc"
