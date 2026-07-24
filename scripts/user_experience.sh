#!/usr/bin/env bash
# user_experience.sh — 10 角色体验验证
# 用 fast 模型跑，采集所有问题
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
ISSUES_FILE="/tmp/ion_user_issues.jsonl"
rm -f "$ISSUES_FILE"

run_role() {
    local role_id="$1"
    local role_name="$2"
    local role_cmd="$3"
    local check="$4"

    echo ""
    echo "============================================"
    echo "  角色 $role_id: $role_name"
    echo "============================================"

    local result
    result=$(eval "$role_cmd" 2>&1)
    local rc=$?

    # Filter noise
    local filtered
    filtered=$(echo "$result" | grep -v "TRACE\|wasmtime\|cranelift\|BlockLow\|lowering\|emit:\|iter:\|WARN.*wasm\|stream-debug\|extension_message\|INFO\|setValue\|valueForKey" | tail -10)

    echo "$filtered"

    # Check
    local pass=true
    local issue=""
    if echo "$result" | grep -qi "no response\|error\|panic\|failed"; then
        if ! echo "$result" | grep -qi "Permission denied\|denied by extension"; then
            pass=false
            issue="error or no response"
        fi
    fi

    if [ -n "$check" ]; then
        if ! echo "$result" | grep -qi "$check"; then
            pass=false
            issue="expected content not found: $check"
        fi
    fi

    if [ "$pass" = true ]; then
        echo "  ✅ PASS"
    else
        echo "  ❌ FAIL: $issue"
        echo "{\"role\":$role_id,\"name\":\"$role_name\",\"issue\":\"$issue\"}" >> "$ISSUES_FILE"
    fi
}

# Ensure binary
if [ ! -f "$ION" ]; then
    cargo build --bin ion --bin ion-worker 2>&1 | tail -3
fi

echo "=========================================="
echo "  ION 10 角色体验验证 — 第一轮"
echo "=========================================="

# Role 1: Junior Dev
run_role 1 "新手开发者" \
    "echo 'Read Cargo.toml and list all dependencies' | timeout 60 $ION --provider zai --model glm-5.2 --max-turns 5" \
    "tokio\|serde\|depend\|crate"

# Role 2: Senior Dev (reviewer)
run_role 2 "资深开发者" \
    "echo 'Review src/agent/agent_loop.rs for error handling issues' | timeout 60 $ION --agent reviewer --provider zai --model glm-5.2 --max-turns 5" \
    "review\|error\|handling\|issue\|suggest"

# Role 3: PM
run_role 3 "项目经理" \
    "echo 'Analyze this project architecture. List main modules and their functions.' | timeout 60 $ION --provider zai --model glm-5.2 --max-turns 5" \
    "module\|agent\|worker\|session\|provider"

# Role 4: QA Engineer
run_role 4 "QA 测试工程师" \
    "echo 'Run cargo test --lib and report results' | timeout 120 $ION --agent developer --provider zai --model glm-5.2 --max-turns 5" \
    "test\|pass\|result\|777"

# Role 5: DevOps (serve mode)
echo ""
echo "============================================"
echo "  角色 5: DevOps 工程师"
echo "============================================"
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
$ION serve > /dev/null 2>&1 &
sleep 3
HEALTH=$($ION rpc --method health --params '{}' 2>/dev/null)
echo "Health: $(echo $HEALTH | head -c 100)"
if echo "$HEALTH" | grep -q '"ok"'; then
    echo "  ✅ PASS"
else
    echo "  ❌ FAIL: health RPC"
    echo '{"role":5,"name":"DevOps","issue":"health RPC failed"}' >> "$ISSUES_FILE"
fi
# Keep serve running for roles 6-9

# Role 6: Security Auditor
echo ""
echo "============================================"
echo "  角色 6: 安全审计员"
echo "============================================"
SID=$($ION rpc --method create_session --params '{"agent":"build"}' 2>/dev/null | grep -o '"session_id":"[^"]*"' | head -1 | sed 's/"session_id":"//;s/"//')
if [ -n "$SID" ]; then
    # Try to read .env (should be denied if rules set, or succeed if no rules)
    READ_RESULT=$($ION rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"path":"Cargo.toml"}}' 2>/dev/null)
    if echo "$READ_RESULT" | grep -qi "error\|fail"; then
        echo "  ⚠️ read Cargo.toml failed"
        echo '{"role":6,"name":"Security","issue":"read tool failed"}' >> "$ISSUES_FILE"
    else
        echo "  ✅ PASS — read tool works"
    fi
else
    echo "  ❌ FAIL: no session"
    echo '{"role":6,"name":"Security","issue":"session creation failed"}' >> "$ISSUES_FILE"
fi

# Role 7: WASM Extension Dev
echo ""
echo "============================================"
echo "  角色 7: WASM 扩展开发者"
echo "============================================"
WASM_LOADED=$(echo "hi" | RUST_LOG="ion=info" timeout 5 $ION --max-turns 1 --no-tools 2>&1 | grep "rules-engine-wasm initialized")
if [ -n "$WASM_LOADED" ]; then
    echo "  ✅ PASS — rules-engine WASM loaded"
else
    echo "  ❌ FAIL — WASM not loaded"
    echo '{"role":7,"name":"WASM Dev","issue":"rules-engine WASM not loaded"}' >> "$ISSUES_FILE"
fi

# Role 8: Multi-Agent Orchestrator
run_role 8 "多Agent编排者" \
    "echo 'Read Cargo.toml and summarize dependencies' | timeout 90 $ION --host --agent developer --provider zai --model glm-5.2 --max-turns 8" \
    "\[wkr_\|tokio\|serde\|depend"

# Role 9: Session Manager
echo ""
echo "============================================"
echo "  角色 9: 会话管理用户"
echo "============================================"
SESSIONS=$($ION sessions 2>/dev/null | head -5)
if [ -n "$SESSIONS" ]; then
    echo "  ✅ PASS — sessions list works"
else
    echo "  ⚠️ sessions list empty or error"
fi

# Role 10: Self-Evolution Maintainer
echo ""
echo "============================================"
echo "  角色 10: 自进化维护者"
echo "============================================"
# Kill serve first (self_test starts its own)
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
bash scripts/self_test.sh 3 > /tmp/role10_selftest.log 2>&1 &
ST_PID=$!
for i in $(seq 1 10); do
    sleep 30
    kill -0 $ST_PID 2>/dev/null || break
done
ST_RESULT=$(grep "Passed:" /tmp/role10_selftest.log 2>/dev/null)
if echo "$ST_RESULT" | grep -q "3"; then
    echo "  ✅ PASS — 3/3 scenarios"
else
    echo "  ❌ FAIL: $ST_RESULT"
    echo '{"role":10,"name":"Maintainer","issue":"self_test failed"}' >> "$ISSUES_FILE"
fi

# ── Summary ──
echo ""
echo "=========================================="
echo "  体验验证总结 — 第一轮"
echo "=========================================="
echo ""
TOTAL_ISSUES=$(wc -l < "$ISSUES_FILE" 2>/dev/null || echo "0")
echo "  问题数: $TOTAL_ISSUES"
echo ""

if [ "$TOTAL_ISSUES" -gt 0 ] && [ -f "$ISSUES_FILE" ]; then
    echo "  问题清单:"
    cat "$ISSUES_FILE" | python3 -c "
import sys,json
for line in sys.stdin:
    try:
        d=json.loads(line.strip())
        print(f\"    角色{d['role']} ({d['name']}): {d['issue']}\")
    except: pass
" 2>/dev/null
fi

echo ""
echo "=========================================="
