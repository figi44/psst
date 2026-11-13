#!/usr/bin/env bash
# Refuses (exit 1) unless every client-facing variable points inside the
# sandbox root ($PSST_SANDBOX_ROOT). Run before every Wayland client.
#
# Checked: XDG_RUNTIME_DIR, WAYLAND_DISPLAY (resolved to its socket),
# NIRI_SOCKET, XDG_STATE_HOME, XDG_CONFIG_HOME must all be under the root;
# WAYLAND_SOCKET and DISPLAY must be unset; nothing may resolve into
# /run/user/<uid> or match the live sockets recorded by sandbox.sh
# (PSST_LIVE_WAYLAND_SOCKET, PSST_LIVE_NIRI_SOCKET).
set -u

fail() { echo "sandbox guard: REFUSING: $*" >&2; exit 1; }

root=${PSST_SANDBOX_ROOT:-}
[ -n "$root" ] || fail "PSST_SANDBOX_ROOT is not set"
[ -d "$root" ] || fail "PSST_SANDBOX_ROOT=$root is not a directory"
root=$(realpath -e "$root") || fail "cannot resolve PSST_SANDBOX_ROOT"
live_runtime="/run/user/$(id -u)"
case "$root/" in
  "$live_runtime"/*) fail "sandbox root $root is inside the live runtime dir" ;;
  /) fail "sandbox root is /" ;;
esac

# under <path> <name>: path must resolve inside the sandbox root.
under() {
  local p
  p=$(realpath -m -- "$1") || fail "$2: cannot resolve $1"
  case "$p/" in
    "$root"/*) ;;
    *) fail "$2=$1 is outside the sandbox root $root" ;;
  esac
  case "$p/" in
    "$live_runtime"/*) fail "$2=$1 resolves into the live runtime dir" ;;
  esac
}

[ -z "${WAYLAND_SOCKET+x}" ] || fail "WAYLAND_SOCKET is set"
[ -z "${DISPLAY+x}" ] || fail "DISPLAY is set (X11 would reach the live session)"

[ -n "${XDG_RUNTIME_DIR:-}" ] || fail "XDG_RUNTIME_DIR is not set"
under "$XDG_RUNTIME_DIR" XDG_RUNTIME_DIR

[ -n "${WAYLAND_DISPLAY:-}" ] || fail "WAYLAND_DISPLAY is not set"
case "$WAYLAND_DISPLAY" in
  /*) sock=$WAYLAND_DISPLAY ;;
  *) sock="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ;;
esac
under "$sock" WAYLAND_DISPLAY
[ -S "$sock" ] || fail "WAYLAND_DISPLAY socket $sock does not exist"

if [ -n "${NIRI_SOCKET:-}" ]; then
  under "$NIRI_SOCKET" NIRI_SOCKET
  [ -S "$NIRI_SOCKET" ] || fail "NIRI_SOCKET $NIRI_SOCKET does not exist"
fi
for v in XDG_STATE_HOME XDG_CONFIG_HOME; do
  [ -n "${!v:-}" ] || fail "$v is not set"
  under "${!v}" "$v"
done

# Same-file checks against the live sockets, in case of bind mounts or links.
same_file() { [ -e "$1" ] && [ -e "$2" ] && [ "$(stat -Lc %d:%i -- "$1")" = "$(stat -Lc %d:%i -- "$2")" ]; }
for live in "${PSST_LIVE_WAYLAND_SOCKET:-}" "${PSST_LIVE_NIRI_SOCKET:-}"; do
  [ -n "$live" ] || continue
  same_file "$sock" "$live" && fail "WAYLAND_DISPLAY is the live socket $live"
  [ -n "${NIRI_SOCKET:-}" ] && same_file "$NIRI_SOCKET" "$live" && fail "NIRI_SOCKET is the live socket $live"
done
exit 0
