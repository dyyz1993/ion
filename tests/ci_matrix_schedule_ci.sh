#!/usr/bin/env bash
#
# ci_matrix_schedule_ci.sh — scheduling & isolation checks for
# scripts/run_ci_matrix_parallel.sh (T02)
#
# Runs the matrix runner against a sandbox fake project (fake ion binary +
# timestamping fake test scripts). No real CI, no LLM, no host, no cargo
# compilation. NOTE: keep this script's messages ASCII — bash 3.2 glues
# bytes >=0x80 onto $var names inside double quotes.
#
#   A1  runner exits 0 when all fake scripts pass
#   A2  every runnable script executes EXACTLY once (no double dispatch)
#   A3  SERIAL_LIST scripts never overlap each other (max concurrency 1)
#   A4  parallel-group scripts do overlap (Phase 1 stays parallel)
#   A5  serial scripts all start after the parallel batch finished
#   A6  source .ion/monitors/ is left untouched (no rm -rf of user configs)
#   A7  manifest.txt covers every discovered script; SKIP-listed member
#       gets a SKIP result
#   A8  two runs use different run roots; earlier run's artifacts survive
#   A9  any script failing (FAKE_RC=7) -> whole runner exits non-zero
#
# Usage: bash tests/ci_matrix_schedule_ci.sh
#
set -u

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SB="$(mktemp -d /tmp/ion-matrix-sched.XXXXXX)"
FP="$SB/fakeproj"
RUN_ROOTS=
trap 'rm -rf "$SB" $RUN_ROOTS' EXIT

P=0
F=0
ok()  { echo "  [PASS] $1"; P=$((P+1)); }
bad() { echo "  [FAIL] $1"; F=$((F+1)); }

# --- Fake project: runner + aggregator copied from the CURRENT repo state ---
# Since T03 the runner runs a REAL cargo preflight, so the fake project must
# be a real (tiny) cargo package with a buildable `ion` bin target.
mkdir -p "$FP/scripts" "$FP/tests" "$FP/target/debug" "$FP/.ion/monitors" "$FP/docs" "$FP/src/bin"
cp "$PROJECT_DIR/scripts/run_ci_matrix_parallel.sh" "$FP/scripts/"
cp "$PROJECT_DIR/scripts/aggregate_ci_results.sh" "$FP/scripts/"
cat > "$FP/Cargo.toml" <<'EOF'
[package]
name = "fakeproj"
version = "0.1.0"
edition = "2018"

[[bin]]
name = "ion"
path = "src/bin/ion.rs"
EOF
printf 'fn main() {}\n' > "$FP/src/bin/ion.rs"
printf 'pub fn hello() {}\n' > "$FP/src/lib.rs"
# No Cargo.lock in the fake project -> preflight must not use --locked here;
# PREFLIGHT_SET keeps the baseline minimal (this suite tests SCHEDULING;
# cargo honesty has its own suite: tests/ci_trust_gates_ci.sh).
PREFLIGHT_SET="cargo build --bin ion"
printf '# user monitor config -- must survive matrix runs\nschedule=* * * * *\n' \
    > "$FP/.ion/monitors/SENTINEL.cfg"
MON_BEFORE=$(cksum "$FP/.ion/monitors/SENTINEL.cfg" | awk '{print $1}')
(cd "$FP" && cargo build --bin ion) > "$SB/setup-build.log" 2>&1 \
    || { echo "  [FAIL] setup: fake project build error"; cat "$SB/setup-build.log"; exit 1; }

# Fake test scripts: log sub-second timestamps via python3 time.time()
# (portable; avoids GNU-only `date +%N`).
make_fake() {
    cat > "$FP/tests/$1.sh" <<EOF
#!/usr/bin/env bash
bn=\$(basename "\$0" .sh)
echo "\$(python3 -c 'import time; print(time.time())') START \$bn" >> "\$INVOKE_LOG"
sleep 0.6
echo "\$(python3 -c 'import time; print(time.time())') END \$bn" >> "\$INVOKE_LOG"
exit \${FAKE_RC:-0}
EOF
    chmod +x "$FP/tests/$1.sh"
}
make_fake a_para_ci          # parallel group
make_fake b_para_ci          # parallel group
make_fake monitor_ci         # SERIAL_LIST member
make_fake message_source_ci  # SERIAL_LIST member
make_fake goal_supervisor_ci # SKIP_LIST member

capture_root() {
    grep -o '/tmp/ci-matrix-[0-9-]*[0-9]' "$1" | head -1
}

# --- Run 1: all PASS --------------------------------------------------------
INVOKE_LOG="$SB/invoke1.log" \
PREFLIGHT_CMDS="$PREFLIGHT_SET" \
        bash "$FP/scripts/run_ci_matrix_parallel.sh" > "$SB/run1.out" 2>&1
RC1=$?
RUN1=$(capture_root "$SB/run1.out")
[ -n "$RUN1" ] && RUN_ROOTS="$RUN_ROOTS $RUN1"

[ "$RC1" -eq 0 ] && ok "A1 runner exit 0 when all pass" \
                 || bad "A1 runner exit ${RC1} (want 0)"

PY_FAILS=$(python3 - "$SB/invoke1.log" <<'PY'
import sys
lines = [l.split() for l in open(sys.argv[1]) if l.strip()]
starts = {}
for ts, kind, name in lines:
    if kind == "START":
        starts[name] = starts.get(name, 0) + 1
runnable = ["a_para_ci", "b_para_ci", "monitor_ci", "message_source_ci"]
fails = 0
for n in runnable:
    c = starts.get(n, 0)
    print("  [%s] A2 %s executed %d time(s) (want 1)" % ("PASS" if c == 1 else "FAIL", n, c))
    if c != 1:
        fails += 1
if "goal_supervisor_ci" in starts:
    print("  [FAIL] A2 SKIP-list member was executed")
    fails += 1
else:
    print("  [PASS] A2 SKIP-list member not executed")

iv = {}
for ts, kind, name in lines:
    iv.setdefault(name, {})[kind] = float(ts)

def overlap(a, b):
    return iv[a]["START"] < iv[b]["END"] and iv[b]["START"] < iv[a]["END"]

try:
    if overlap("monitor_ci", "message_source_ci"):
        print("  [FAIL] A3 serial group overlapped"); fails += 1
    else:
        print("  [PASS] A3 serial group max concurrency 1")
    if overlap("a_para_ci", "b_para_ci"):
        print("  [PASS] A4 parallel group overlapped")
    else:
        print("  [FAIL] A4 parallel group did not overlap"); fails += 1
    para_end = max(iv["a_para_ci"]["END"], iv["b_para_ci"]["END"])
    serial_start = min(iv["monitor_ci"]["START"], iv["message_source_ci"]["START"])
    if para_end < serial_start:
        print("  [PASS] A5 serial scripts started after parallel batch ended")
    else:
        print("  [FAIL] A5 serial script leaked into parallel batch"); fails += 1
except KeyError as e:
    print("  [FAIL] scheduling assertion missing event %s" % e); fails += 1
print(fails)
sys.exit(0)
PY
)
echo "$PY_FAILS" | sed -n '1,10p'
PY_F=$(echo "$PY_FAILS" | tail -1)
if [ "${PY_F:-0}" -eq 0 ]; then P=$((P+6)); else F=$((F+1)); fi

MON_AFTER=$(cksum "$FP/.ion/monitors/SENTINEL.cfg" 2>/dev/null | awk '{print $1}')
if [ "$MON_BEFORE" = "$MON_AFTER" ] && [ -f "$FP/.ion/monitors/SENTINEL.cfg" ]; then
    ok "A6 source .ion/monitors/SENTINEL.cfg untouched"
else
    bad "A6 source .ion/monitors modified or deleted"
fi

MANIFEST="${RUN1}/results/manifest.txt"
if [ -f "$MANIFEST" ] && [ "$(wc -l < "$MANIFEST" | tr -d ' ')" -eq 5 ]; then
    ok "A7 manifest.txt covers all 5 discovered scripts"
else
    bad "A7 manifest missing or wrong entry count at ${MANIFEST}"
fi
if grep -q '"status":"SKIP"' "${RUN1}/results/skipped.jsonl" 2>/dev/null; then
    ok "A7 SKIP-list member recorded as SKIP"
else
    bad "A7 SKIP result record missing"
fi

# --- Run 2: per-run isolation + earlier artifacts survive -------------------
INVOKE_LOG="$SB/invoke2.log" \
PREFLIGHT_CMDS="$PREFLIGHT_SET" \
        bash "$FP/scripts/run_ci_matrix_parallel.sh" > "$SB/run2.out" 2>&1
RC2=$?
RUN2=$(capture_root "$SB/run2.out")
[ -n "$RUN2" ] && RUN_ROOTS="$RUN_ROOTS $RUN2"

if [ -n "$RUN1" ] && [ -n "$RUN2" ] && [ "$RUN1" != "$RUN2" ]; then
    ok "A8 run roots differ (${RUN1} vs ${RUN2})"
else
    bad "A8 run roots identical or missing"
fi
if [ -f "${RUN1}/results/a_para_ci.jsonl" ] && [ -f "${RUN2}/results/a_para_ci.jsonl" ]; then
    ok "A8 earlier run artifacts not clobbered"
else
    bad "A8 some run artifacts lost"
fi
[ "$RC2" -eq 0 ] && ok "A8 second all-PASS run exits 0" || bad "A8 second run exit ${RC2}"

# --- Run 3: any failure propagates ------------------------------------------
INVOKE_LOG="$SB/invoke3.log" \
FAKE_RC=7 \
PREFLIGHT_CMDS="$PREFLIGHT_SET" \
    bash "$FP/scripts/run_ci_matrix_parallel.sh" > "$SB/run3.out" 2>&1
RC3=$?
RUN3=$(capture_root "$SB/run3.out")
[ -n "$RUN3" ] && RUN_ROOTS="$RUN_ROOTS $RUN3"

if [ "$RC3" -ne 0 ]; then
    ok "A9 failing script -> runner exit ${RC3} (non-zero)"
else
    bad "A9 scripts failed but runner exited 0 (false success)"
fi
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if d["counts"]["fail"]>=1 and d["verdict"]=="FAIL" else 1)' \
       "${RUN3}/results/aggregate-summary.json" 2>/dev/null; then
    ok "A9 summary verdict FAIL with failure counts"
else
    bad "A9 summary did not record the failure correctly"
fi

# --- Summary -----------------------------------------------------------------
echo ""
echo "========================================="
echo "  ci_matrix_schedule_ci: ${P} passed / ${F} failed"
echo "========================================="
[ "$F" -eq 0 ]
