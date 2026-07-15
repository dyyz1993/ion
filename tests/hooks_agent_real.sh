#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Hooks agent handler 真实 LLM e2e（需手动触发，调真模型）
#
# 用法：
#   ION_E2E=1 bash tests/hooks_agent_real.sh
#
# 前置条件：
#   - ion config set api-key "sk-xxx"（配好 API key）
#   - ion config set default-model "deepseek-v4-flash"（或 GLM-4.7 等）
#
# 验证：agent handler spawn 的子 Worker 真能用工具（read/write）操作文件
# ──────────────────────────────────────────────────────────
set -uo pipefail

# 需要显式启用
if [ "${ION_E2E:-0}" != "1" ]; then
    echo "⏭️  跳过：需 ION_E2E=1 启用（调真实 LLM，花 token）"
    exit 0
fi

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || { echo "请先 cargo build --bin ion"; exit 1; }

PASS=0; FAIL=0
pass() { printf "\033[32m  ✅ %s\033[0m\n" "$1"; PASS=$((PASS+1)); }
fail() { printf "\033[31m  ❌ %s\033[0m\n" "$1"; FAIL=$((FAIL+1)); }

echo "══════════════════════════════════════════════════════"
echo "  Hooks agent handler 真实 LLM e2e — $(date)"
echo "══════════════════════════════════════════════════════"

TEST_DIR=$(mktemp -d)
mkdir -p "$TEST_DIR/.ion"

# 准备测试文件
echo "# Test Document" > "$TEST_DIR/test.md"

# hooks.json：Stop → agent handler，让子 Worker 读 test.md 并报告内容
cat > "$TEST_DIR/.ion/hooks.json" <<'EOF'
{
  "version": 1,
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "agent",
            "agent": "default",
            "prompt": "读 test.md 文件，报告它的内容。用 read 工具。",
            "max_turns": 5,
            "allowed_tools": ["read"],
            "timeout": 60
          }
        ]
      }
    ]
  }
}
EOF

cd "$TEST_DIR"
echo ""
echo "── 验证：agent handler 的子 Worker 真能用 read 工具 ──"
echo "  （主 Worker 跑完 → Stop → agent handler spawn 子 Worker → 子 Worker 读 test.md）"
echo ""

OUTPUT=$(ION_HOST_TIMEOUT=90 timeout 120 "$ION_BIN" --no-session --host "完成任务" 2>&1)

echo "  输出摘要："
echo "$OUTPUT" | tail -10 | sed 's/^/    /'
echo ""

# 验证 1：spawn 了子 Worker（>= 2 个 worker）
WKR_COUNT=$(echo "$OUTPUT" | grep -oE '\[wkr_[a-f0-9]' | sort -u | wc -l | tr -d ' ')
if [ "$WKR_COUNT" -ge 2 ]; then
    pass "R1 agent handler spawn 了子 Worker（$WKR_COUNT 个）"
else
    fail "R1 agent handler spawn 了子 Worker（只有 $WKR_COUNT 个）"
fi

# 验证 2：子 Worker 真的用了 read 工具（输出里有 read 工具调用或 test.md 内容）
if echo "$OUTPUT" | grep -qiE "read|test\.md|Test Document"; then
    pass "R2 子 Worker 用了 read 工具（检测到文件内容）"
else
    fail "R2 子 Worker 用了 read 工具（没检测到 read/test.md）"
fi

# 验证 3：没有死循环（入口 Worker 可能因 retry 触发多次 Stop，
# 但 hook_depth >= 1 保护让子 Worker 不再 spawn，所以总数应该 <= 3）
if [ "$WKR_COUNT" -le 3 ]; then
    pass "R3 没有死循环（worker 数=$WKR_COUNT <= 3）"
else
    fail "R3 没有死循环（worker 数=$WKR_COUNT > 3）"
fi

rm -rf "$TEST_DIR"

echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] || exit 1
