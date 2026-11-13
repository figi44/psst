#!/usr/bin/env bash
# `scripts/gpu-session.sh true` repairs a killed run without doing anything else.
# shellcheck disable=SC2329 # cleanup and on_signal run from traps
set -euo pipefail

log() { printf 'gpu-session: %s\n' "$*" >&2; }

if (($# == 0)); then
    echo "usage: $0 <command> [args...]" >&2
    exit 64
fi

# A nested call: the session that started it already has the GPU.
if [[ -n ${PSST_GPU_SESSION:-} ]] && kill -0 "$PSST_GPU_SESSION" 2>/dev/null; then
    exec "$@"
fi

cache_dir=${PSST_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/psst}
lock_file=$cache_dir/gpu.lock
marker=$cache_dir/gpu-session.marker

# `psst-server model` needs this user in the server's local_users. A model that's unloaded (by gaming mode, say) or
# failed is left alone, and isn't loaded afterwards.
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

psst_unloaded=0

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
# The command inherits the lock fd on purpose: if this script is killed, the GPU stays locked until the command ends.

started=$(date --iso-8601=seconds)

if [[ -e $marker ]]; then
    log "repairing after a session that was killed (pid $(marker_get pid), started $(marker_get started))"
    # Carry the unload over: this session needs the GPU anyway, and loads the model again when it ends.
    [[ $(marker_get psst) == unloaded ]] && psst_unloaded=1
fi
write_marker "$@"

child=

descendants() {
    local pid
    for pid in $(pgrep -P "$1"); do
        echo "$pid"
        descendants "$pid"
    done
}

running() {
    local state
    state=$(ps -o stat= -p "$1" 2>/dev/null) && [[ $state != Z* ]]
}

# Waits until the command and everything it started are gone, so nothing still holds the GPU when the psst model is
# loaded again.
stop_command() {
    local pids pid deadline signal
    # Collected before anything is signalled: a process whose parent dies is reparented out of the tree (a wrapper
    # script killed before its Python worker, for example).
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

# In the background, so a signal to this script is handled at once instead of after the command exits. Background
# jobs ignore SIGINT in a non-interactive shell, so Ctrl-C reaches the command as the SIGTERM sent by cleanup.
export PSST_GPU_SESSION=$$
"$@" <&0 &
child=$!
rc=0
wait "$child" || rc=$?
child=
exit "$rc"
