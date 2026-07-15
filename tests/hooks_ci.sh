#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Hooks CI — 验证 hooks 系统的核心场景
#
# 这个脚本是 hooks 功能的 CI 验证。用户想看"哪些用例验证过了"，
# 看这个文件即可。每个 Group 对应 HOOKS_GUIDE.md 附录 B 的一个教程。
#
# 验证：
#   Group A:  配置加载 + 热重载（HOOKS_GUIDE §4）
#   Group B:  B.1 拦截 git --no-verify（PreToolUse + exit 2 block）
#   Group C:  B.2 注入项目约定（UserPromptSubmit + stdout 注入）
#   Group D:  B.3 Stop 强制检查测试（Stop + exit 2 block + 通过放行）
#
# 依赖：jq
# 用法：bash tests/hooks_ci.sh
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

# 找到 hooks_test.sh
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOKS_TEST="$PROJECT_DIR/scripts/hooks_test.sh"
chmod +x "$HOOKS_TEST" 2>/dev/null || true

# 检查 jq
if ! command -v jq &>/dev/null; then
    echo "❌ 需要 jq"
    exit 1
fi

# 临时测试目录
WORKDIR=$(mktemp -d)
trap "rm -rf $WORKDIR" EXIT

echo "════════════════════════════════════════════════════"
echo "  Hooks CI — $(date)"
echo "════════════════════════════════════════════════════"
echo ""

# ═════════════════════════════════════════════════════════
# Group A: 配置加载 + 热重载
# ═════════════════════════════════════════════════════════
echo "── Group A: 配置加载 + 热重载 ──"

mkdir -p "$WORKDIR/a/.ion"
echo '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"command","command":"echo hi"}]}}' \
    > "$WORKDIR/a/.ion/hooks.json"

cd "$WORKDIR/a"
OUT=$("$HOOKS_TEST" validate .ion/hooks.json 2>&1)
if echo "$OUT" | grep -q "✅"; then
    pass "A1 validate 配置格式正确"
else
    fail "A1 validate 配置格式正确"
    echo "$OUT"
fi

OUT=$("$HOOKS_TEST" list 2>&1)
if echo "$OUT" | grep -q "UserPromptSubmit"; then
    pass "A2 list 列出生效事件"
else
    fail "A2 list 列出生效事件"
    echo "$OUT"
fi

# 热重载：改配置后立即生效
echo '{"version":1,"hooks":{"Stop":[{"type":"command","command":"echo bye"}]}}' \
    > "$WORKDIR/a/.ion/hooks.json"
OUT=$("$HOOKS_TEST" list 2>&1)
if echo "$OUT" | grep -q "Stop"; then
    pass "A3 热重载（改完立即反映 Stop 事件）"
else
    fail "A3 热重载（改完立即反映 Stop 事件）"
    echo "$OUT"
fi
echo ""

# ═════════════════════════════════════════════════════════
# Group B: B.1 拦截 git --no-verify（PreToolUse + exit 2）
# ═════════════════════════════════════════════════════════
echo "── Group B: 拦截 git --no-verify（对应教程 B.1）──"

mkdir -p "$WORKDIR/b/.ion/scripts"
cat > "$WORKDIR/b/.ion/hooks.json" <<'EOF'
{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      {"matcher":"bash","hooks":[{"type":"command","command":"bash .ion/scripts/block_no_verify.sh","timeout":5}]}
    ]
  }
}
EOF

cat > "$WORKDIR/b/.ion/scripts/block_no_verify.sh" <<'SCRIPT'
#!/bin/bash
INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""')
if echo "$COMMAND" | grep -qi "git.*--no-verify"; then
    echo '{"decision":"block","reason":"禁止使用 --no-verify"}'
    exit 2
fi
exit 0
SCRIPT
chmod +x "$WORKDIR/b/.ion/scripts/block_no_verify.sh"

cd "$WORKDIR/b"

OUT=$("$HOOKS_TEST" test PreToolUse \
    --stdin '{"tool_name":"bash","tool_input":{"command":"git commit --no-verify -m test"}}' 2>&1)
if echo "$OUT" | grep -q "BLOCK" && echo "$OUT" | grep -q "exit code: 2"; then
    pass "B1 git --no-verify 被 BLOCK（exit 2 + reason）"
else
    fail "B1 git --no-verify 被 BLOCK（exit 2 + reason）"
    echo "$OUT"
fi

OUT=$("$HOOKS_TEST" test PreToolUse \
    --stdin '{"tool_name":"bash","tool_input":{"command":"git commit -m test"}}' 2>&1)
if echo "$OUT" | grep -q "CONTINUE"; then
    pass "B2 正常 git commit 放行（CONTINUE）"
else
    fail "B2 正常 git commit 放行（CONTINUE）"
    echo "$OUT"
fi
echo ""

# ═════════════════════════════════════════════════════════
# Group C: B.2 注入项目约定（UserPromptSubmit + stdout 注入）
# ═════════════════════════════════════════════════════════
echo "── Group C: 注入项目约定（对应教程 B.2）──"

mkdir -p "$WORKDIR/c/.ion/scripts"
cat > "$WORKDIR/c/.ion/hooks.json" <<'EOF'
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [
      {"type":"command","command":"bash .ion/scripts/inject_conventions.sh","timeout":3}
    ]
  }
}
EOF

cat > "$WORKDIR/c/.ion/scripts/inject_conventions.sh" <<'SCRIPT'
#!/bin/bash
CONV=".ion/conventions.md"
[ ! -f "$CONV" ] && exit 0
echo "=== 项目代码约定 ==="
cat "$CONV"
SCRIPT
chmod +x "$WORKDIR/c/.ion/scripts/inject_conventions.sh"

echo "- 用 Rust 写代码
- 错误处理用 anyhow" > "$WORKDIR/c/.ion/conventions.md"

cd "$WORKDIR/c"

OUT=$("$HOOKS_TEST" test UserPromptSubmit --stdin '{"prompt":"写个函数"}' 2>&1)
if echo "$OUT" | grep -q "CONTINUE" && echo "$OUT" | grep -q "项目代码约定"; then
    pass "C1 UserPromptSubmit 注入约定内容（CONTINUE + additionalContext）"
else
    fail "C1 UserPromptSubmit 注入约定内容（CONTINUE + additionalContext）"
    echo "$OUT"
fi
echo ""

# ═════════════════════════════════════════════════════════
# Group D: B.3 Stop 强制检查测试（exit 2 block + 通过放行）
# ═════════════════════════════════════════════════════════
echo "── Group D: Stop 强制检查测试（对应教程 B.3）──"

mkdir -p "$WORKDIR/d/.ion/scripts"
cat > "$WORKDIR/d/.ion/hooks.json" <<'EOF'
{
  "version": 1,
  "hooks": {
    "Stop": [
      {"loop_limit":3,"hooks":[{"type":"command","command":"bash .ion/scripts/check_tests.sh","timeout":60}]}
    ]
  }
}
EOF

cat > "$WORKDIR/d/.ion/scripts/check_tests.sh" <<'SCRIPT'
#!/bin/bash
if [ -f .ion/FAIL ]; then
    echo '{"decision":"block","reason":"测试失败，请修复"}'
    exit 2
fi
exit 0
SCRIPT
chmod +x "$WORKDIR/d/.ion/scripts/check_tests.sh"

cd "$WORKDIR/d"

# 场景 1：测试失败（FAIL 文件存在）
touch .ion/FAIL
OUT=$("$HOOKS_TEST" test Stop --stdin '{"last_assistant_message":"做完了"}' 2>&1)
if echo "$OUT" | grep -q "BLOCK" && echo "$OUT" | grep -q "测试失败"; then
    pass "D1 测试失败时 Stop 被 BLOCK（exit 2 + reason）"
else
    fail "D1 测试失败时 Stop 被 BLOCK（exit 2 + reason）"
    echo "$OUT"
fi

# 场景 2：测试通过（删除 FAIL 文件）
rm -f .ion/FAIL
OUT=$("$HOOKS_TEST" test Stop --stdin '{"last_assistant_message":"做完了"}' 2>&1)
if echo "$OUT" | grep -q "CONTINUE"; then
    pass "D2 测试通过时 Stop 放行（CONTINUE）"
else
    fail "D2 测试通过时 Stop 放行（CONTINUE）"
    echo "$OUT"
fi
echo ""

# ═════════════════════════════════════════════════════════
echo "══════════════════════════════════════════════════════"
echo "  结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
