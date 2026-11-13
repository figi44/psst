#!/usr/bin/env bash
# Offscreen Wayland sandbox for psst tests. Never touches the live session.
#
#   sandbox.sh [--compositor niri|sway] [--keep] [--] [command [args...]]
#
# Starts headless sway (WLR_BACKENDS=headless, pixman) in a private runtime
# dir. With --compositor niri (default) it then starts the nested niri from
# PATH inside that sway, rendering with mesa llvmpipe (never the GPU), and
# runs <command> (default: $SHELL) as a client of the nested niri. With
# --compositor sway the command is a client of the headless sway itself
# (no niri IPC).
#
# Clients get WAYLAND_DISPLAY, NIRI_SOCKET, XDG_RUNTIME_DIR, XDG_STATE_HOME,
# XDG_CONFIG_HOME and PSST_SANDBOX_ROOT pointing into the sandbox,
# WAYLAND_SOCKET and DISPLAY cleared, the session bus disabled, and guard.sh
# must pass before the command runs. PSST_SANDBOX_HOST_SOCKET is the
# headless sway's socket (niri's host, used to simulate physical keys);
# PSST_SANDBOX_SWAY_PID and PSST_SANDBOX_NIRI_PID are the compositors' pids.
#
# Env overrides: PSST_SWAY, PSST_NIRI (binaries), PSST_MESA_EGL_VENDOR (glvnd
# json), PSST_SANDBOX_TMP (parent dir, default $TMPDIR or /tmp; socket paths
# must stay under 108 bytes), PSST_SANDBOX_LOGS (copy logs there on exit),
# PSST_NIRI_LOG (RUST_LOG for niri, default niri=info).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
compositor=niri
keep=0
while [ $# -gt 0 ]; do
  case "$1" in
    --compositor) compositor=$2; shift 2 ;;
    --keep) keep=1; shift ;;
    --) shift; break ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    -*) echo "sandbox: unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
case "$compositor" in niri|sway) ;; *) echo "sandbox: --compositor must be niri or sway" >&2; exit 2 ;; esac
[ $# -gt 0 ] || set -- "${SHELL:-bash}"

say() { echo "sandbox: $*" >&2; }
die() { say "ERROR: $*"; exit 1; }

# Remember the live session's sockets before anything is overwritten, so the
# guard can compare against them.
uid=$(id -u)
live_rt=${XDG_RUNTIME_DIR:-/run/user/$uid}
live_wl=""
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
  case "$WAYLAND_DISPLAY" in /*) live_wl=$WAYLAND_DISPLAY ;; *) live_wl=$live_rt/$WAYLAND_DISPLAY ;; esac
fi
live_niri=${NIRI_SOCKET:-}

nix_out() { nix build --no-link --print-out-paths "$1" | head -n1; }
sway_bin=${PSST_SWAY:-$(command -v sway || true)}
[ -n "$sway_bin" ] || sway_bin="$(nix_out nixpkgs#sway)/bin/sway"
niri_bin=""
mesa_vendor=""
if [ "$compositor" = niri ]; then
  niri_bin=${PSST_NIRI:-$(command -v niri || true)}
  [ -n "$niri_bin" ] || niri_bin="$(nix_out nixpkgs#niri)/bin/niri"
  mesa_vendor=${PSST_MESA_EGL_VENDOR:-}
  if [ -z "$mesa_vendor" ]; then
    if [ -e /run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json ]; then
      mesa_vendor=/run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json
    else
      mesa_vendor="$(nix_out nixpkgs#mesa)/share/glvnd/egl_vendor.d/50_mesa.json"
    fi
  fi
  [ -e "$mesa_vendor" ] || die "mesa EGL vendor file $mesa_vendor not found"
fi

base=${PSST_SANDBOX_TMP:-${TMPDIR:-/tmp}}
root=$(mktemp -d "$base/psst-sbx.XXXXXX")
root=$(realpath "$root")
[ ${#root} -le 60 ] || { rm -rf "$root"; die "sandbox root $root is too long for Unix socket paths"; }
case "$root/" in "/run/user/"*) rm -rf "$root"; die "sandbox root must not be in /run/user" ;; esac
mkdir -p "$root"/{host,run,state,config,cache,home,logs}
chmod 700 "$root"/host "$root"/run

sway_pid=""
niri_pid=""
cleanup() {
  local status=$?
  [ -n "$niri_pid" ] && kill "$niri_pid" 2>/dev/null && wait "$niri_pid" 2>/dev/null
  [ -n "$sway_pid" ] && kill "$sway_pid" 2>/dev/null && wait "$sway_pid" 2>/dev/null
  if [ -n "${PSST_SANDBOX_LOGS:-}" ]; then
    mkdir -p "$PSST_SANDBOX_LOGS" && cp "$root"/logs/* "$PSST_SANDBOX_LOGS"/ 2>/dev/null || true
  fi
  if [ $status -ne 0 ]; then
    for f in "$root"/logs/*.log; do [ -s "$f" ] && { say "--- tail of $f"; tail -n 15 "$f" >&2; }; done
  fi
  if [ $keep -eq 1 ]; then say "kept $root"; else rm -rf "$root"; fi
  exit $status
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# Wait for a socket matching a glob to appear; prints its path.
wait_sock() {
  local pattern=$1 pid=$2 i s
  for i in $(seq 100); do
    for s in $pattern; do
      case "$s" in *.lock) continue ;; esac
      [ -S "$s" ] && { echo "$s"; return 0; }
    done
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 0.05
  done
  return 1
}

# Compositors get a clean environment: nothing from the live session.
base_env=(HOME="$root/home" PATH="$PATH" LANG=C.UTF-8
  XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state" XDG_CACHE_HOME="$root/cache"
  DBUS_SESSION_BUS_ADDRESS=disabled: NO_AT_BRIDGE=1)

env -i "${base_env[@]}" XDG_RUNTIME_DIR="$root/host" \
  WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
  "$sway_bin" -c "$here/sway.conf" >"$root/logs/sway.log" 2>&1 &
sway_pid=$!
host_sock=$(wait_sock "$root/host/wayland-*" "$sway_pid") || die "headless sway did not start"
case "$host_sock" in "$root"/*) ;; *) die "host socket $host_sock is outside the sandbox" ;; esac

if [ "$compositor" = niri ]; then
  # The nested niri's own connection to its host is the one exemption from the
  # guard; it is an absolute path to the sandbox's sway, never the live display.
  env -i "${base_env[@]}" XDG_RUNTIME_DIR="$root/run" WAYLAND_DISPLAY="$host_sock" \
    __EGL_VENDOR_LIBRARY_FILENAMES="$mesa_vendor" LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
    RUST_LOG="${PSST_NIRI_LOG:-niri=info}" \
    "$niri_bin" -c "$here/niri.kdl" >"$root/logs/niri.log" 2>&1 &
  niri_pid=$!
  client_sock=$(wait_sock "$root/run/wayland-*" "$niri_pid") || die "nested niri did not start"
  niri_sock=$(wait_sock "$root/run/niri.*.sock" "$niri_pid") || die "nested niri IPC socket missing"
  # Never the GPU: refuse if niri mapped the NVIDIA driver or opened a GPU node.
  if grep -q 'libnvidia' "/proc/$niri_pid/maps" 2>/dev/null; then die "nested niri loaded the NVIDIA driver"; fi
  if ls -l "/proc/$niri_pid/fd" 2>/dev/null | grep -qE '/dev/(nvidia|dri/)'; then die "nested niri opened a GPU device"; fi
  client_rt="$root/run"
else
  client_sock=$host_sock
  niri_sock=""
  client_rt="$root/host"
fi

# Client environment: the caller's env minus anything that reaches a session.
unset WAYLAND_SOCKET DISPLAY NIRI_SOCKET SWAYSOCK I3SOCK XAUTHORITY HYPRLAND_INSTANCE_SIGNATURE
export DBUS_SESSION_BUS_ADDRESS=disabled: NO_AT_BRIDGE=1
export XDG_RUNTIME_DIR="$client_rt" WAYLAND_DISPLAY="${client_sock##*/}"
export XDG_STATE_HOME="$root/state" XDG_CONFIG_HOME="$root/config"
[ -n "$niri_sock" ] && export NIRI_SOCKET="$niri_sock"
export PSST_SANDBOX_ROOT="$root" PSST_SANDBOX_COMPOSITOR="$compositor" PSST_SANDBOX_HOST_SOCKET="$host_sock"
export PSST_SANDBOX_SWAY_PID="$sway_pid" PSST_SANDBOX_NIRI_PID="$niri_pid"
export PSST_LIVE_WAYLAND_SOCKET="$live_wl" PSST_LIVE_NIRI_SOCKET="$live_niri"

"$here/guard.sh" || die "guard refused the client environment"
say "$compositor ready in $root (WAYLAND_DISPLAY=$WAYLAND_DISPLAY${niri_sock:+, NIRI_SOCKET=$niri_sock})"
set +e
"$@"
status=$?
set -e
exit $status
