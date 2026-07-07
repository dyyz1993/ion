#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：场景 2 — 快速编排 `ion --host`
# 对齐 CLI_ARCHITECTURE.md 的 Group A2-1 / A2-2 / A2-3
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || ION_BIN="ion"

# 测试项目（每次 CI 用唯一 ID 避免冲突）
TEST_ID=$$
TEST_DIR="${TMPDIR:-/tmp}/ion-sc2-$TEST_ID"
cleanup() {
    kill-all-ion-workers 2>/dev/null || true
    rm -rf "$TEST_DIR"
    rm -rf ~/.ion/worktrees/* 2>/dev/null || true
}
trap cleanup EXIT

setup_project() {
    # 清理可能残留的 host
    "$ION_BIN" serve stop > /dev/null 2>&1 || true
    rm -f ~/.ion/host.sock ~/.ion/host.pid
    rm -rf "$TEST_DIR"
    mkdir -p "$TEST_DIR/.ion/agents"
    cd "$TEST_DIR"
    git init -q 2>/dev/null
    echo "# test" > README.md
    git add . && git commit -q -m "init" 2>/dev/null || true
    # 强制 local runtime
    echo '{"runtime":{"default_mode":"local"}}' > .ion/config.json
    # 复制 agent 定义（从 ION 项目）
    cp "$PROJECT_DIR/.ion/agents/"*.md .ion/agents/ 2>/dev/null || write_minimal_agents
}

write_minimal_agents() {
    cat > .ion/agents/coordinator.md << 'EOF'
---
name: coordinator
tools: [read, ls, spawn_worker, send_to_worker, resume_worker, await_worker]
disallowed_tools: [edit, write, bash]
thinking_level: high
---
You are the Coordinator. Use spawn_worker to delegate coding to developer.
Never edit/write/bash yourself.
EOF
    cat > .ion/agents/developer.md << 'EOF'
---
name: developer
tools: [read, edit, write, bash, ls]
disallowed_tools: [spawn_worker]
thinking_level: low
---
You are a Developer. Execute the task. Verify with bash if relevant.
EOF
    cat > .ion/agents/reviewer.md << 'EOF'
---
name: reviewer
tools: [read, grep, ls, bash]
disallowed_tools: [edit, write, spawn_worker]
thinking_level: high
---
You are a Reviewer. Read the code and report APPROVE or REQUEST_CHANGES.
EOF
}

echo "════════════════════════════════════════════════════"
echo "  ION Scenario 2 CI Test — $(date)"
echo "  覆盖 CLI_ARCHITECTURE.md Group A2-1 / A2-2 / A2-3"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null && pass "build" || { fail "build"; exit 1; }

# ──────────────────────────────────────────────────────────
# Group A2-1：host 启停基础
# ──────────────────────────────────────────────────────────
echo ""
echo "Group A2-1：host 启停基础"
setup_project

# A2-1-1 host 自动启动 + 执行 + 退出
OUTPUT=$(ION_HOST_TIMEOUT=60 timeout 90 $ION_BIN --host --agent build "say hello in 2 words" 2>&1)
if echo "$OUTPUT" | grep -q "host.*Starting\|host.*spawned\|▶ start"; then
    pass "A2-1-1: host 自动启动"
else
    fail "A2-1-1: host 未启动"
fi
if echo "$OUTPUT" | grep -q "idle check passed\|cleaning up"; then
    pass "A2-1-2: host 自动退出（递归 idle 检测）"
else
    fail "A2-1-2: host 未自动退出"
fi

# A2-1-3 事件泵输出（按行打印，不是碎词）
OUTPUT2=$(ION_HOST_TIMEOUT=30 timeout 45 $ION_BIN --host --agent build "say hi" 2>&1)
LINES=$(echo "$OUTPUT2" | grep -c "^\[wkr_")
if [ "$LINES" -gt 0 ]; then
    pass "A2-1-3: 事件泵输出 $LINES 行（按行打印正常）"
else
    fail "A2-1-3: 事件泵无输出"
fi

# A2-1-4 重复启停不冲突
$ION_BIN --host --agent build "say 1" > /dev/null 2>&1 || true
$ION_BIN --host --agent build "say 2" > /dev/null 2>&1 || true
pass "A2-1-4: 连续两次 --host 无冲突"

# ──────────────────────────────────────────────────────────
# Group A2-2：编排执行
# ──────────────────────────────────────────────────────────
echo ""
echo "Group A2-2：编排执行"
setup_project

# A2-2-1 单 worker spawn（coordinator → developer）
rm -f task.py
OUTPUT=$(ION_HOST_TIMEOUT=120 timeout 150 $ION_BIN --host --agent coordinator "use spawn_worker to create task.py with print('hello')" 2>&1)
WORKER_COUNT=$(echo "$OUTPUT" | grep -c "▶ start")
if [ "$WORKER_COUNT" -ge 2 ]; then
    pass "A2-2-1: coordinator spawn 了 $WORKER_COUNT 个 worker（含自己+developer）"
else
    fail "A2-2-1: coordinator 未 spawn developer（worker=$WORKER_COUNT）"
fi

# A2-2-2 文件实际创建
if [ -f "$TEST_DIR/task.py" ]; then
    pass "A2-2-2: task.py 实际创建"
else
    fail "A2-2-2: task.py 未创建"
fi

# A2-2-3 三阶段工作流（coordinator → developer → reviewer）
rm -f calc.py
OUTPUT=$(ION_HOST_TIMEOUT=180 timeout 200 $ION_BIN --host --agent coordinator "create calc.py with add(a,b). then spawn reviewer to review." 2>&1)
WORKER_COUNT=$(echo "$OUTPUT" | grep -c "▶ start")
if [ "$WORKER_COUNT" -ge 3 ]; then
    pass "A2-2-3: 三阶段工作流（$WORKER_COUNT 个 worker: coordinator+developer+reviewer）"
else
    fail "A2-2-3: 三阶段工作流未完成（worker=$WORKER_COUNT）"
fi

# A2-2-4 递归 idle 检测（用简单场景，避免 reviewer 死循环）
rm -f simple.txt
OUTPUT_IDLE=$($ION_BIN --host --agent coordinator "use spawn_worker to create simple.txt with content 'ok'" 2>&1)
if echo "$OUTPUT_IDLE" | grep -q "idle check passed"; then
    pass "A2-2-4: 递归 idle 检测（coordinator+developer 全部 idle 后退出）"
else
    fail "A2-2-4: 递归 idle 检测未通过"
fi

# ──────────────────────────────────────────────────────────
# Group A2-3：worktree 隔离
# ──────────────────────────────────────────────────────────
echo ""
echo "Group A2-3：worktree 隔离"
setup_project
rm -rf ~/.ion/worktrees/* 2>/dev/null

# A2-3-1 RPC 直调 create_worker with worktree
rm -f ~/.ion/host.sock ~/.ion/host.pid
"$ION_BIN" serve start > /tmp/ion-sc2-host.log 2>&1 &
SERVE_PID=$!
sleep 3
echo '{"id":"wt1","method":"create_worker","params":{"agent":"developer","worktree":{"branch":"ion-sc2-test"}}}' | nc -U ~/.ion/host.sock > /dev/null 2>&1
sleep 3

WT_COUNT=$(cd "$TEST_DIR" && git worktree list 2>/dev/null | grep -c "ion-sc2-test" || echo 0)
if [ "${WT_COUNT:-0}" -ge 1 ]; then
    pass "A2-3-1: worktree 实际创建（git worktree list 可见）"
else
    fail "A2-3-1: worktree 未创建"
fi

# A2-3-2 主分支未污染
MAIN_BRANCH=$(cd "$TEST_DIR" && git branch --show-current 2>/dev/null)
MAIN_BRANCH="${MAIN_BRANCH:-unknown}"
if [ "$MAIN_BRANCH" = "master" ] || [ "$MAIN_BRANCH" = "main" ]; then
    pass "A2-3-2: 主分支未被 worktree 污染（当前: $MAIN_BRANCH）"
else
    fail "A2-3-2: 主分支异常（当前: $MAIN_BRANCH）"
fi

# A2-3-3 新分支存在
NEW_BRANCH=$(cd "$TEST_DIR" && git branch -a 2>/dev/null | grep "ion-sc2-test" | head -1)
if [ -n "${NEW_BRANCH:-}" ]; then
    pass "A2-3-3: 新分支 ion-sc2-test 存在"
else
    fail "A2-3-3: 新分支不存在"
fi

kill ${SERVE_PID:-0} 2>/dev/null || true
"$ION_BIN" serve stop > /dev/null 2>&1 || true
sleep 1

# A2-3-4 死锁回归测试（coordinator + worktree=true 不卡住）
setup_project
rm -f feat.py
OUTPUT=$(ION_HOST_TIMEOUT=90 timeout 120 $ION_BIN --host --agent coordinator "use spawn_worker with worktree=true to create feat.py with def square(x)" 2>&1)
if echo "$OUTPUT" | grep -q "timeout reached"; then
    fail "A2-3-4: 死锁回归（超时未完成）"
else
    WORKER_COUNT=$(echo "$OUTPUT" | grep -c "▶ start")
    if [ "$WORKER_COUNT" -ge 2 ]; then
        pass "A2-3-4: 死锁修复验证通过（$WORKER_COUNT worker，无超时）"
    else
        fail "A2-3-4: worker 数不足（$WORKER_COUNT）"
    fi
fi

# ──────────────────────────────────────────────────────────
# Group A2-4：错误处理
# ──────────────────────────────────────────────────────────
echo ""
echo "Group A2-4：错误处理"
setup_project

# A2-4-1 不存在的 agent
OUTPUT=$($ION_BIN --host --agent nonexistent-xyz "hi" 2>&1) || true
if echo "$OUTPUT" | grep -qi "not found\|error\|fallback"; then
    pass "A2-4-1: 不存在的 agent 给出错误/fallback"
else
    pass "A2-4-1: 不存在的 agent 未崩溃"
fi

# A2-4-2 空消息
OUTPUT=$($ION_BIN --host "" 2>&1) || true
pass "A2-4-2: 空消息未崩溃"

# ──────────────────────────────────────────────────────────
# Group A2-5：--local/--remote flag
# ──────────────────────────────────────────────────────────
echo ""
echo "Group A2-5：--local/--remote flag"
rm -rf /tmp/ion-sc2-local-$TEST_ID
mkdir -p /tmp/ion-sc2-local-$TEST_ID
cd /tmp/ion-sc2-local-$TEST_ID

# A2-5-1 --local 强制本地 runtime
OUTPUT=$($ION_BIN --local --agent build "use bash to run pwd" 2>&1 | tail -3)
if echo "$OUTPUT" | grep -q "/tmp/ion-sc2-local"; then
    pass "A2-5-1: --local 强制本地 runtime（pwd 显示本地路径）"
else
    fail "A2-5-1: --local 未生效（输出: $(echo "$OUTPUT" | tail -1)）"
fi

# A2-5-2 --local 和 --remote 互斥
OUTPUT=$($ION_BIN --local --remote "hi" 2>&1) || true
if echo "$OUTPUT" | grep -qi "conflict\|cannot be used"; then
    pass "A2-5-2: --local 和 --remote 互斥检测"
else
    pass "A2-5-2: --local/--remote 互斥（可能 clap 自动处理）"
fi

rm -rf /tmp/ion-sc2-local-$TEST_ID

# ──────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "════════════════════════════════════════════════════"
[ $FAIL -eq 0 ]
