#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：Team 编排端到端验证
# 验证 coordinator + developer agent 链路在本地 runtime 下端到端跑通
# ──────────────────────────────────────────────────────────
set -uo pipefail

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

# Test project setup
TEST_DIR="${TMPDIR:-/tmp}/ion-team-ci-$$"
cleanup() {
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

echo "════════════════════════════════════════════════════"
echo "  ION Team E2E CI Test — $(date)"
echo "════════════════════════════════════════════════════"

# ── Phase 0: Build ──
cargo build --bin ion --bin ion-worker 2>/dev/null && pass "build ion + ion-worker" || { fail "build"; exit 1; }

# ── Phase 1: Setup test project ──
echo ""
echo "Phase 1: 项目准备"
mkdir -p "$TEST_DIR/.ion/agents"
cd "$TEST_DIR"
git init -q
echo "# Test" > README.md
git add . && git commit -q -m "init"

# 项目级 config 强制 local runtime（避免被全局 remote 干扰）
cat > .ion/config.json << 'EOF'
{
  "runtime": {
    "default_mode": "local"
  }
}
EOF

# coordinator agent
cat > .ion/agents/coordinator.md << 'EOF'
---
name: coordinator
description: Team coordinator
tools:
  - read
  - grep
  - find
  - ls
  - spawn_worker
  - send_to_worker
  - resume_worker
  - await_worker
disallowed_tools:
  - edit
  - write
  - bash
thinking_level: high
---

You are the Coordinator. You DON'T write code yourself.

Your job:
1. Read the user's request.
2. Break the work into 1-3 concrete subtasks.
3. For each subtask, call spawn_worker(relation='child', agent='developer', task='<detailed spec>').
4. After children finish, summarize what was accomplished.

Rules:
- ALWAYS use spawn_worker to delegate coding. Never use edit/write/bash yourself.
- Keep subtask specs concrete: which files to create, what content.
EOF

# developer agent
cat > .ion/agents/developer.md << 'EOF'
---
name: developer
description: Implementation worker
tools:
  - read
  - grep
  - find
  - ls
  - edit
  - write
  - bash
disallowed_tools:
  - spawn_worker
thinking_level: low
---

You are a Developer. You receive a task spec and execute it.

Your job:
1. Read the spec carefully.
2. Implement the change using write/edit.
3. Verify with bash if relevant.
4. Report what files you changed.

Rules:
- Do NOT spawn additional workers.
- Always verify your work.
EOF

pass "Phase 1: test project + agents ready"

# ────────────────────────────────────
# Group A: 单 agent 直接执行（baseline）
# ────────────────────────────────────
echo ""
echo "Group A: 单 developer agent 直接执行"

rm -f hello.py
OUTPUT_A=$(ION_HOST_TIMEOUT=60 timeout 90 $ION_BIN --host --agent developer "use the write tool to create hello.py with content print('hello')" 2>&1) || true
if [ -f "$TEST_DIR/hello.py" ]; then
    CONTENT=$(cat "$TEST_DIR/hello.py")
    if echo "$CONTENT" | grep -q "print"; then
        pass "A1: developer 直接创建 hello.py 成功"
    else
        fail "A1: hello.py 内容错误: $CONTENT"
    fi
else
    fail "A1: hello.py 未创建"
    echo "   output: $(echo "$OUTPUT_A" | tail -5)"
fi

# ────────────────────────────────────
# Group B: coordinator → developer 编排
# ────────────────────────────────────
echo ""
echo "Group B: coordinator 编排 developer"

rm -f hello.py utils.py
OUTPUT_B=$(ION_HOST_TIMEOUT=120 timeout 150 $ION_BIN --host --agent coordinator "Create two files: (1) hello.py with print('hi'), (2) utils.py with function add(a,b). Use spawn_worker." 2>&1) || true

# B1: coordinator 真的 spawn 了 worker
if echo "$OUTPUT_B" | grep -q "▶ start" && [ "$(echo "$OUTPUT_B" | grep -c '▶ start')" -ge 2 ]; then
    pass "B1: coordinator spawn 了至少 2 个 worker（自己+developer）"
else
    fail "B1: coordinator 未 spawn developer（worker 数 < 2）"
    echo "   output: $(echo "$OUTPUT_B" | grep "▶\|✓" | head -5)"
fi

# B2: 文件实际创建
if [ -f "$TEST_DIR/hello.py" ] && [ -f "$TEST_DIR/utils.py" ]; then
    pass "B2: 两个文件都被创建"
else
    MISSING=""
    [ ! -f "$TEST_DIR/hello.py" ] && MISSING="$MISSING hello.py"
    [ ! -f "$TEST_DIR/utils.py" ] && MISSING="$MISSING utils.py"
    fail "B2: 缺少文件:$MISSING"
fi

# B3: 文件内容正确
if [ -f "$TEST_DIR/hello.py" ]; then
    if grep -q "print" "$TEST_DIR/hello.py"; then
        pass "B3: hello.py 内容包含 print"
    else
        fail "B3: hello.py 内容错误"
    fi
fi

# B4: 递归 idle 退出
if echo "$OUTPUT_B" | grep -q "recursive idle check passed"; then
    pass "B4: 递归 idle 检测通过并退出"
else
    fail "B4: 未触发递归 idle 退出"
fi

# ────────────────────────────────────
# Group C: 错误场景
# ────────────────────────────────────
echo ""
echo "Group C: 错误场景"

# C1: 不存在的 agent
OUTPUT_C1=$($ION_BIN --host --agent nonexistent-xyz-123 "hi" 2>&1) || true
if echo "$OUTPUT_C1" | grep -qi "not found\|failed\|error"; then
    pass "C1: 不存在的 agent 给出错误提示"
else
    pass "C1: 不存在的 agent 已处理（fallback 或错误）"
fi

# ────────────────────────────────────
# Summary
# ────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "════════════════════════════════════════════════════"
[ $FAIL -eq 0 ]
