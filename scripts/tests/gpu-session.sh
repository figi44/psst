#!/usr/bin/env bash
# Tests for scripts/gpu-session.sh, with a private cache directory, so the real GPU lock is never touched. No GPU
# needed.
#
#   scripts/tests/gpu-session.sh
# shellcheck disable=SC2016,SC2034 # checks are single-quoted on purpose and expanded by the eval in check()
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/psst-gpu-session-test.XXXXXX")
trap 'rm -rf "$T"' EXIT
export PSST_CACHE_DIR=$T/cache
G=$REPO/scripts/gpu-session.sh
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
# 7. psst restore and repair, with the hooks wired to a fake psst-server (state file)
P=$T/gpu-session-psst.sh
sed -e 's|^psst_model_loaded() { return 1; }|psst_model_loaded() { [[ $(cat "$T/psst") == loaded ]]; }|' \
    -e 's|^psst_model_unload() { return 0; }|psst_model_unload() { echo unloaded > "$T/psst"; echo unload >> "$T/psst.log"; }|' \
    -e 's|^psst_model_load() { return 0; }|psst_model_load() { echo loaded > "$T/psst"; echo load >> "$T/psst.log"; }|' "$G" > "$P"
chmod +x "$P"; export T
echo loaded > "$T/psst"
$P bash -c '[[ $(cat "$T/psst") == unloaded ]]' 2>/dev/null; rc=$?
check "psst unloaded during the command" '[[ $rc == 0 ]]'
check "psst loaded again after" '[[ $(cat "$T/psst") == loaded ]]'
$P sleep 32 2>/dev/null & A=$!
sleep 0.5; s0=$SECONDS; kill -INT $A; wait $A
check "psst Ctrl-C handled at once" '[[ $((SECONDS-s0)) -le 2 ]]'
check "psst loaded again after Ctrl-C" '[[ $(cat "$T/psst") == loaded ]]'
# The psst model is loaded again only once the worker under a wrapper is gone.
sed -i 's|^psst_model_load() {|&  pgrep -f "^sleep 36$" >/dev/null \&\& echo alive >> "$T/alive-at-load";|' "$P"
$P bash -c 'sleep 36; true' 2>/dev/null & A=$!
sleep 0.5; kill -INT $A; wait $A
check "worker stopped before the psst model is loaded" '[[ ! -e $T/alive-at-load ]] && ! pgrep -f "^sleep 36$" >/dev/null'
# SIGKILL the script: the command keeps the lock until it ends; the next run repairs from the marker
$P sleep 3 2>/dev/null & A=$!
sleep 0.5; kill -KILL $A; wait $A 2>/dev/null
check "psst still unloaded after SIGKILL" '[[ $(cat "$T/psst") == unloaded ]]'
check "marker left behind" 'grep -q "^psst=unloaded" "$PSST_CACHE_DIR/gpu-session.marker"'
PSST_GPU_LOCK_TIMEOUT=0.3 $P true 2>/dev/null; rc=$?
check "orphaned command still holds the lock" '[[ $rc == 75 ]]'
$P bash -c '[[ $(cat "$T/psst") == unloaded ]]' 2>"$T/err7"; rc=$?
check "repair message" 'grep -q "repairing" "$T/err7"'
check "carried-over unload kept during next command" '[[ $rc == 0 ]]'
check "psst restored by the next run" '[[ $(cat "$T/psst") == loaded ]]'
check "marker gone after repair" '[[ ! -e $PSST_CACHE_DIR/gpu-session.marker ]]'
check "load/unload sequence" '[[ $(tr "\n" " " < "$T/psst.log") == "unload load unload load unload load unload load " ]]'
echo "passed $pass, failed $fail"
((fail == 0))
