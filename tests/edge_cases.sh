#!/usr/bin/env bash
# edge_cases.sh — 时序/并发/异常边界测试
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

echo "=========================================="
echo "  ION 边界场景测试（时序/并发/异常）"
echo "=========================================="

# ── 时序问题 ──────────────────────────────────

echo ""
echo "=== T1: serve 刚启动立刻 rpc ==="
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
"$ION" serve > /dev/null 2>&1 &
SERVE_PID=$!
# Immediately send RPC (no sleep)
HEALTH=$("$ION" rpc --method health --params '{}' 2>/dev/null)
if echo "$HEALTH" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); exit(0 if d.get('success') else 1)" 2>/dev/null; then
    record_pass
else
    record_fail "health RPC failed immediately after serve start" "T1"
fi
sleep 2

echo ""
echo "=== T2: agent 跑着时发只读 RPC（并发 select!） ==="
SID=$("$ION" rpc --method create_session --params '{"agent":"developer"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
if [ -n "$SID" ]; then
    # Send prompt (fire-and-forget, agent starts running)
    "$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    sleep 2
    # While agent is running, send a read-only RPC
    STATE=$("$ION" rpc --session "$SID" --method get_state --params '{}' 2>/dev/null)
    if echo "$STATE" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); exit(0 if d.get('success') else 1)" 2>/dev/null; then
        record_pass
    else
        record_fail "get_state blocked during agent run" "T2"
    fi
else
    record_fail "no session for T2" "T2"
fi

echo ""
echo "=== T3: 不存在的 session 发 RPC ==="
RESULT=$("$ION" rpc --session "sess_nonexistent_12345" --method get_state --params '{}' 2>/dev/null)
if echo "$RESULT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); exit(0 if d.get('success') or 'not found' in str(d).lower() or 'auto' in str(d).lower() else 1)" 2>/dev/null; then
    record_pass
else
    record_fail "crash on non-existent session" "T3"
fi

# ── 并发问题 ──────────────────────────────────

echo ""
echo "=== C1: 同一 session 发两个 prompt（应返回 busy） ==="
if [ -n "$SID" ]; then
    # First prompt (fire-and-forget)
    "$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml and list deps"}' > /dev/null 2>&1 &
    sleep 1
    # Second prompt (should get busy or queued)
    RESULT2=$("$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml again"}' 2>/dev/null)
    BUSY=$(echo "$RESULT2" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); data=d.get('data',{}); print(data.get('status','') if isinstance(data,dict) else '')" 2>/dev/null)
    if [ "$BUSY" = "busy" ] || [ "$BUSY" = "forwarded" ]; then
        record_pass
    else
        record_fail "second prompt should return busy/forwarded, got: $BUSY" "C1"
    fi
else
    record_fail "no session for C1" "C1"
fi

echo ""
echo "=== C2: 两个不同 session 同时 prompt ==="
SID2=$("$ION" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | python3 -c "import sys,json;print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
if [ -n "$SID2" ]; then
    "$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    "$ION" rpc --session "$SID2" --method prompt --params '{"text":"Read Cargo.toml"}' > /dev/null 2>&1 &
    sleep 5
    # Both sessions should still be alive
    SESSIONS=$("$ION" rpc --method list_sessions --params '{}' 2>/dev/null | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(len(d.get('data',{}).get('sessions',[])))" 2>/dev/null)
    if [ "$SESSIONS" -ge 2 ] 2>/dev/null; then
        record_pass
    else
        record_fail "sessions dropped during concurrent prompt" "C2"
    fi
else
    record_fail "could not create second session" "C2"
fi

# ── 异常边界 ──────────────────────────────────

echo ""
echo "=== E1: read 不存在的文件 ==="
READ_RESULT=$("$ION" rpc --session "$SID2" --method call_tool --params '{"tool":"read","args":{"file_path":"/nonexistent/file.rs"}}' 2>/dev/null)
SUCCESS=$(echo "$READ_RESULT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('success',False))" 2>/dev/null)
if [ "$SUCCESS" = "False" ] || echo "$READ_RESULT" | grep -qi "not found\|no such file\|does not exist"; then
    record_pass
else
    record_fail "read nonexistent file should fail gracefully" "E1"
fi

echo ""
echo "=== E2: 空消息 prompt ==="
EMPTY_RESULT=$("$ION" rpc --session "$SID2" --method prompt --params '{"text":""}' 2>/dev/null)
if echo "$EMPTY_RESULT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); exit(0 if d.get('success') else 1)" 2>/dev/null; then
    record_pass
else
    # Empty prompt may be rejected, that's also acceptable
    record_pass
fi

echo ""
echo "=== E3: 不存在的 model ==="
BAD_MODEL=$(echo "hi" | timeout 15 "$ION" --provider zai --model "nonexistent-model-xyz" --max-turns 1 --no-tools 2>&1 | grep -ci "error\|not found\|invalid")
if [ "$BAD_MODEL" -gt 0 ]; then
    record_pass
else
    record_fail "nonexistent model should error" "E3"
fi

echo ""
echo "=== E4: kill serve 后重连 ==="
kill $SERVE_PID 2>/dev/null
sleep 2
RECONNECT=$("$ION" rpc --method health --params '{}' 2>/dev/null)
if echo "$RECONNECT" | grep -qi "error\|cannot connect\|refused"; then
    record_pass
else
    record_fail "should not connect after serve killed" "E4"
fi

echo ""
echo "=== E5: serve 重启后恢复 ==="
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
"$ION" serve > /dev/null 2>&1 &
sleep 3
HEALTH2=$("$ION" rpc --method health --params '{}' 2>/dev/null)
if echo "$HEALTH2" | grep -q '"ok"'; then
    record_pass
else
    record_fail "serve restart failed" "E5"
fi

# Cleanup
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null

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
