#!/usr/bin/env bash
# host_read CI — Host 级会话直读（不拉起 worker 读消息内容）
#
# 验证三件事：
#   1. 新命令 get_session_messages / list_session_turns：host 纯磁盘读 JSONL，
#      响应形状与 worker 级 get_messages / list_turns 一致
#   2. 只读拦截：旧命令名 get_messages / list_turns 带 session 时也走直读，
#      不 auto-create worker（读历史会话不拉进程）
#   3. 错误语义：不存在的 session 报错（不静默创建）；缺 session 参数报错
#
# 场景：ion serve（场景 3）+ socket RPC。会话用 path 方式传（load_session_entries
# 支持文件路径直读），fixture 是合成 JSONL，全封闭不依赖真实会话/LLM。
#
# 用法：bash tests/host_read_ci.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"

source "$(dirname "$0")/ci_host_helper.sh"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }

# jq 取字段的辅助（失败时输出空，让断言走 fail 分支）
jf() { jq -r "$1" 2>/dev/null; }

TEST_DIR="$(mktemp -d /tmp/ion-host-read-ci-XXXXXX)"
trap 'cleanup_host; rm -rf "$TEST_DIR"' EXIT

# ── fixture：合成会话 JSONL（2 轮 user/assistant，结构与真实会话一致）──
SESS_FILE="$TEST_DIR/ci_session.jsonl"
cat > "$SESS_FILE" <<'EOF'
{"cwd":"/tmp/ion-host-read-ci","id":"ci_hostread_sess","parentSession":null,"timestamp":"2026-08-24T00:00:00.000Z","type":"session","version":3}
{"id":"u1","message":{"User":{"content":[{"Text":{"text":"first question"}}],"role":"user","source":"prompt","timestamp":1786249005773}},"parentId":"ci_hostread_sess","timestamp":"2026-08-24T00:00:01.000Z","type":"message"}
{"id":"a1","message":{"Assistant":{"api":"openai-completions","content":[{"Text":{"text":"first answer"}}],"role":"assistant","source":"api","timestamp":1786249006773}},"parentId":"u1","timestamp":"2026-08-24T00:00:02.000Z","type":"message"}
{"id":"u2","message":{"User":{"content":[{"Text":{"text":"second question"}}],"role":"user","source":"prompt","timestamp":1786249007773}},"parentId":"a1","timestamp":"2026-08-24T00:00:03.000Z","type":"message"}
{"id":"a2","message":{"Assistant":{"api":"openai-completions","content":[{"Text":{"text":"second answer"}}],"role":"assistant","source":"api","timestamp":1786249008773}},"parentId":"u2","timestamp":"2026-08-24T00:00:04.000Z","type":"message"}
EOF

ensure_host || { echo "host 启动失败"; exit 1; }

# 读前 worker 进程数（断言读操作零新增 worker）
worker_count() { pgrep -f "ion.*--mode rpc" 2>/dev/null | wc -l | tr -d ' '; }
WORKERS_BEFORE=$(worker_count)

echo ""
echo "── Group A: host 级直读命令 ──"

# A1 get_session_messages 返回全部消息
R=$("$ION_BIN" rpc --method get_session_messages --params "{\"session\":\"$SESS_FILE\"}")
N=$(echo "$R" | jf '.data.messages | length')
TC=$(echo "$R" | jf '.data.totalCount')
[ "$N" = "4" ] && [ "$TC" = "4" ]; check $? "A1 get_session_messages 返回 4 条消息 (got $N/$TC)"

# A2 响应形状与 worker 级一致（messages/hasMore/totalCount/nextCursor/view/compactionPoints）
SHAPE=$(echo "$R" | jq -r '.data | keys_unsorted | sort | join(",")' 2>/dev/null)
[ "$SHAPE" = "compactionPoints,hasMore,messages,nextCursor,totalCount,view" ]
check $? "A2 响应字段与 worker 级 get_messages 一致"

# A3 limit 分页
R=$("$ION_BIN" rpc --method get_session_messages --params "{\"session\":\"$SESS_FILE\",\"limit\":2}")
N=$(echo "$R" | jf '.data.messages | length'); HM=$(echo "$R" | jf '.data.hasMore')
[ "$N" = "2" ] && [ "$HM" = "true" ]; check $? "A3 limit=2 返回 2 条 + hasMore=true (got $N/$HM)"

# A4 list_session_turns 轮次识别
R=$("$ION_BIN" rpc --method list_session_turns --params "{\"session\":\"$SESS_FILE\"}")
N=$(echo "$R" | jf '.data.turns | length')
FIRST=$(echo "$R" | jf '.data.turns[0].userContent')
[ "$N" = "2" ] && [ "$FIRST" = "first question" ]; check $? "A4 list_session_turns 识别 2 轮 (got $N)"

# A5 延迟断言（直读应 < 500ms；宽松上界防回归成慢路径）
T0=$(python3 -c 'import time; print(time.time())')
"$ION_BIN" rpc --method get_session_messages --params "{\"session\":\"$SESS_FILE\"}" > /dev/null
DT=$(python3 -c "import time; print((time.time() - $T0) * 1000)")
python3 -c "exit(0 if $DT < 500 else 1)"; check $? "A5 直读延迟 < 500ms (${DT%.*}ms)"

echo ""
echo "── Group B: 只读拦截（旧命令名不拉 worker）──"

# B1 旧命令名 get_messages（session 在 params）→ 直读成功
R=$("$ION_BIN" rpc --method get_messages --params "{\"session\":\"$SESS_FILE\",\"limit\":3}")
OK=$(echo "$R" | jf '.success'); N=$(echo "$R" | jf '.data.messages | length')
[ "$OK" = "true" ] && [ "$N" = "3" ]; check $? "B1 get_messages(params session) 直读 3 条 (ok=$OK n=$N)"

# B2 旧命令名 list_turns（session 顶层，ion rpc --session 路径）→ 直读成功
R=$("$ION_BIN" rpc --session "$SESS_FILE" --method list_turns)
OK=$(echo "$R" | jf '.success'); N=$(echo "$R" | jf '.data.turns | length')
[ "$OK" = "true" ] && [ "$N" = "2" ]; check $? "B2 list_turns(顶层 session) 直读 2 轮 (ok=$OK n=$N)"

# B3 全部读操作完成后 worker 数零增长
sleep 1
AFTER=$(worker_count)
[ "$AFTER" = "$WORKERS_BEFORE" ]; check $? "B3 读全程零新增 worker (before=$WORKERS_BEFORE after=$AFTER)"

echo ""
echo "── Group C: 错误语义 ──"

# C1 不存在的 session 报错（不静默 auto-create）
R=$("$ION_BIN" rpc --method get_session_messages --params '{"session":"sess_not_exist_ci"}')
OK=$(echo "$R" | jf '.success'); ERR=$(echo "$R" | jf '.error')
[ "$OK" = "false" ] && echo "$ERR" | grep -q "not found"; check $? "C1 不存在的 session 报 not found (ok=$OK)"

# C2 缺 session 参数报错
R=$("$ION_BIN" rpc --method get_session_messages --params '{}')
OK=$(echo "$R" | jf '.success'); ERR=$(echo "$R" | jf '.error')
[ "$OK" = "false" ] && echo "$ERR" | grep -qi "session"; check $? "C2 缺 session 参数报错 (err=$ERR)"

echo ""
echo "═══ host_read CI: $PASS passed / $FAIL failed ═══"
[ "$FAIL" -eq 0 ]
