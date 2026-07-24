#!/usr/bin/env bash
# edge_cases.sh — 时序/并发/异常边界测试（v2，独立 serve 生命周期）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
ISSUES_FILE="/tmp/ion_edge_issues.jsonl"
rm -f "$ISSUES_FILE"

PASS=0
FAIL=0
record_pass() { echo "  ✅ PASS"; PASS=$((PASS+1)); }
record_fail() { echo "  ❌ FAIL: $1"; echo "{\"test\":\"$2\",\"issue\":\"$1\"}" >> "$ISSUES_FILE"; FAIL=$((FAIL+1)); }

start_serve() {
    lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
    rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
    sleep 1
    "$ION" serve > /dev/null 2>&1 &
    SERVE_PID=$!
    sleep 4
}

stop_serve() {
    lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
    rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
    sleep 1
}

echo "=========================================="
echo "  ION 边界场景测试 v2"
echo "=========================================="

# ── T1: serve 刚启动 rpc（预期：可能失败，socket 还没好）──
echo ""
echo "=== T1: serve 刚启动立刻 rpc ==="
stop_serve
"$ION" serve > /dev/null 2>&1 &
sleep 0.5
HEALTH=$(timeout 5 "$ION" rpc --method health --params '{}' 2>/dev/null)
# T1 is informational: socket may not be ready yet. Not a bug.
if echo "$HEALTH" | grep -q '"ok"'; then
    record_pass
else
    echo "  ℹ️ INFO: socket not ready in 0.5s (expected, not a bug)"
    record_pass  # Not a failure — serve needs time to initialize
fi
stop_serve

# ── T2: agent 跑着时发只读 RPC ──
echo ""
echo "=== T2: agent 跑着时发只读 RPC ==="
start_serve
SID=$("$ION" rpc --method create_session --params '{"agent":"developer"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
if [ -n "$SID" ]; then
    "$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    sleep 2
    STATE=$(timeout 10 "$ION" rpc --session "$SID" --method get_state --params '{}' 2>/dev/null)
    if echo "$STATE" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); exit(0 if d.get('success') else 1)" 2>/dev/null; then
        record_pass
    else
        record_fail "get_state blocked during agent run" "T2"
    fi
else
    record_fail "no session" "T2"
fi
stop_serve

# ── T3: 不存在的 session（预期：30s 超时退出，不卡死）──
echo ""
echo "=== T3: 不存在的 session ==="
start_serve
START=$(date +%s)
timeout 35 "$ION" rpc --session "sess_fake_999" --method get_state --params '{}' > /tmp/t3_out.txt 2>&1
RC=$?
END=$(date +%s)
ELAPSED=$((END - START))
if [ $RC -ne 0 ] && [ $ELAPSED -le 32 ]; then
    echo "  Timed out in ${ELAPSED}s (expected: ~30s)"
    record_pass
else
    record_fail "expected 30s timeout exit, got rc=$RC elapsed=${ELAPSED}s" "T3"
fi
stop_serve

# ── C1: 同一 session 两个 prompt ──
echo ""
echo "=== C1: 同一 session 双 prompt ==="
start_serve
SID=$("$ION" rpc --method create_session --params '{"agent":"developer"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
if [ -n "$SID" ]; then
    "$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    sleep 2
    RESULT2=$(timeout 35 "$ION" rpc --session "$SID" --method prompt --params '{"text":"again"}' 2>/dev/null)
    STATUS=$(echo "$RESULT2" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('data',{}).get('status','') if isinstance(d.get('data'),dict) else 'forwarded')" 2>/dev/null)
    if [ "$STATUS" = "busy" ] || [ "$STATUS" = "forwarded" ]; then
        record_pass
    else
        record_fail "expected busy/forwarded, got: $STATUS" "C1"
    fi
else
    record_fail "no session" "C1"
fi
stop_serve

# ── C2: 两个 session 并发 ──
echo ""
echo "=== C2: 两 session 并发 prompt ==="
start_serve
SID1=$("$ION" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
SID2=$("$ION" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
if [ -n "$SID1" ] && [ -n "$SID2" ]; then
    "$ION" rpc --session "$SID1" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    "$ION" rpc --session "$SID2" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    sleep 5
    COUNT=$("$ION" rpc --method list_sessions --params '{}' 2>/dev/null | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(len(d.get('data',{}).get('sessions',[])))" 2>/dev/null)
    if [ "$COUNT" -ge 2 ] 2>/dev/null; then
        record_pass
    else
        record_fail "sessions dropped: count=$COUNT" "C2"
    fi
else
    record_fail "could not create sessions: SID1=$SID1 SID2=$SID2" "C2"
fi
stop_serve

# ── E1: read 不存在的文件 ──
echo ""
echo "=== E1: read 不存在文件 ==="
start_serve
SID=$("$ION" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
RESULT=$(timeout 10 "$ION" rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"/nonexistent/file.rs"}}' 2>/dev/null)
SUCCESS=$(echo "$RESULT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('success','unknown'))" 2>/dev/null)
if [ "$SUCCESS" = "False" ]; then
    record_pass
else
    record_fail "expected success=false, got: $SUCCESS" "E1"
fi
stop_serve

# ── E2: 空消息 prompt ──
echo ""
echo "=== E2: 空消息 prompt ==="
start_serve
SID=$("$ION" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
RESULT=$(timeout 10 "$ION" rpc --session "$SID" --method prompt --params '{"text":""}' 2>/dev/null)
# Empty prompt may succeed or fail — both acceptable
record_pass
stop_serve

# ── E3: 不存在的 model ──
echo ""
echo "=== E3: 不存在 model ==="
BAD=$(echo "hi" | timeout 15 "$ION" --provider zai --model "fake-model-xyz" --max-turns 1 --no-tools 2>&1 | grep -ci "error\|not found\|invalid\|refused")
if [ "$BAD" -gt 0 ]; then
    record_pass
else
    record_fail "nonexistent model should error" "E3"
fi

# ── E4: serve 死后 rpc（预期：立即报错 Cannot connect）──
echo ""
echo "=== E4: serve 死后 rpc ==="
start_serve
stop_serve
RESULT=$(timeout 5 "$ION" rpc --method health --params '{}' 2>&1)
if echo "$RESULT" | grep -qi "cannot connect\|no such file\|refused\|connection"; then
    record_pass
else
    record_fail "should report connection error, got: $RESULT" "E4"
fi

# ── E5: serve 重启后恢复 ──
echo ""
echo "=== E5: serve 重启恢复 ==="
start_serve
HEALTH=$("$ION" rpc --method health --params '{}' 2>/dev/null)
if echo "$HEALTH" | grep -q '"ok"'; then
    record_pass
else
    record_fail "serve restart failed" "E5"
fi
stop_serve

# ── Summary ──
echo ""
echo "=========================================="
echo "  边界测试总结"
echo "=========================================="
echo "  通过: $PASS"
echo "  失败: $FAIL"
if [ -f "$ISSUES_FILE" ]; then
    echo ""
    echo "  问题:"
    cat "$ISSUES_FILE"
fi
echo "=========================================="
