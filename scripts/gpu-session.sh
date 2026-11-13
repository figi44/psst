#!/usr/bin/env bash
# Run a command with the GPU to itself.
#
#   scripts/gpu-session.sh <command> [args...]
#
# - Takes an exclusive lock on ~/.cache/psst/gpu.lock, so GPU jobs from parallel worktrees run one at a time.
# - Unloads this machine's psst server's model if it's loaded (or on its way), and loads it again when the command
#   ends, also on Ctrl-C, SIGTERM or SIGHUP. On a signal, the command and every process under it are stopped first.
# - Records what it unloaded in a marker file next to the lock. A run that was killed outright is repaired by the next
#   one, from the marker.
# - Exits with the command's status. A nested call (from inside a session) just runs the command.
#
# `scripts/gpu-session.sh true` repairs a killed run without doing anything else.
#
# Environment:
#   PSST_CACHE_DIR           where the lock and marker live (default: ${XDG_CACHE_HOME:-~/.cache}/psst)
#   PSST_GPU_LOCK_TIMEOUT    seconds to wait for the lock (default: wait as long as it takes)
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

# --- psst-server --------------------------------------------------------------------------------------------------
# The server on this machine, through `psst-server model` on its socket, which needs this user in its local_users. A
# model that's loaded, loading or downloading holds the GPU or soon will, so it's unloaded. One that's unloaded (by
# gaming mode, say) or failed is left alone, and isn't loaded afterwards. Without psst-server on PATH there's nothing
# to unload; when the server can't be asked, the command runs anyway, with a warning.

psst_model_loaded() {
    command -v psst-server >/dev/null || return 1
    local status holds='"state": *"(loaded|loading|downloading)"'
    if ! status=$(psst-server model status --json 2>&1); then
        log "warning: couldn't ask psst-server about its model, so it stays as it is: $status"
        return 1
    fi
    [[ $status =~ $holds ]]
}
psst_model_unload() { psst-server model unload >/dev/null; }
psst_model_load() { psst-server model load >/dev/null; }

# --- marker -------------------------------------------------------------------------------------------------------

psst_unloaded=0 # this session (or a killed one before it) unloaded the psst model, so it must be loaded again

write_marker() {
    {
        echo "# gpu-session.sh: what this session unloaded. Removed when the session ends."
        echo "pid=$$"
        echo "started=$started"
        echo "command=${*@Q}"
        echo "psst=$( ((psst_unloaded)) && echo unloaded || echo untouched)"
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
fi
write_marker "$@"

child=

# Every process under this script. Collected before anything is signalled, because a process whose parent dies is
# reparented out of the tree (a wrapper script killed before its Python worker, for example).
descendants() {
    local pid
    for pid in $(pgrep -P "$1"); do
        echo "$pid"
        descendants "$pid"
    done
}

running() { # running <pid>: exists and isn't a zombie
    local state
    state=$(ps -o stat= -p "$1" 2>/dev/null) && [[ $state != Z* ]]
}

# Stops the command and everything it started, and waits until they're gone, so nothing still holds the GPU when the
# psst model is loaded again: SIGTERM, then SIGKILL after 10 s.
stop_command() {
    local pids pid deadline signal
    pids=$(descendants $$)
    [[ -n $pids ]] || return 0
    # shellcheck disable=SC2086 # one pid per word
    kill -TERM $pids 2>/dev/null || true
    for signal in KILL ''; do
        deadline=$((SECONDS + 10))
        for pid in $pids; do
            while running "$pid" && ((SECONDS < deadline)); do sleep 0.1; done
        done
        # shellcheck disable=SC2086
        [[ -z $signal ]] || kill "-$signal" $pids 2>/dev/null || true
    done
    wait "$child" 2>/dev/null || true
}

cleanup() {
    local rc=$?
    trap - EXIT INT TERM HUP
    if [[ -n $child ]]; then
        stop_command
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
