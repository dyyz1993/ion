#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Hooks agent handler CI — 验证 agent handler 真能 spawn 子 Worker
#
# 这是 hooks 系统的"场景 2 端到端测试"：
#   ion --host + FauxProvider（不调真 LLM）+ hooks.json (agent handler)
#
# 验证链路：
#   主 Worker 跑完（Stop 事件触发）
#     → HookExtension 读到 agent handler
#     → run_agent → Runtime::spawn_worker
#     → Manager 创建子 Worker
#     → 子 Worker 跑完（FauxProvider 驱动）
#
# 依赖：ion + ion-worker 二进制（脚本会先 build）
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
yellow(){ printf "\033[33m%s\033[0m\n" "$1"; }
pass() { green "  ✅ $1"; PASS=$((PASS+1)); }
fail() { red "  ❌ $1"; FAIL=$((FAIL+1)); }

echo "══════════════════════════════════════════════════════"
echo "  Hooks agent handler CI — $(date)"
echo "══════════════════════════════════════════════════════"

# Phase 0: Build
echo "── Phase 0: Build ──"
cargo build --bin ion --bin ion-worker 2>&1 | tail -2
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || { echo "ion binary missing"; exit 1; }

# 准备测试目录
TEST_DIR=$(mktemp -d)
mkdir -p "$TEST_DIR/.ion"

echo ""
echo "── Group E: agent handler 真能 spawn 子 Worker ──"

# E1: Stop 事件 → agent handler → spawn 子 Worker
#
# 设计：
#   - hooks.json 配 Stop 事件 → agent handler
#   - FauxProvider script：主 Worker 回文本"任务完成"（触发 Stop）
#   - agent handler spawn 子 Worker（也用 FauxProvider，子 Worker 回"同步完成"）
#   - 验证：输出里出现子 Worker 的日志（wkr_ 开头）
#
# FauxProvider script 格式：每行一个 JSON {"text":"..."}，FIFO 消费
# 主 Worker 需要至少 1 条响应；子 Worker 需要至少 1 条

cat > "$TEST_DIR/.ion/hooks.json" <<'EOF'
{
  "version": 1,
  "hooks": {
    "Stop": [
      {
        "loop_limit": 1,
        "hooks": [
          {
            "type": "agent",
            "agent": "default",
            "prompt": "检查项目状态，报告完成",
            "model": "faux",
            "max_turns": 3,
            "timeout": 30,
            "once": true
          }
        ]
      }
    ]
  }
}
EOF

# FauxProvider script：多条响应（主 Worker + 子 Worker 各消费）
# 主 Worker 第 1 轮：回文本（触发 Stop）
# 子 Worker 第 1 轮：回文本（agent handler spawn 的子 Worker）
cat > "$TEST_DIR/faux_script.jsonl" <<'EOF'
{"text":"任务完成"}
{"text":"同步检查完成"}
{"text":"done"}
{"text":"done"}
EOF

cd "$TEST_DIR"
OUTPUT=$(ION_FAUX_SCRIPT="$TEST_DIR/faux_script.jsonl" \
         ION_HOST_TIMEOUT=30 \
         timeout 60 "$ION_BIN" --no-session --host "测试 agent handler" 2>&1)

echo "  输出摘要（最后 15 行）："
echo "$OUTPUT" | tail -15 | sed 's/^/    /'
echo ""

# 验证 1：主 Worker 跑起来了
if echo "$OUTPUT" | grep -q "任务完成"; then
    pass "E1 主 Worker 用 FauxProvider 回放成功"
else
    fail "E1 主 Worker 用 FauxProvider 回放成功"
fi

# 验证 2：子 Worker 被 spawn（host 日志或事件里有第二个 worker）
# host 的 event pump 会打印 [wkr_xxxx] 格式的日志
WKR_COUNT=$(echo "$OUTPUT" | grep -oE '\[wkr_[a-f0-9]' | sort -u | wc -l | tr -d ' ')
if [ "$WKR_COUNT" -ge 2 ]; then
    pass "E2 agent handler spawn 了子 Worker（检测到 $WKR_COUNT 个 worker）"
else
    # 也可能是 agent handler 没被触发，或 spawn 失败
    # 检查日志里有没有 hooks/agent 相关信息
    if echo "$OUTPUT" | grep -qi "hooks.*agent\|agent handler"; then
        yellow "  ⚠️  检测到 hooks agent 日志但 spawn 可能失败（worker 数=$WKR_COUNT）"
    fi
    fail "E2 agent handler spawn 了子 Worker（只检测到 $WKR_COUNT 个 worker，期望 >= 2）"
fi

# 验证 3：子 Worker 真的跑完了（有 >= 2 个 "✓ done"，说明主 Worker + 子 Worker 都完成了）
DONE_COUNT=$(echo "$OUTPUT" | grep -c "✓ done")
if [ "$DONE_COUNT" -ge 2 ]; then
    pass "E3 子 Worker 跑完（检测到 $DONE_COUNT 个 done）"
else
    fail "E3 子 Worker 跑完（只检测到 $DONE_COUNT 个 done，期望 >= 2）"
fi

# 验证 4：没有死循环（worker 数 <= 3，不会像之前那样 spawn 16 个）
if [ "$WKR_COUNT" -le 3 ]; then
    pass "E4 没有死循环（worker 数=$WKR_COUNT <= 3）"
else
    fail "E4 没有死循环（worker 数=$WKR_COUNT > 3，可能有递归）"
fi

# 清理
rm -rf "$TEST_DIR"

echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
