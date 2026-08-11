#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Hooks prompt handler deny CI（场景 1：直接执行）
#
# 验证：场景 1（ion "..."，无 --host）下，prompt handler 能调 LLM 判断
#       并用内置 schema（{"decision":"block"}）阻断工具执行。
#
# 这是 hooks 系统最轻量的"让 LLM 判断是否继续执行"能力——
# 不需要 spawn 子 Worker，不需要 host 引擎，场景 1 直接可用。
#
# 链路：
#   主 Agent 调 bash(touch MARKER)
#     → PreToolUse hook 触发
#     → prompt handler → 调 LLM（FauxProvider 第 2 条响应）
#     → LLM 返回 {"decision":"block","reason":"..."}
#     → interpret_stdout 解析 → block=true
#     → ToolDenied → bash 不执行（MARKER 不创建）
#     → 主 Agent 收到 deny 错误，继续回复（第 3 条响应）
#
# 依赖：ion 二进制（脚本会先 build）
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
echo "  Hooks prompt handler deny CI（场景 1）— $(date)"
echo "══════════════════════════════════════════════════════"

# Phase 0: Build
echo "── Phase 0: Build ──"
cargo build --bin ion 2>&1 | tail -2
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || { echo "ion binary missing"; exit 1; }

# 准备隔离的测试环境
TEST_ROOT="$(mktemp -d /tmp/ion-hooks-prompt-deny-XXXXXX)"
TEST_HOME="$TEST_ROOT/home"
TEST_PROJECT="$TEST_ROOT/project"
MARKER="$TEST_ROOT/tool-ran.txt"
SESSION_ID="sess_hooks_prompt_deny_ci"
HTML="$TEST_ROOT/export.html"
mkdir -p "$TEST_HOME" "$TEST_PROJECT/.ion"

cleanup() {
    if [ "${KEEP_TEST_ROOT:-0}" = "1" ]; then
        printf '  debug artifacts: %s\n' "$TEST_ROOT"
    else
        rm -rf "$TEST_ROOT"
    fi
}
trap cleanup EXIT

echo ""
echo "── Group G: prompt handler 在场景 1 下能 deny 工具 ──"

# hooks.json: PreToolUse → prompt handler（单轮 LLM 判断，内置 schema）
cat > "$TEST_PROJECT/.ion/hooks.json" <<'EOF'
{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "prompt",
            "prompt": "你是一个安全审查 Agent。判断即将执行的 bash 命令是否安全。如果命令会修改文件系统（如 touch/rm/mv），判定为危险。",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
EOF

# FauxProvider script（全局 FIFO 队列，主 Agent 和 prompt handler 共享）：
#   第 1 条：主 Agent 发 bash tool_call（触发 PreToolUse）
#   第 2 条：prompt handler 的 LLM 返回 block 决策（interpret_stdout 解析）
#   第 3 条：主 Agent 收到 deny 后的续命回复
cat > "$TEST_ROOT/faux.jsonl" <<JSONL
{"tool_call":{"name":"bash","input":{"command":"touch $MARKER"}}}
{"text":"{\"decision\":\"block\",\"reason\":\"touch 会修改文件系统，判定为危险操作\"}"}
{"text":"Bash 被安全审查拒绝了，没有执行。"}
JSONL

# 跑：场景 1（直接执行，无 --host）
cd "$TEST_PROJECT"
OUTPUT=$(HOME="$TEST_HOME" \
         ION_FAUX_SCRIPT="$TEST_ROOT/faux.jsonl" \
         ION_GRACEFUL_DRAIN_MS=0 \
         "$ION_BIN" \
           --no-context-files \
           --provider faux \
           --model faux-test \
           --session-id "$SESSION_ID" \
           --export "$HTML" \
           "请执行 bash 命令" 2>&1)
RUN_EXIT=$?
cd "$PROJECT_DIR"

echo "  运行退出码：$RUN_EXIT"
echo "  输出摘要（最后 10 行）："
echo "$OUTPUT" | tail -10 | sed 's/^/    /'
echo ""

# 验证 1：bash 被拒绝（MARKER 文件不存在）
if [ ! -e "$MARKER" ]; then
    pass "G1 prompt handler 成功 deny 了 bash（MARKER 文件未创建）"
else
    fail "G1 prompt handler 成功 deny 了 bash（MARKER 文件被创建了——deny 没生效）"
fi

# 验证 2：主 Agent 收到 deny 后继续回复了（没卡死）
if echo "$OUTPUT" | grep -qi "拒绝\|没有执行"; then
    pass "G2 主 Agent 收到 deny 后正常回复（没卡死）"
else
    fail "G2 主 Agent 收到 deny 后正常回复"
fi

# 验证 3：session.jsonl 里有 is_error=true 的 ToolResult
SESSION_FILE=$(find "$TEST_HOME/.ion/agent/sessions" -name "${SESSION_ID}*.jsonl" -print -quit 2>/dev/null)
if [ -n "$SESSION_FILE" ] && [ -s "$SESSION_FILE" ]; then
    pass "G3 session.jsonl 已生成"
    if jq -s -e '[.[] | select(.type=="message") | .message | to_entries[] | select(.key=="ToolResult")] | any(.value.is_error == true)' \
        "$SESSION_FILE" >/dev/null 2>&1; then
        pass "G4 session.jsonl 里有 is_error=true 的 ToolResult（deny 生效）"
    else
        fail "G4 session.jsonl 里有 is_error=true 的 ToolResult"
    fi
else
    fail "G3 session.jsonl 已生成"
    yellow "  ⚠️  跳过 G4（无 session 文件）"
fi

# 验证 5：hook_event 审计记录存在
if [ -n "$SESSION_FILE" ] && jq -s -e '[.[] | select(.customType == "hook_event")] | length > 0' \
    "$SESSION_FILE" >/dev/null 2>&1; then
    pass "G5 session.jsonl 里有 hook_event 审计记录"
else
    yellow "  ⚠️  没找到 hook_event 审计记录"
    fail "G5 session.jsonl 里有 hook_event 审计记录"
fi

echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
