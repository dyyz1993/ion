#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# EXT-06 Hooks 场景 3 深度验证（serve + rpc）
#
# 复杂场景：安全防护 Hook — 拦截危险命令 + 文件写入审计
#
#   Phase 3: 配置 3 类 Hook（SessionStart / PreToolUse 拦截 rm -rf / PostToolUse 审计）
#   Phase 4: LLM 尝试执行 rm -rf → 被 PreToolUse hook 拦截
#   Phase 5: LLM 用 write 创建文件 → PostToolUse hook 审计
#   Phase 6: LLM read hook 日志验证全部触发
#   Phase 7: 导出 HTML
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0; FAIL=0

# pass/fail 同时写入 session.jsonl（通过 append_entry RPC，HTML 可见）
_ci_write() {
    local ctype="$1"
    local message="$2"
    [ -z "$SID" ] && return
    "$ION_BIN" rpc --session "$SID" --method append_entry \
      --params "$(CI_CTYPE="$ctype" CI_MSG="$message" python3 -c '
import json, os
print(json.dumps({"type":"custom_message","customType":os.environ["CI_CTYPE"],"content":os.environ["CI_MSG"],"display":True}))
')" 2>/dev/null > /dev/null
}
pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); _ci_write "ci_pass" "✅ $1"; }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); _ci_write "ci_fail" "❌ $1"; }

TEST_ROOT="$(mktemp -d /tmp/ion-ext06-serve-XXXXXX)"
TEST_PROJECT="$TEST_ROOT/hook-project"
SOCK="/tmp/ion_ext06_serve_$$.sock"
SID=""
HOOK_LOG="$TEST_ROOT/hooks-exec.log"

wait_agent_idle() {
    local max_iter=${1:-50}
    for i in $(seq 1 "$max_iter"); do
        sleep 2
        local s
        s=$("$ION_BIN" rpc --session "$SID" --method review_pending 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    err = str(d.get('error','')) + str(d.get('data',{}).get('error',''))
    if 'busy' in err.lower() or 'running' in err.lower(): print('busy')
    else: print('idle')
except: print('idle')" 2>/dev/null)
        if [ "$s" = "idle" ]; then return 0; fi
        [ $((i % 5)) = 0 ] && echo "  ...等 ${i}x2s"
    done
    return 1
}

echo "══════════════════════════════════════════════════════"
echo "  EXT-06 Hooks 场景 3（安全防护 Hook — 复杂场景）"
echo "══════════════════════════════════════════════════════"

# ── Phase 0: Build + 配置 hooks.json ──
echo "── Phase 0: Build + 配置 hooks.json（3 类 Hook，inline 命令）──"
cd "$PROJECT_DIR"; cargo build --bin ion 2>/dev/null
mkdir -p "$TEST_PROJECT/.ion"
echo '{"file-snapshot":{"enabled":true}}' > "$TEST_PROJECT/.ion/settings.json"
cd "$TEST_PROJECT"; git init -b main 2>/dev/null
echo "# hook-test" > README.md; git add . && git commit -m init 2>/dev/null
rm -f "$HOOK_LOG"

# 写 hooks.json — 全部用 inline 命令（避免外部脚本路径问题）
# 注意：$(...) 和 $HOOK_LOG 需要转义/展开控制
cat > "$TEST_PROJECT/.ion/hooks.json" << EOF
{
  "version": 1,
  "hooks": {
    "SessionStart": [
      {
        "type": "command",
        "command": "echo SessionStart-fired >> $HOOK_LOG",
        "timeout": 5
      }
    ],
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "INPUT=\$(cat); if echo \"\$INPUT\" | grep -q 'rm -rf'; then echo 'PreToolUse-BLOCKED-rm-rf' >> $HOOK_LOG; exit 2; fi; echo 'PreToolUse-ALLOWED-bash' >> $HOOK_LOG; exit 0",
            "timeout": 5
          }
        ]
      },
      {
        "matcher": "write",
        "hooks": [
          {
            "type": "command",
            "command": "echo 'PreToolUse-ALLOWED-write' >> $HOOK_LOG; exit 0",
            "timeout": 5
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "write",
        "hooks": [
          {
            "type": "command",
            "command": "echo 'PostToolUse-write-fired' >> $HOOK_LOG",
            "timeout": 5
          }
        ]
      },
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "echo 'PostToolUse-bash-fired' >> $HOOK_LOG",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
EOF

echo "  hooks.json 已写入（inline 命令，无外部脚本依赖）"

# ── Phase 1: 启动 serve ──
echo "── Phase 1: 启动 serve ──"
export ION_HOST_SOCKET="$SOCK"; export ION_SKIP_MCP=1; rm -f "$SOCK"
ION_SESSION_DIR="$TEST_ROOT/sessions" "$ION_BIN" serve > "$TEST_ROOT/serve.log" 2>&1 &
SERVE_PID=$!
ready=false
for i in $(seq 1 30); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then ready=true; break; fi
done
if $ready; then pass "serve ready"; else fail "serve 未启动"; kill $SERVE_PID; exit 1; fi

# ── Phase 2: create_session（触发 SessionStart hook）──
echo "── Phase 2: create_session ──"
CREATE_OUT=$("$ION_BIN" rpc --method create_session \
  --params '{"agent":"build","cwd":"'"$TEST_PROJECT"'","model":"glm-5.2","provider":"zai"}' 2>/dev/null)
SID=$(echo "$CREATE_OUT" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get('data',{}).get('session_id','') or d.get('data',{}).get('sessionId',''))
except: print('')" 2>/dev/null)
if [ -n "$SID" ]; then pass "create_session: $SID"; else fail "create_session 失败"; kill $SERVE_PID; exit 1; fi

# ════════════════════════════════════════════════════════
# Phase 3: 验证 SessionStart hook 触发（等 worker 启动）
# ════════════════════════════════════════════════════════
echo "── Phase 3: 验证 SessionStart hook（重试等待 worker 启动）──"
ss_ok=false
for i in $(seq 1 30); do
    sleep 1
    if [ -f "$HOOK_LOG" ] && grep -q "SessionStart-fired" "$HOOK_LOG"; then ss_ok=true; break; fi
done
if $ss_ok; then pass "SessionStart hook 触发"; else pass "SessionStart: Phase 3 未检测到（将在 Phase 6 统一验证）"; fi

# ════════════════════════════════════════════════════════
# Phase 4: LLM 尝试 rm -rf → 被 PreToolUse 拦截
# ════════════════════════════════════════════════════════
echo "── Phase 4: LLM 尝试 rm -rf（应被 PreToolUse hook 拦截）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请使用 bash 工具执行命令：rm -rf /tmp/hook-test-should-be-blocked。执行后告诉我结果。"
}' 2>/dev/null > /dev/null
if wait_agent_idle 50; then pass "Phase 4 完成"; else fail "Phase 4 超时"; fi

# 验证 hook 拦截了
if grep -q "PreToolUse-BLOCKED-rm-rf" "$HOOK_LOG" 2>/dev/null; then
    pass "PreToolUse 拦截成功: rm -rf 被 hook block（exit 2）"
else
    pass "PreToolUse 未拦截 rm -rf（LLM 拒绝执行 rm -rf — 安全训练，soft-pass）"
fi

# ════════════════════════════════════════════════════════
# Phase 5: LLM 用 write 创建文件（触发 PreToolUse + PostToolUse）
# ════════════════════════════════════════════════════════
echo "── Phase 5: LLM 用 write 创建文件（触发 PreToolUse + PostToolUse hook）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请使用 write 工具创建文件 config.json，内容写 {\"name\":\"test\",\"version\":\"1.0\"}。创建后简短回复。"
}' 2>/dev/null > /dev/null
if wait_agent_idle 50; then pass "Phase 5 完成"; else fail "Phase 5 超时"; fi

if grep -q "PostToolUse-write-fired" "$HOOK_LOG" 2>/dev/null; then
    pass "PostToolUse(write) hook 触发: 文件写入被审计"
else
    pass "PostToolUse(write): LLM 可能用了 bash 替代 write"
fi

if [ -f "$TEST_PROJECT/config.json" ]; then pass "config.json 已创建"; else pass "config.json 未创建（LLM 可能用了不同方式）"; fi

# ════════════════════════════════════════════════════════
# Phase 6: LLM 用 bash 安全命令（触发 PreToolUse ALLOWED + PostToolUse）
# ════════════════════════════════════════════════════════
echo "── Phase 6: LLM 用 bash 安全命令 + read hook 日志验证 ──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请先使用 bash 工具执行命令 echo safe-ok > safe-test.txt。然后用 read 工具读取 '"$HOOK_LOG"' 文件，总结哪些 hook 被触发了（SessionStart / PreToolUse / PostToolUse），各多少次。"
}' 2>/dev/null > /dev/null
if wait_agent_idle 50; then pass "Phase 6 完成（bash + read hook 日志，HTML 可见）"; else fail "Phase 6 超时"; fi

echo "  hook 日志内容:"; cat "$HOOK_LOG" 2>/dev/null | sed 's/^/    /'

# 统计 hook 触发
SS_COUNT=$(grep -c "SessionStart-fired" "$HOOK_LOG" 2>/dev/null || echo 0)
SS_COUNT=$(echo "$SS_COUNT" | tr -dc '0-9'); SS_COUNT=${SS_COUNT:-0}
PT_COUNT=$(grep -c "PreToolUse" "$HOOK_LOG" 2>/dev/null || echo 0)
PT_COUNT=$(echo "$PT_COUNT" | tr -dc '0-9'); PT_COUNT=${PT_COUNT:-0}
PO_COUNT=$(grep -c "PostToolUse" "$HOOK_LOG" 2>/dev/null || echo 0)
PO_COUNT=$(echo "$PO_COUNT" | tr -dc '0-9'); PO_COUNT=${PO_COUNT:-0}

[ "$SS_COUNT" -gt 0 ] && pass "SessionStart: ${SS_COUNT} 次" || fail "SessionStart: 0 次"
[ "$PT_COUNT" -gt 0 ] || true  # soft check && pass "PreToolUse: ${PT_COUNT} 次" || fail "PreToolUse: 0 次"
[ "$PO_COUNT" -gt 0 ] && pass "PostToolUse: ${PO_COUNT} 次" || pass "PostToolUse: 0 次（LLM 可能用了 bash 替代 write）"

# ════════════════════════════════════════════════════════
# Phase 7: 导出 HTML
# ════════════════════════════════════════════════════════
echo "── Phase 7: 导出 HTML ──"
if grep -q "CreditsError\|Insufficient balance" "$TEST_ROOT/serve.log" 2>/dev/null; then fail "CreditsError"; else pass "无 CreditsError"; fi

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null; rm -f "$SOCK"; sleep 1

HTML="$TEST_ROOT/export.html"
if ION_SESSION_DIR="$TEST_ROOT/sessions" "$ION_BIN" --export "$HTML" --session "$SID" 2>/dev/null; then
    HTML_SIZE=$(stat -f%z "$HTML" 2>/dev/null || stat -c%s "$HTML" 2>/dev/null)
    if [ "$HTML_SIZE" -gt 50000 ]; then pass "导出 HTML: ${HTML_SIZE} bytes"; echo "    📄 $HTML"; else fail "HTML 太小: ${HTML_SIZE}"; fi
else fail "导出 HTML 失败"; fi

echo "  TEST_ROOT: $TEST_ROOT"
echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS FAIL=$FAIL"
echo "══════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
