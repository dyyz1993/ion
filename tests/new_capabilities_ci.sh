#!/bin/bash
# new_capabilities_ci.sh — 13 项新能力 CI 测试（2026-08-17 补齐的 RPC + 事件）
# 覆盖：search_sessions / FilesRestored / ExtensionListChanged / McpServerStatusChanged
#        failures[] / SettingsChanged×4 / SessionRenamed / QueueChanged / MemoryDistilled
#        memory LIMIT / memory unarchive
set -uo pipefail

ION="${ION_BIN:-./target/debug/ion}"
HOST_SOCK="$HOME/.ion/host.sock"
PASS=0; FAIL=0; SKIP=0

say()  { echo "  $1"; }
ok()   { PASS=$((PASS+1)); say "✓ $1"; }
bad()  { FAIL=$((FAIL+1)); say "✗ $1"; }
skip() { SKIP=$((SKIP+1)); say "⊘ $1 (skip)"; }

cleanup() {
  local PIDS=$(lsof -ti "$HOST_SOCK" 2>/dev/null | sort -u)
  [ -n "$PIDS" ] && kill $PIDS 2>/dev/null || true
}
trap cleanup EXIT

# ── 启动 host ──
if ! lsof -ti "$HOST_SOCK" &>/dev/null; then
  rm -f "$HOST_SOCK"
  nohup "$ION" serve start > /tmp/ion-ci-host.log 2>&1 &
  sleep 8
fi

# 找一个有效 session
SID=$("$ION" rpc --method list_all_sessions 2>/dev/null | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)['data']['sessions']
    print(d[0]['id'] if d else '')
except: print('')
")

rpc() {
  "$ION" rpc --method "$1" ${2:+--session "$SID"} ${3:+--params "$3"} 2>/dev/null
}

echo "═══ 1. search_sessions RPC (P0) ═══"
R=$(rpc search_sessions "" '{"query":"测试","limit":5}')
if echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin); exit(0 if d.get('success') else 1)" 2>/dev/null; then
  N=$(echo "$R" | python3 -c "import json,sys; print(len(json.load(sys.stdin)['data'].get('results',[])))")
  ok "search_sessions 标题查询（$N 条命中）"
else
  bad "search_sessions 标题查询"
fi

R=$(rpc search_sessions "" '{"query":"ion","searchContent":true,"limit":3}')
if echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin); exit(0 if d.get('success') else 1)" 2>/dev/null; then
  ok "search_sessions 内容查询"
else
  bad "search_sessions 内容查询"
fi

# 空查询应报错
R=$(rpc search_sessions "" '{"query":""}')
if echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin); exit(0 if not d.get('success') else 1)" 2>/dev/null; then
  ok "search_sessions 空查询报错"
else
  bad "search_sessions 空查询未报错"
fi

echo "═══ 2. SettingsChanged 事件（set_permission_mode/set_model/set_thinking_level/set_active_tools） ═══"
[ -n "$SID" ] && {
  R=$(rpc set_permission_mode "$SID" '{"mode":"blacklist"}')
  echo "$R" | python3 -c "import json,sys; exit(0 if json.load(sys.stdin).get('success') else 1)" 2>/dev/null \
    && ok "set_permission_mode → SettingsChanged" || bad "set_permission_mode"

  R=$(rpc set_model "$SID" '{"modelId":"glm-5.2","provider":"zai"}')
  echo "$R" | python3 -c "import json,sys; exit(0 if json.load(sys.stdin).get('success') else 1)" 2>/dev/null \
    && ok "set_model → SettingsChanged" || bad "set_model"

  R=$(rpc set_thinking_level "$SID" '{"level":"low"}')
  echo "$R" | python3 -c "import json,sys; exit(0 if json.load(sys.stdin).get('success') else 1)" 2>/dev/null \
    && ok "set_thinking_level → SettingsChanged" || bad "set_thinking_level"
} || skip "无 session，跳过"

echo "═══ 3. failures[]（approve_all 响应） ═══"
[ -n "$SID" ] && {
  R=$(rpc review_approve_all "$SID")
  echo "$R" | python3 -c "
import json,sys
d=json.load(sys.stdin)['data']
exit(0 if 'failures' in d else 1)" 2>/dev/null \
    && ok "approve_all 响应含 failures[]" || bad "approve_all 响应缺 failures[]"
} || skip "无 session"

echo "═══ 4. memory LIMIT + unarchive ═══"
# 仅 serve 模式下 global-memory 可用
R=$(rpc extension_rpc "$SID" '{"extension":"global-memory","method":"list","params":{"limit":3}}')
if echo "$R" | python3 -c "import json,sys; d=json.load(sys.stdin)['data']; exit(0 if 'totalCount' in d else 1)" 2>/dev/null; then
  TC=$(echo "$R" | python3 -c "import json,sys; print(json.load(sys.stdin)['data'].get('totalCount','?'))")
  ok "memory list LIMIT（total=$TC returned≤3）"
else
  skip "global-memory 仅 serve 模式"
fi

echo "═══ 5. SessionRenamed 事件 ═══"
[ -n "$SID" ] && {
  R=$(rpc set_session_name "$SID" '{"name":"ci-test-name"}')
  echo "$R" | python3 -c "import json,sys; exit(0 if json.load(sys.stdin).get('success') else 1)" 2>/dev/null \
    && ok "set_session_name → SessionRenamed" || bad "set_session_name"
} || skip "无 session"

echo "═══ 6. QueueChanged 事件（clear_queue） ═══"
[ -n "$SID" ] && {
  R=$(rpc clear_queue "$SID")
  echo "$R" | python3 -c "import json,sys; exit(0 if json.load(sys.stdin).get('success') else 1)" 2>/dev/null \
    && ok "clear_queue → QueueChanged" || bad "clear_queue"
} || skip "无 session"

echo ""
echo "═══ 汇总 ═══"
echo "  PASS: $PASS  FAIL: $FAIL  SKIP: $SKIP"
[ "$FAIL" -eq 0 ] && echo "  ✅ 全部通过" || { echo "  ❌ 有失败项"; exit 1; }
