#!/usr/bin/env bash
# Tests for scripts/gpu-session.sh, with a private cache directory and a fake psst-server first on PATH, so the real
# GPU lock and the real server's model are never touched. No GPU needed.
#
#   scripts/tests/gpu-session.sh
# shellcheck disable=SC2016,SC2034 # checks are single-quoted on purpose and expanded by the eval in check()
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/psst-gpu-session-test.XXXXXX")
trap 'rm -rf "$T"' EXIT
export PSST_CACHE_DIR=$T/cache
G=$REPO/scripts/gpu-session.sh
# The fake psst-server: `model status --json`, `model unload` and `model load` against the model state in $T/psst
# (no file: no server running). It logs each unload and load to $T/psst.log, and notes in $T/alive-at-load when a
# `sleep 36` is still running as the model is loaded.
mkdir -p "$T/bin"
cat > "$T/bin/psst-server" <<'EOF'
#!/usr/bin/env bash
if [[ ! -e $T/psst ]]; then
    echo "psst-server: cannot connect to /run/psst/client.sock (is psst-server running?)" >&2
    exit 1
fi
case "$1 $2" in
    "model status") printf '{\n  "name": "large-v3-turbo",\n  "state": "%s"\n}\n' "$(cat "$T/psst")" ;;
    "model unload") echo unloaded > "$T/psst"; echo unload >> "$T/psst.log" ;;
    "model load")
        if pgrep -f "^sleep 36$" >/dev/null; then echo alive >> "$T/alive-at-load"; fi
        echo loaded > "$T/psst"; echo load >> "$T/psst.log" ;;
    *) exit 64 ;;
esac
EOF
chmod +x "$T/bin/psst-server"
export PATH=$T/bin:$PATH T
pass=0 fail=0
check() { if eval "$2"; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1"; fail=$((fail+1)); fi; }

# 1. runs the command with a marker naming it, propagates exit status, removes the marker
$G bash -c 'cat "$PSST_CACHE_DIR/gpu-session.marker" > "$PSST_CACHE_DIR/seen"; exit 3' 2>/dev/null; rc=$?
check "exit status propagated" '[[ $rc == 3 ]]'
check "marker names the command" 'grep -q "^command=.*bash" "$PSST_CACHE_DIR/seen"'
check "marker removed" '[[ ! -e $PSST_CACHE_DIR/gpu-session.marker ]]'
# 3. stdin and stdout pass through
out=$(echo hello | $G cat 2>/dev/null)
check "stdin/stdout pass through" '[[ $out == hello ]]'
# 4. lock: a second session waits; with a timeout it gives up with 75
$G sleep 2 2>/dev/null & A=$!
sleep 0.5
PSST_GPU_LOCK_TIMEOUT=0.5 $G true 2>"$T/err4"; rc=$?
check "lock timeout exits 75" '[[ $rc == 75 ]]'
check "lock wait names the holder" 'grep -q "held by pid .*sleep" "$T/err4"'
start=$SECONDS; $G true 2>/dev/null; waited=$((SECONDS-start))
check "second session waited for the first" '[[ $waited -ge 1 ]]'
wait $A
set -m  # own process groups, so the jobs start with SIGINT not ignored (as in a terminal)
# 5. SIGTERM / SIGINT to the script stops the command and cleans up
$G sleep 30 2>/dev/null & A=$!
sleep 0.5; kill -TERM $A; wait $A; rc=$?
check "SIGTERM exits 143" '[[ $rc == 143 ]]'
check "child sleep 30 gone" '! pgrep -f "^sleep 30$" >/dev/null'
check "marker removed after SIGTERM" '[[ ! -e $PSST_CACHE_DIR/gpu-session.marker ]]'
$G sleep 31 2>/dev/null & A=$!
sleep 0.5; s0=$SECONDS; kill -INT $A; wait $A; rc=$?
check "SIGINT handled at once" '[[ $((SECONDS-s0)) -le 2 ]]'
check "SIGINT exits 130" '[[ $rc == 130 ]]'
check "child sleep 31 gone" '! pgrep -f "^sleep 31$" >/dev/null'
# A wrapper script with a worker under it (like bench/run.sh -> python): the worker is stopped too.
$G bash -c 'sleep 34; true' 2>/dev/null & A=$!
sleep 0.5; kill -TERM $A; wait $A
check "worker under a wrapper stopped on SIGTERM" '! pgrep -f "^sleep 34$" >/dev/null'
# 6. nested call runs directly
out=$($G bash -c "$G echo nested" 2>/dev/null)
check "nested call does not deadlock" '[[ $out == nested ]]'
# 7. the psst server's model (the fake psst-server): which states are unloaded, restore, repair
for state in loading downloading; do
    echo $state > "$T/psst"
    $G bash -c '[[ $(cat "$T/psst") == unloaded ]]' 2>/dev/null; rc=$?
    check "model $state: unloaded during the command, loaded after" '[[ $rc == 0 && $(cat "$T/psst") == loaded ]]'
done
for state in unloaded failed; do
    echo $state > "$T/psst"
    $G bash -c '[[ $(cat "$T/psst") == $1 ]]' _ $state 2>/dev/null; rc=$?
    check "model $state: left as it is" '[[ $rc == 0 && $(cat "$T/psst") == $state ]]'
done
rm "$T/psst"
out=$($G echo ran 2>"$T/err7"); rc=$?
check "without a server the command runs" '[[ $rc == 0 && $out == ran ]]'
check "without a server it warns" 'grep -q "warning: couldn.t ask psst-server" "$T/err7"'
: > "$T/psst.log"
echo loaded > "$T/psst"
$G bash -c '[[ $(cat "$T/psst") == unloaded ]]' 2>/dev/null; rc=$?
check "psst unloaded during the command" '[[ $rc == 0 ]]'
check "psst loaded again after" '[[ $(cat "$T/psst") == loaded ]]'
$G sleep 32 2>/dev/null & A=$!
sleep 0.5; s0=$SECONDS; kill -INT $A; wait $A
check "psst Ctrl-C handled at once" '[[ $((SECONDS-s0)) -le 2 ]]'
check "psst loaded again after Ctrl-C" '[[ $(cat "$T/psst") == loaded ]]'
# The psst model is loaded again only once the worker under a wrapper is gone.
$G bash -c 'sleep 36; true' 2>/dev/null & A=$!
sleep 0.5; kill -INT $A; wait $A
check "worker stopped before the psst model is loaded" '[[ ! -e $T/alive-at-load ]] && ! pgrep -f "^sleep 36$" >/dev/null'
# SIGKILL the script: the command keeps the lock until it ends; the next run repairs from the marker
$G sleep 3 2>/dev/null & A=$!
sleep 0.5; kill -KILL $A; wait $A 2>/dev/null
check "psst still unloaded after SIGKILL" '[[ $(cat "$T/psst") == unloaded ]]'
check "marker left behind" 'grep -q "^psst=unloaded" "$PSST_CACHE_DIR/gpu-session.marker"'
PSST_GPU_LOCK_TIMEOUT=0.3 $G true 2>/dev/null; rc=$?
check "orphaned command still holds the lock" '[[ $rc == 75 ]]'
$G bash -c '[[ $(cat "$T/psst") == unloaded ]]' 2>"$T/err7"; rc=$?
check "repair message" 'grep -q "repairing" "$T/err7"'
check "carried-over unload kept during next command" '[[ $rc == 0 ]]'
check "psst restored by the next run" '[[ $(cat "$T/psst") == loaded ]]'
check "marker gone after repair" '[[ ! -e $PSST_CACHE_DIR/gpu-session.marker ]]'
check "load/unload sequence" '[[ $(tr "\n" " " < "$T/psst.log") == "unload load unload load unload load unload load " ]]'
echo "passed $pass, failed $fail"
((fail == 0))
