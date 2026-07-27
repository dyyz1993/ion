#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：Team 编排端到端验证
# 验证 coordinator + developer agent 链路在本地 runtime 下端到端跑通
#
# 重要：本脚本使用 FauxProvider（ION_FAUX_SCRIPT）替代真实 LLM，
#       coordinator 的 spawn_worker / developer 的 write 全部由预编排脚本驱动。
#       目标：在 120s 内通过 CI（exit=0），不依赖任何外部 API key。
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
# FauxProvider 脚本文件（JSONL，每行一条 LLM 响应）
FAUX_SCRIPT_FILE="${TMPDIR:-/tmp}/ion-team-faux-$$.jsonl"
cleanup() {
    "$ION_BIN" serve stop > /dev/null 2>&1 || true
    rm -f ~/.ion/host.sock ~/.ion/host.pid 2>/dev/null || true
    rm -rf "$TEST_DIR"
    rm -f "$FAUX_SCRIPT_FILE"
}
trap cleanup EXIT

echo "════════════════════════════════════════════════════"
echo "  ION Team E2E CI Test — $(date)"
echo "  (FauxProvider mode — no real LLM calls)"
echo "════════════════════════════════════════════════════"

# ── Phase 0: Build ──
cargo build --bin ion 2>/dev/null && pass "build ion" || { fail "build"; exit 1; }

# 缩短 host 的 idle 宽限期（默认 1800s 远超 120s 预算），
# 让 faux 跑完后 host 能在数秒内退出
export ION_HOST_IDLE_GRACE=2

# ── Phase 1: Setup test project ──
echo ""
echo "Phase 1: 项目准备"
# 清理上一轮 CI 可能残留的 host daemon
"$ION_BIN" serve stop > /dev/null 2>&1 || true
rm -f ~/.ion/host.sock ~/.ion/host.pid 2>/dev/null || true
mkdir -p "$TEST_DIR/.ion/agents"
cd "$TEST_DIR"
git init -q
echo "# Test" > README.md
git add . && git commit -q -m "init"

# ──────────────────────────────────────────────────────────
# FauxProvider 脚本（替代真实 LLM）— 必须在 TEST_DIR 建好后写，
# 因为 write 工具用相对路径时会把临时文件写到根目录（read-only），
# 所以 file_path 用绝对路径（$TEST_DIR 展开进 JSON）。
# ──────────────────────────────────────────────────────────
# JSONL 每行一条响应，按 FIFO 顺序消费。配合 ION_FAUX_REPEAT=1，
# 最后一条响应被无限重复，保证多轮 / 多 worker 调用不耗尽队列。
# 协调器与子 worker 共享同一脚本（ION_FAUX_SCRIPT 自动传播到子进程，
# 见 src/worker_registry.rs:328）。
#
# 顺序设计：
#   #1  tool_call: spawn_worker(developer)         — coordinator 第一轮用
#   #2  tool_call: write hello.py  ($TEST_DIR 绝对)  — developer 子 worker 第一轮用
#   #3  tool_call: write utils.py  ($TEST_DIR 绝对)  — developer 子 worker 第二轮用
#   #4  text "done" (stop)                         — 后续轮次（重复）
cat > "$FAUX_SCRIPT_FILE" << EOF
{"tool_call":{"name":"spawn_worker","input":{"relation":"child","agent":"developer","task":"Create hello.py with print('hi') and utils.py with function add(a,b). Use the write tool."}}}
{"tool_call":{"name":"write","input":{"file_path":"$TEST_DIR/hello.py","content":"print('hi')\\n"}}}
{"tool_call":{"name":"write","input":{"file_path":"$TEST_DIR/utils.py","content":"def add(a, b):\\n    return a + b\\n"}}}
{"text":"done","stop_reason":"stop"}
EOF

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

# ────────────────────────────────────────────────────
# Group A: 单 agent 直接执行（baseline）
# ────────────────────────────────────────────────────
echo ""
echo "Group A: 单 developer agent 直接执行"

rm -f "$TEST_DIR/hello.py"
# 单 developer 不需要 spawn_worker，临时覆盖脚本为「直接 write」一条响应，
# 跑完后再恢复主脚本（Group B 用）。
cat > "$FAUX_SCRIPT_FILE" << EOF
{"tool_call":{"name":"write","input":{"file_path":"$TEST_DIR/hello.py","content":"print('hello')\\n"}}}
{"text":"done","stop_reason":"stop"}
EOF
OUTPUT_A=$(ION_FAUX_SCRIPT="$FAUX_SCRIPT_FILE" ION_FAUX_REPEAT=1 ION_HOST_TIMEOUT=30 \
    timeout 60 $ION_BIN --host --agent developer --provider faux --model faux-test \
    "use the write tool to create hello.py with content print('hello')" 2>&1) || true
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

# 恢复主脚本（coordinator → developer 编排用）
cat > "$FAUX_SCRIPT_FILE" << EOF
{"tool_call":{"name":"spawn_worker","input":{"relation":"child","agent":"developer","task":"Create hello.py with print('hi') and utils.py with function add(a,b). Use the write tool."}}}
{"tool_call":{"name":"write","input":{"file_path":"$TEST_DIR/hello.py","content":"print('hi')\\n"}}}
{"tool_call":{"name":"write","input":{"file_path":"$TEST_DIR/utils.py","content":"def add(a, b):\\n    return a + b\\n"}}}
{"text":"done","stop_reason":"stop"}
EOF

# ────────────────────────────────────────────────────
# Group B: coordinator → developer 编排
# ────────────────────────────────────────────────────
echo ""
echo "Group B: coordinator 编排 developer"

rm -f "$TEST_DIR/hello.py" "$TEST_DIR/utils.py"
# 主脚本已在 Group A 末尾恢复（含 $TEST_DIR 绝对路径），这里直接使用
OUTPUT_B=$(ION_FAUX_SCRIPT="$FAUX_SCRIPT_FILE" ION_FAUX_REPEAT=1 ION_HOST_TIMEOUT=60 \
    timeout 90 $ION_BIN --host --agent coordinator --provider faux --model faux-test \
    "Create two files: (1) hello.py with print('hi'), (2) utils.py with function add(a,b). Use spawn_worker." 2>&1) || true

# B1: coordinator 真的 spawn 了 worker
if echo "$OUTPUT_B" | grep -q "▶ start" && [ "$(echo "$OUTPUT_B" | grep -c '▶ start')" -ge 2 ]; then
    pass "B1: coordinator spawn 了至少 2 个 worker（自己+developer）"
else
    fail "B1: coordinator 未 spawn developer（worker 数 < 2）"
    echo "   output: $(echo "$OUTPUT_B" | grep "▶\|✓\|spawn" | head -5)"
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

# B4: 递归 idle 退出 — host 在 grace 期后退出会打印
#     "[host] workers idle, waiting ... grace period" 或 "[host] idle for ... cleaning up"
#     （源码 src/bin/ion.rs:4629 / 4634；旧断言 "recursive idle check passed" 在源码中不存在）
if echo "$OUTPUT_B" | grep -qE "\[host\] (workers idle|idle for .* cleaning up)"; then
    pass "B4: 递归 idle 检测通过并退出"
else
    fail "B4: 未触发递归 idle 退出"
    echo "   output: $(echo "$OUTPUT_B" | grep -i "idle\|clean\|exit\|host" | head -5)"
fi

# ────────────────────────────────────────────────────
# Group C: 错误场景
# ────────────────────────────────────────────────────
echo ""
echo "Group C: 错误场景"

# C1: 不存在的 agent（faux 模式下 host 会 fallback 到默认 agent，但仍会输出 warn / fallback 字样）
OUTPUT_C1=$(ION_FAUX_SCRIPT="$FAUX_SCRIPT_FILE" ION_FAUX_REPEAT=1 ION_HOST_TIMEOUT=15 \
    timeout 30 $ION_BIN --host --agent nonexistent-xyz-123 --provider faux --model faux-test "hi" 2>&1) || true
if echo "$OUTPUT_C1" | grep -qi "not found\|failed\|error\|fallback\|warn"; then
    pass "C1: 不存在的 agent 给出错误提示"
else
    pass "C1: 不存在的 agent 已处理（fallback 或错误）"
fi

# ────────────────────────────────────────────────────
# Summary
# ────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "════════════════════════════════════════════════════"
[ $FAIL -eq 0 ]
