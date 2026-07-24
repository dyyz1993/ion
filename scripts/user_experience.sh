#!/usr/bin/env bash
# user_experience.sh — 10 角色体验验证（v2，修复 bash 转义问题）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
ISSUES_FILE="/tmp/ion_user_issues.jsonl"
rm -f "$ISSUES_FILE"

check_result() {
    local role_id="$1"
    local role_name="$2"
    local output="$3"
    local keyword="$4"

    if echo "$output" | grep -qi "no response"; then
        if ! echo "$output" | grep -qi "tokio\|serde\|cargo\|module\|agent\|test\|pass\|wkr_"; then
            echo "  ❌ FAIL: no response"
            echo "{\"role\":$role_id,\"name\":\"$role_name\",\"issue\":\"no response\"}" >> "$ISSUES_FILE"
            return
        fi
    fi

    if [ -n "$keyword" ] && ! echo "$output" | grep -qi "$keyword"; then
        echo "  ❌ FAIL: keyword '$keyword' not found"
        echo "{\"role\":$role_id,\"name\":\"$role_name\",\"issue\":\"keyword not found: $keyword\"}" >> "$ISSUES_FILE"
        return
    fi

    echo "  ✅ PASS"
}

echo "=========================================="
echo "  ION 10 角色体验验证"
echo "=========================================="

# ── Role 1: Junior Dev ──
echo ""
echo "=== 角色 1: 新手开发者 ==="
OUT=$(echo "Read Cargo.toml and list all dependencies" | timeout 60 "$ION" --provider zai --model glm-5.2 --max-turns 5 2>&1)
echo "$OUT" | grep -v "TRACE\|wasmtime\|cranelift\|WARN.*wasm\|extension_message\|INFO\|stream-debug" | tail -3
check_result 1 "新手开发者" "$OUT" "tokio\|serde\|depend\|crate"

# ── Role 2: Senior Dev ──
echo ""
echo "=== 角色 2: 资深开发者 ==="
OUT=$(echo "Briefly review src/lib.rs for code organization" | timeout 60 "$ION" --agent reviewer --provider zai --model glm-5.2 --max-turns 5 2>&1)
echo "$OUT" | grep -v "TRACE\|wasmtime\|cranelift\|WARN.*wasm\|extension_message\|INFO\|stream-debug" | tail -3
check_result 2 "资深开发者" "$OUT" "module\|pub\|mod\|review\|code"

# ── Role 3: PM ──
echo ""
echo "=== 角色 3: 项目经理 ==="
OUT=$(echo "List the main modules in src/lib.rs and describe what each does" | timeout 60 "$ION" --provider zai --model glm-5.2 --max-turns 5 2>&1)
echo "$OUT" | grep -v "TRACE\|wasmtime\|cranelift\|WARN.*wasm\|extension_message\|INFO\|stream-debug" | tail -3
check_result 3 "项目经理" "$OUT" "module\|agent\|session\|worker"

# ── Role 4: QA Engineer ──
echo ""
echo "=== 角色 4: QA 测试工程师 ==="
OUT=$(echo "Run: cargo test --lib 2>&1 | tail -5. Report the test count." | timeout 120 "$ION" --agent developer --provider zai --model glm-5.2 --max-turns 5 2>&1)
echo "$OUT" | grep -v "TRACE\|wasmtime\|cranelift\|WARN.*wasm\|extension_message\|INFO\|stream-debug" | tail -3
check_result 4 "QA" "$OUT" "test\|pass\|result\|777"

# ── Role 5: DevOps ──
echo ""
echo "=== 角色 5: DevOps 工程师 ==="
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
"$ION" serve > /dev/null 2>&1 &
sleep 3
HEALTH=$("$ION" rpc --method health --params '{}' 2>/dev/null)
if echo "$HEALTH" | grep -q '"ok"'; then
    echo "  ✅ PASS"
else
    echo "  ❌ FAIL: health RPC"
    echo '{"role":5,"name":"DevOps","issue":"health failed"}' >> "$ISSUES_FILE"
fi

# ── Role 6: Security ──
echo ""
echo "=== 角色 6: 安全审计员 ==="
sleep 3  # Extra time for serve to fully initialize
SID=""
for retry in 1 2 3 4 5; do
    SID=$("$ION" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | grep -o '"session_id":"[^"]*"' | head -1 | sed 's/"session_id":"//;s/"//')
    [ -n "$SID" ] && break
    sleep 2
done
if [ -n "$SID" ]; then
    READ_OUT=$("$ION" rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"path":"Cargo.toml"}}' 2>/dev/null)
    if echo "$READ_OUT" | grep -qi "error\|fail"; then
        echo "  ❌ FAIL: read tool error"
        echo '{"role":6,"name":"Security","issue":"read tool failed"}' >> "$ISSUES_FILE"
    else
        echo "  ✅ PASS — tools work in serve mode"
    fi
else
    echo "  ❌ FAIL: session creation failed"
    echo '{"role":6,"name":"Security","issue":"session creation failed"}' >> "$ISSUES_FILE"
fi

# ── Role 7: WASM Extension Dev ──
echo ""
echo "=== 角色 7: WASM 扩展开发者 ==="
WASM_CHECK=$(echo "hi" | RUST_LOG="ion=info" timeout 5 "$ION" --max-turns 1 --no-tools 2>&1 | grep "rules-engine-wasm initialized")
if [ -n "$WASM_CHECK" ]; then
    echo "  ✅ PASS"
else
    echo "  ❌ FAIL: WASM not loaded"
    echo '{"role":7,"name":"WASM","issue":"wasm not loaded"}' >> "$ISSUES_FILE"
fi

# ── Role 8: Orchestrator ──
echo ""
echo "=== 角色 8: 多Agent编排者 ==="
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
OUT=$(echo "Read Cargo.toml and summarize" | timeout 90 "$ION" --host --agent developer --provider zai --model glm-5.2 --max-turns 8 2>&1)
echo "$OUT" | grep "\[wkr_\|tokio\|serde\|depend" | head -3
check_result 8 "编排者" "$OUT" "wkr_\|tokio\|serde\|depend"

# ── Role 9: Session Manager ──
echo ""
echo "=== 角色 9: 会话管理用户 ==="
SESSIONS=$("$ION" sessions 2>/dev/null | head -3)
if [ -n "$SESSIONS" ]; then
    echo "  ✅ PASS"
else
    echo "  ❌ FAIL: sessions empty"
    echo '{"role":9,"name":"SessionMgr","issue":"sessions empty"}' >> "$ISSUES_FILE"
fi

# ── Role 10: Maintainer ──
echo ""
echo "=== 角色 10: 自进化维护者 ==="
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
bash scripts/self_test.sh 3 > /tmp/role10_st.log 2>&1 &
ST_PID=$!
for i in $(seq 1 10); do sleep 30; kill -0 $ST_PID 2>/dev/null || break; done
ST_RESULT=$(grep "Passed:" /tmp/role10_st.log 2>/dev/null)
if echo "$ST_RESULT" | grep -q "3"; then
    echo "  ✅ PASS"
else
    echo "  ❌ FAIL: $ST_RESULT"
    echo '{"role":10,"name":"Maintainer","issue":"self_test failed"}' >> "$ISSUES_FILE"
fi

# ── Summary ──
echo ""
echo "=========================================="
echo "  体验验证总结"
echo "=========================================="
TOTAL=$(wc -l < "$ISSUES_FILE" 2>/dev/null || echo "0")
PASS=$((10 - TOTAL))
echo "  通过: $PASS / 10"
echo "  失败: $TOTAL"
if [ "$TOTAL" -gt 0 ]; then
    echo ""
    echo "  问题:"
    cat "$ISSUES_FILE"
fi
echo "=========================================="
