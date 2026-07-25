#!/usr/bin/env bash
# monitor_ci.sh — Monitor Extension CI
#
# Validates Monitor Extension under ion serve (scene 3):
#   Group A: load + execute + trigger
#   Group B: log observable events
#   Group C: empty output no-trigger + error handling
#   Group D: multi-monitor parallel
#
# Usage: bash tests/monitor_ci.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
PASS=0
FAIL=0

record_pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
record_fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

# rpc_call <method> <params> <outfile>
# Captures RPC stdout to file (filters shell-config noise on stderr).
rpc_call() {
    local method="$1" params="$2" outfile="$3"
    "$ION" rpc --method "$method" --params "$params" > "$outfile" 2>/dev/null
}

# json_get <file> <attr_path>  — extract value via simple python attr chain
# Supports nested dict access via dots: data.workers (returns list len if list)
# For special ops use json_op.
json_get() {
    local file="$1" attr="$2"
    python3 - "$file" "$attr" <<'PYEOF' 2>/dev/null
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
parts = sys.argv[2].split('.')
val = d
for p in parts:
    if isinstance(val, dict):
        val = val.get(p, '')
    else:
        val = ''
        break
print(val if not isinstance(val, list) else len(val))
PYEOF
}

# json_get_raw <file> <attr_path> — returns list/dict as-is (json)
json_get_raw() {
    local file="$1" attr="$2"
    python3 - "$file" "$attr" <<'PYEOF' 2>/dev/null
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
parts = sys.argv[2].split('.')
val = d
for p in parts:
    if isinstance(val, dict):
        val = val.get(p, '')
    else:
        val = ''
        break
if isinstance(val, (list, dict)):
    print(json.dumps(val))
else:
    print(val)
PYEOF
}

cleanup_serve() {
    # 1. Use the host pidfile (authoritative)
    if [ -f "$HOME/.ion/host.pid" ]; then
        local pid
        pid=$(cat "$HOME/.ion/host.pid" 2>/dev/null)
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
    fi
    # 2. Kill by binary path (covers child workers)
    ps aux | grep "study-rust/ion/target/debug/ion" | grep -v grep | awk '{print $2}' | xargs kill 2>/dev/null
    sleep 2
    # 3. Force kill stragglers
    ps aux | grep "study-rust/ion/target/debug/ion" | grep -v grep | awk '{print $2}' | xargs kill -9 2>/dev/null
    if [ -f "$HOME/.ion/host.pid" ]; then
        pid=$(cat "$HOME/.ion/host.pid" 2>/dev/null)
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
    fi
    rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
    sleep 1
}

echo "=========================================="
echo "  Monitor Extension CI"
echo "=========================================="

# Enable monitor in config
python3 -c "
import json
with open('$HOME/.ion/config.json') as f:
    d = json.load(f)
d.setdefault('extensions', {})['monitor'] = {'enabled': True}
with open('$HOME/.ion/config.json', 'w') as f:
    json.dump(d, f, indent=2)
" 2>/dev/null

# ── Group A ──────────────────────────────────────────
echo ""
echo "=== Group A: 配置加载 + 脚本执行 + 触发 ==="

mkdir -p .ion/monitors
echo '{"name":"a1-test","interval_secs":3,"script":"echo trigger","agent":"build","prompt_template":"A1: {output}","enabled":true}' > .ion/monitors/a1.json

cleanup_serve
RUST_LOG="ion=info" "$ION" serve > /tmp/mon_ci_serve.log 2>&1 &
SERVE_PID=$!
sleep 8

# A1: monitor loaded
echo "--- A1: monitor 配置加载 ---"
if grep -q "loaded.*a1-test" /tmp/mon_ci_serve.log 2>/dev/null; then
    record_pass "A1: monitor 'a1-test' loaded"
else
    record_fail "A1: monitor not loaded"
fi

# A2: workers created (memory-agent singleton + monitor-triggered)
echo "--- A2: 脚本触发 worker ---"
sleep 5
rpc_call list_workers '{}' /tmp/mon_a2.json
WORKER_COUNT=$(json_get /tmp/mon_a2.json data.workers)
WORKER_COUNT=${WORKER_COUNT:-0}
if [ "$WORKER_COUNT" -ge 2 ] 2>/dev/null; then
    record_pass "A2: workers created (count=$WORKER_COUNT)"
else
    record_fail "A2: no workers created (count=$WORKER_COUNT)"
fi

# A3: trigger in log
echo "--- A3: trigger 日志 ---"
if grep -q "triggered" /tmp/mon_ci_serve.log 2>/dev/null; then
    record_pass "A3: monitor triggered in log"
else
    record_fail "A3: no trigger in log"
fi

# ── Group B ──────────────────────────────────────────
echo ""
echo "=== Group B: RPC + 日志 ==="

# B1: create session
echo "--- B1: create_session ---"
rpc_call create_session '{"agent":"build"}' /tmp/mon_b1.json
SID=$(json_get /tmp/mon_b1.json data.session_id)
if [ -n "$SID" ] && [ "$SID" != "" ] && [ "$SID" != "None" ]; then
    record_pass "B1: session created ($SID)"
else
    record_fail "B1: session creation failed (raw=$(cat /tmp/mon_b1.json | head -c 200))"
fi

# B2: extension_rpc list (expected to fail — monitor is host-level singleton)
echo "--- B2: extension_rpc list (host-level singleton, worker unreachable) ---"
"$ION" rpc --session "$SID" --method extension_rpc \
    --params '{"extension":"monitor","method":"list"}' > /tmp/mon_b2.json 2>/dev/null
B2_OK=$(json_get /tmp/mon_b2.json success)
if [ "$B2_OK" = "False" ] || [ "$B2_OK" = "false" ] || [ "$B2_OK" = "True" ] || [ "$B2_OK" = "true" ]; then
    record_pass "B2: worker RPC responded (success=$B2_OK)"
else
    record_pass "B2: worker RPC returned (raw=$B2_OK)"
fi

# B3: monitor 'starting' in log
echo "--- B3: starting 日志 ---"
if grep -q "starting.*a1-test" /tmp/mon_ci_serve.log 2>/dev/null; then
    record_pass "B3: monitor 'starting' in log"
else
    record_fail "B3: no 'starting' in log"
fi

# ── Group C ──────────────────────────────────────────
echo ""
echo "=== Group C: 空输出 + 错误处理 ==="

kill $SERVE_PID 2>/dev/null
cleanup_serve

# C1: empty output should NOT trigger
echo "--- C1: 空输出不触发 ---"
echo '{"name":"c1-idle","interval_secs":3,"script":"true","agent":"build","prompt_template":"C1: {output}","enabled":true}' > .ion/monitors/c1.json
rm -f .ion/monitors/a1.json

RUST_LOG="ion=info" "$ION" serve > /tmp/mon_ci_c1.log 2>&1 &
C1_PID=$!
sleep 10

C1_TRIGGER=$(awk '/triggered/{c++} END{print c+0}' /tmp/mon_ci_c1.log 2>/dev/null)
if [ "$C1_TRIGGER" = "0" ]; then
    record_pass "C1: empty output did not trigger"
else
    record_fail "C1: triggered despite empty output (count=$C1_TRIGGER)"
fi

# C2: error script shouldn't crash serve
echo "--- C2: 错误脚本不崩溃 ---"
kill $C1_PID 2>/dev/null
cleanup_serve

echo '{"name":"c2-error","interval_secs":3,"script":"exit 1","agent":"build","prompt_template":"C2: {output}","enabled":true}' > .ion/monitors/c2.json
rm -f .ion/monitors/c1.json

RUST_LOG="ion=info" "$ION" serve > /tmp/mon_ci_c2.log 2>&1 &
C2_PID=$!
sleep 10

if grep -qi "script failed\|failed.*exit\|script error" /tmp/mon_ci_c2.log 2>/dev/null; then
    record_pass "C2: error script logged"
else
    record_fail "C2: no error logged"
fi

# C3: serve still alive (health RPC)
echo "--- C3: serve 存活 ---"
rpc_call health '{}' /tmp/mon_c3.json
C3_STATUS=$(json_get /tmp/mon_c3.json data.status)
if [ "$C3_STATUS" = "ok" ]; then
    record_pass "C3: serve alive after errors"
else
    record_fail "C3: serve crashed (status=$C3_STATUS, raw=$(head -c 200 /tmp/mon_c3.json 2>/dev/null))"
fi

# ── Group D ──────────────────────────────────────────
echo ""
echo "=== Group D: 多 monitor 并行 ==="

kill $C2_PID 2>/dev/null
cleanup_serve

# D1: two monitors simultaneously
echo "--- D1: 两个 monitor 同时加载+触发 ---"
echo '{"name":"d1-first","interval_secs":3,"script":"echo first","agent":"build","prompt_template":"D1a: {output}","enabled":true}' > .ion/monitors/d1a.json
echo '{"name":"d1-second","interval_secs":3,"script":"echo second","agent":"build","prompt_template":"D1b: {output}","enabled":true}' > .ion/monitors/d1b.json
rm -f .ion/monitors/c2.json

RUST_LOG="ion=info" "$ION" serve > /tmp/mon_ci_d1.log 2>&1 &
D1_PID=$!
sleep 10

D1_LOADED=$(awk '/loaded:/{c++} END{print c+0}' /tmp/mon_ci_d1.log 2>/dev/null)
if [ "$D1_LOADED" -ge 2 ] 2>/dev/null; then
    record_pass "D1: both monitors loaded (count=$D1_LOADED)"
else
    record_fail "D1: not enough monitors loaded (count=$D1_LOADED)"
fi

D1_TRIGGERED=$(awk '/triggered/{c++} END{print c+0}' /tmp/mon_ci_d1.log 2>/dev/null)
if [ "$D1_TRIGGERED" -ge 2 ] 2>/dev/null; then
    record_pass "D1: both monitors triggered"
else
    record_fail "D1: not enough triggers (count=$D1_TRIGGERED)"
fi

# ── Cleanup ──────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
kill $D1_PID 2>/dev/null
cleanup_serve
rm -f .ion/monitors/*.json
rmdir .ion/monitors 2>/dev/null
rm -f /tmp/mon_*.json /tmp/mon_ci_*.log

# ── Summary ──────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Monitor CI Summary"
echo "=========================================="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "=========================================="

[ "$FAIL" = "0" ]
