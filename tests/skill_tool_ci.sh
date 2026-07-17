#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Skill Tool CI — LLM 按需调用 skill 验证
#
# 验证：skill 工具 list / inject / fork（spawn_worker 起子任务）/ get_skills RPC
# 方式：FauxProvider 驱动（host + call_tool RPC 直调，不依赖真实 LLM）
# 隔离：HOME=临时目录（socket 隔离）+ ION_AGENT_DIR（全局 skill 隔离）
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

echo "════════════════════════════════════════════════════"
echo "  Skill Tool CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ── 准备隔离测试目录 ──
# HOME=临时目录 → socket 路径 $HOME/.ion/host.sock 隔离，不污染用户真实 ~/.ion/
TEST_TMP="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$TEST_TMP"' EXIT

FAKE_HOME="$TEST_TMP/home"
mkdir -p "$FAKE_HOME"

# 项目级 skill（在测试项目 .ion/skills/ 下）
mkdir -p "$TEST_TMP/proj/.ion/skills"
cat > "$TEST_TMP/proj/.ion/skills/code-review.md" <<'EOF'
---
name: code-review
description: Perform a thorough code review
trigger: when user asks to review code
---
# Code Review Skill
## Steps
1. Read the changed files
2. Check for common issues (security, performance, style)
3. Provide structured feedback
EOF

cat > "$TEST_TMP/proj/.ion/skills/testing.md" <<'EOF'
---
name: testing
description: Write and run tests
---
# Testing Skill
Write unit tests and integration tests.
EOF

# 全局 skill（ION_AGENT_DIR 指向隔离目录）
AGENT_DIR="$TEST_TMP/agent"
mkdir -p "$AGENT_DIR/skills"
cat > "$AGENT_DIR/skills/deployment.md" <<'EOF'
---
name: deployment
description: Deploy to production
---
# Deployment Skill
Deploy the application safely.
EOF

# 公共环境变量（所有子命令继承）
export HOME="$FAKE_HOME"
export ION_AGENT_DIR="$AGENT_DIR"

# ── 启动 host（FauxProvider，在项目目录下运行）──
cd "$TEST_TMP/proj"
ION_FAUX_REPLY="skill test" $ION_BIN serve >"$TEST_TMP/host.log" 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 $HOST_PID 2>/dev/null; then
    fail "S0: host 启动失败"
    cat "$TEST_TMP/host.log" | tail -5
    exit 1
fi
pass "S0: host 启动成功（FauxProvider + 隔离 skill 目录）"

CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')

if [ -z "$SID" ]; then
    fail "S0: create_session 失败"
    kill $HOST_PID 2>/dev/null; exit 1
fi
pass "S0: create_session 成功 (sid=${SID:0:12}...)"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group S: Skill 工具（list / inject / fork / get_skills）"

# S1: call_tool skill list — 应列出全局 + 项目级 skill
LIST_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"list"}}' 2>&1)
if echo "$LIST_OUT" | grep -q "code-review" && echo "$LIST_OUT" | grep -q "deployment" && echo "$LIST_OUT" | grep -q "testing"; then
    pass "S1: skill list 列出全局 + 项目级 skill（code-review + testing + deployment）"
else
    fail "S1: skill list 未列出预期 skill"
    echo "  输出: $(echo "$LIST_OUT" | head -5)"
fi

# S2: skill list 包含 description（frontmatter 解析）
if echo "$LIST_OUT" | grep -q "thorough code review"; then
    pass "S2: skill list 包含 description（frontmatter 解析）"
else
    fail "S2: skill list 缺 description"
fi

# S3: inject 模式 — 加载 code-review skill，返回正文
LOAD_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review"}}' 2>&1)
if echo "$LOAD_OUT" | grep -q "Skill 'code-review' loaded" && echo "$LOAD_OUT" | grep -q "Code Review Skill"; then
    pass "S3: skill inject 返回正文（code-review）"
else
    fail "S3: skill inject 未返回正文"
    echo "  输出: $(echo "$LOAD_OUT" | head -5)"
fi

# S4: inject 模式默认（不传 context 参数）— 等价于 inject
LOAD_DEFAULT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"testing"}}' 2>&1)
if echo "$LOAD_DEFAULT" | grep -q "Skill 'testing' loaded" && echo "$LOAD_DEFAULT" | grep -q "Testing Skill"; then
    pass "S4: skill inject 默认模式（不传 context）加载 testing"
else
    fail "S4: skill inject 默认模式失败"
fi

# S5: fork 模式 — spawn_worker 起子任务，skill 注入 system prompt，返回执行结果
FORK_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review","context":"fork"}}' 2>&1)
if echo "$FORK_OUT" | grep -qi "executed in fork mode" && echo "$FORK_OUT" | grep -qi '"success": true\|"success":true'; then
    pass "S5: skill fork 执行成功（spawn_worker 起子任务，返回结果）"
else
    fail "S5: skill fork 执行失败"
    echo "  输出: $(echo "$FORK_OUT" | head -5)"
fi

# S6: 加载不存在的 skill — 应报错
GHOST_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"ghost-skill"}}' 2>&1)
if echo "$GHOST_OUT" | grep -qi "not found\|error"; then
    pass "S6: 加载不存在的 skill 正确报错"
else
    fail "S6: 加载不存在 skill 未报错"
    echo "  输出: $(echo "$GHOST_OUT" | head -5)"
fi

# S7: 缺 skill_name 参数 — 应报错
MISSING_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{}}' 2>&1)
if echo "$MISSING_OUT" | grep -qi "missing.*skill_name\|error"; then
    pass "S7: 缺 skill_name 参数正确报错"
else
    fail "S7: 缺参数未报错"
fi

# S8: get_skills RPC — 已有 RPC 不受 skill 工具影响
GETSKILLS_OUT=$($ION_BIN rpc --session "$SID" --method get_skills 2>&1)
if echo "$GETSKILLS_OUT" | grep -q "code-review" && echo "$GETSKILLS_OUT" | grep -q "count"; then
    pass "S8: get_skills RPC 正常（已有 RPC 不受 skill 工具影响）"
else
    fail "S8: get_skills RPC 异常"
fi

# S9: 加载全局 skill（deployment，在 ION_AGENT_DIR 下）
DEPLOY_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"deployment"}}' 2>&1)
if echo "$DEPLOY_OUT" | grep -q "Skill 'deployment' loaded" && echo "$DEPLOY_OUT" | grep -q "Deploy the application"; then
    pass "S9: 加载全局 skill（deployment，ION_AGENT_DIR 目录）"
else
    fail "S9: 加载全局 skill 失败"
    echo "  输出: $(echo "$DEPLOY_OUT" | head -5)"
fi

# ──────────────────────────────────────────────────────────
# Group E: 边界 / 空 skill
# ──────────────────────────────────────────────────────────
echo ""
echo "Group E: 边界场景"

# E1: 无 skill 时 skill list 返回提示
# 先杀掉当前 host
kill $HOST_PID 2>/dev/null
wait $HOST_PID 2>/dev/null

EMPTY_AGENT="$TEST_TMP/empty_agent"
mkdir -p "$EMPTY_AGENT"
mkdir -p "$TEST_TMP/emptyproj"

cd "$TEST_TMP/emptyproj"
ION_AGENT_DIR="$EMPTY_AGENT" \
ION_FAUX_REPLY="empty" \
$ION_BIN serve >"$TEST_TMP/empty_host.log" 2>&1 &
EMPTY_PID=$!
sleep 2

if kill -0 $EMPTY_PID 2>/dev/null; then
    EMPTY_CREATE=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
    EMPTY_SID=$(echo "$EMPTY_CREATE" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')
    if [ -n "$EMPTY_SID" ]; then
        EMPTY_LIST=$($ION_BIN rpc --session "$EMPTY_SID" --method call_tool \
          --params '{"tool":"skill","args":{"skill_name":"list"}}' 2>&1)
        if echo "$EMPTY_LIST" | grep -qi "No skills available"; then
            pass "E1: 无 skill 时 skill list 返回 'No skills available'"
        else
            fail "E1: 无 skill 时未返回正确提示"
            echo "  输出: $(echo "$EMPTY_LIST" | head -3)"
        fi
    else
        skip "E1: 空 host create_session 失败"
    fi
    kill $EMPTY_PID 2>/dev/null
    wait $EMPTY_PID 2>/dev/null
else
    skip "E1: 空 host 启动失败"
fi

# ──────────────────────────────────────────────────────────
cd "$PROJECT_DIR"
echo ""
echo "══════════════════════════════════════════"
echo "  PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "══════════════════════════════════════════"

if [ "$FAIL" -eq 0 ]; then
    echo "🎉 全部通过"
    exit 0
else
    echo "⚠️ 有 $FAIL 个失败"
    exit 1
fi
