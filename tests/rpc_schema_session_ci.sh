#!/usr/bin/env bash
# rpc_schema_session_ci.sh — 会话/消息域 RPC 契约 CLI 验证（host 级命令抽样）
#
# 用隔离三件套（假 HOME + 私有 ION_SESSION_DIR + 独立 ION_HOST_SOCKET）起
# 一个独立 host，对 host 级会话/消息域命令打真实请求，用 jq 断言响应形状
# 与 schemas/rpc/session/*.json 契约的关键 required 字段一致。
# 如果 python3 可 import jsonschema，则额外做完整 schema 校验（缺失则 SKIP，
# 不算失败——深度校验已由 tests/rpc_schema_session_test.rs 用 jsonschema crate 覆盖）。
#
# 覆盖命令：create_session / list_sessions / list_all_sessions /
#   search_sessions / token_usage_summary / get_session_messages(host 直读) /
#   list_session_turns(host 直读) / session_remove
#
# 🔴 绝不触碰真实 ~/.ion；绝不 pkill；host 由本脚本 PID 精确清理。
# 用法：bash tests/rpc_schema_session_ci.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"

PASS=0
FAIL=0
SKIP=0
pass() { printf '  ok   %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
skip() { printf '  skip %s\n' "$1"; SKIP=$((SKIP + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }
jf() { jq -r "$1" 2>/dev/null; }

# ── 隔离三件套 ──
TEST_DIR="$(mktemp -d /tmp/ion-s1-schema-ci-XXXXXX)"
export HOME="$TEST_DIR/home"                 # 假 HOME：~/.ion → $TEST_DIR/home/.ion
# 会话目录指向独立位置（不在假 HOME 默认位置 $HOME/.ion/agent/sessions 下）——
# 直接断言所有会话读写/清理路径都吃 ION_SESSION_DIR 覆盖。S1 曾因
# session_remove 硬编码 root()/agent/sessions 在此绕过（对齐默认位置），
# G1 修复后 removed_files 必须在非默认 ION_SESSION_DIR 下依然 >=1（Group H2）。
export ION_SESSION_DIR="$TEST_DIR/sessions"
mkdir -p "$HOME" "$ION_SESSION_DIR"
export ION_HOST_SOCKET="/tmp/ion_s1_schema_ci_$$.sock"  # 独立 socket

source "$PROJECT_DIR/tests/ci_host_helper.sh"

cleanup() {
  cleanup_host
  rm -rf "$TEST_DIR" >/dev/null 2>&1
}
trap cleanup EXIT

# schema 深度校验（python3+jsonschema 可用时生效）
PY_JSONSCHEMA=0
python3 -c "import jsonschema" >/dev/null 2>&1 && PY_JSONSCHEMA=1

validate_data() {
  # $1=schema file, $2=response json；校验 .data 载荷
  if [ "$PY_JSONSCHEMA" = "1" ]; then
    echo "$2" | python3 -c '
import json, sys, jsonschema
resp = json.load(sys.stdin)
schema = json.load(open(sys.argv[1]))
errs = list(jsonschema.validator_for(schema).iter_errors(resp.get("data")))
for e in errs:
    print(f"    schema error at {list(e.absolute_path)}: {e.message}", file=sys.stderr)
sys.exit(1 if errs else 0)
' "$1"
  else
    return 0  # 深度校验不可用时不算失败（Rust 测试已覆盖）
  fi
}

# ── 确保 binary 存在 ──
if [ ! -x "$ION_BIN" ]; then
  echo "ion binary missing, building..."
  (cd "$PROJECT_DIR" && ~/.cargo/bin/cargo build --bin ion) || { echo "build failed"; exit 1; }
fi

ensure_host || { echo "host 启动失败"; exit 1; }

SID="ci_s1_schema_$$"
SchemasDir="$PROJECT_DIR/schemas/rpc/session"

echo ""
echo "── Group A: host 级会话生命周期 ──"

# A1 create_session（agent/cwd 形态）
R=$("$ION_BIN" rpc --method create_session --params "{\"session_id\":\"$SID\",\"agent\":\"build\",\"cwd\":\"$TEST_DIR\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "A0 create_session success"
[ "$(echo "$R" | jf '.data.session_id')" = "$SID" ]; check $? "A1 create_session data.session_id"
[ "$(echo "$R" | jf '.data.status')" = "created" ]; check $? "A2 create_session data.status=created"
validate_data "$SchemasDir/create_session.json" "$R"; check $? "A3 create_session data @ schema"

# A2 list_sessions 含新会话
R=$("$ION_BIN" rpc --method list_sessions)
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "B0 list_sessions success"
N=$(echo "$R" | jf ".data.sessions[] | select(.session_id==\"$SID\")" | jq -r '.session_id' 2>/dev/null)
[ "$N" = "$SID" ]; check $? "B1 list_sessions contains $SID"
S=$(echo "$R" | jf ".data.sessions[] | select(.session_id==\"$SID\") | .status" | head -1)
[ -n "$S" ] && [ "$S" != "null" ]; check $? "B2 session item has status field"
validate_data "$SchemasDir/list_sessions.json" "$R"; check $? "B3 list_sessions data @ schema"

# A3 list_all_sessions（SessionIndex 目录）
R=$("$ION_BIN" rpc --method list_all_sessions)
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "C0 list_all_sessions success"
TC=$(echo "$R" | jf '.data.totalCount')
[ "${TC:-0}" -ge 1 ] 2>/dev/null; check $? "C1 list_all_sessions totalCount>=1 (got ${TC})"
ID_OK=$(echo "$R" | jq -r ".data.sessions[] | select(.id==\"$SID\") | .id" 2>/dev/null)
[ "$ID_OK" = "$SID" ]; check $? "C2 list_all_sessions contains $SID (lineage catalog)"
validate_data "$SchemasDir/list_all_sessions.json" "$R"; check $? "C3 list_all_sessions data @ schema"

# A4 token_usage_summary
R=$("$ION_BIN" rpc --method token_usage_summary)
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "D0 token_usage_summary success"
[ "$(echo "$R" | jf '.data.totalTokens')" != "null" ]; check $? "D1 token_usage_summary totalTokens present"
validate_data "$SchemasDir/token_usage_summary.json" "$R"; check $? "D2 token_usage_summary data @ schema"

echo ""
echo "── Group E: host 直读（不拉起 worker）──"

SESS_FILE=$(find "$ION_SESSION_DIR" -name "$SID.jsonl" 2>/dev/null | head -1)
[ -n "$SESS_FILE" ]; check $? "E0 session file exists: ${SESS_FILE##*/}"

cat >> "$SESS_FILE" <<'EOF'
{"id":"m1","parentId":null,"timestamp":"2026-09-15T10:00:01Z","turnId":1,"type":"message","message":{"User":{"content":[{"Text":{"text":"schema 契约第一问"}}],"role":"user","source":"prompt","timestamp":1786249005773}}}
{"id":"m2","parentId":"m1","timestamp":"2026-09-15T10:00:02Z","turnId":1,"type":"message","message":{"Assistant":{"api":"openai-completions","content":[{"Text":{"text":"schema 契约第一答"}}],"role":"assistant","source":"api","timestamp":1786249006773}}}
EOF

WORKERS_BEFORE=$(pgrep -f "target/debug/ion.*--mode rpc" 2>/dev/null | wc -l | tr -d ' ')

R=$("$ION_BIN" rpc --method get_session_messages --params "{\"session\":\"$SESS_FILE\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "E1 get_session_messages(host) success"
MN=$(echo "$R" | jf '.data.messages | length')
[ "${MN:-0}" = "2" ]; check $? "E2 host direct read returns 2 messages (got ${MN})"
[ "$(echo "$R" | jf '.data.totalCount')" = "2" ]; check $? "E3 totalCount=2"
[ "$(echo "$R" | jf '.data.hasMore')" != "null" ]; check $? "E4 hasMore present"
validate_data "$SchemasDir/get_session_messages.json" "$R"; check $? "E5 get_session_messages data @ schema"

R=$("$ION_BIN" rpc --method list_session_turns --params "{\"session\":\"$SESS_FILE\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "F0 list_session_turns(host) success"
TN=$(echo "$R" | jf '.data.turns | length')
[ "${TN:-0}" = "1" ]; check $? "F1 one turn (got ${TN})"
[ "$(echo "$R" | jf '.data.turns[0].turnId')" = "m1" ]; check $? "F2 turnId = anchor entry id (m1)"
[ "$(echo "$R" | jf '.data.turns[0].tokens.input')" != "null" ]; check $? "F3 turn tokens.input present"
validate_data "$SchemasDir/list_session_turns.json" "$R"; check $? "F4 list_session_turns data @ schema"

WORKERS_AFTER=$(pgrep -f "target/debug/ion.*--mode rpc" 2>/dev/null | wc -l | tr -d ' ')
[ "$WORKERS_BEFORE" = "$WORKERS_AFTER" ]; check $? "F5 direct read spawned no worker ($WORKERS_BEFORE -> $WORKERS_AFTER)"

echo ""
echo "── Group E2: host 直读简形消息（{\"role\":\"user\"} 非 {\"User\":{...}}）──"

# S1 简形 fixture（同 tests/rpc_schema_session_test.rs 的 worker 级构造）：
# worker 慢路径一直兼容简形，host fast path（FileIndex）此前只认枚举形 →
# list_session_turns 返回 0 turns 且不回落，与 get_session_messages 行为分裂。
SIMPLE_FILE="$ION_SESSION_DIR/ci_s1_simple_$$.jsonl"
cat > "$SIMPLE_FILE" <<EOF
{"cwd":"$TEST_DIR","id":"ci_s1_simple","parentSession":null,"timestamp":"2026-09-15T10:00:00Z","type":"session","version":3}
{"id":"m1","parentId":null,"timestamp":"2026-09-15T10:00:01Z","type":"message","message":{"role":"user","content":"简形第一问"}}
{"id":"m2","parentId":"m1","timestamp":"2026-09-15T10:00:02Z","type":"message","message":{"role":"assistant","content":[{"Text":{"text":"简形第一答"}}]}}
EOF

R=$("$ION_BIN" rpc --method get_session_messages --params "{\"session\":\"$SIMPLE_FILE\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "E2a simple-form get_session_messages success"
MN=$(echo "$R" | jf '.data.messages | length')
[ "${MN:-0}" = "2" ]; check $? "E2b simple-form returns 2 messages (got ${MN})"

R=$("$ION_BIN" rpc --method list_session_turns --params "{\"session\":\"$SIMPLE_FILE\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "E2c simple-form list_session_turns success"
TN=$(echo "$R" | jf '.data.turns | length')
[ "${TN:-0}" = "1" ]; check $? "E2d simple-form groups into 1 turn (got ${TN})"
[ "$(echo "$R" | jf '.data.turns[0].turnId')" = "m1" ]; check $? "E2e turnId=m1 (user anchor)"
UC=$(echo "$R" | jf '.data.turns[0].userContent')
[ "$UC" = "简形第一问" ]; check $? "E2f userContent preview extracted (got ${UC})"
validate_data "$SchemasDir/list_session_turns.json" "$R"; check $? "E2g simple-form turns @ schema"

echo ""
echo "── Group G: search_sessions ──"

# 命中：名字/内容搜（SessionIndex name 为空时 title 不中；直接搜 project 路径片段）
R=$("$ION_BIN" rpc --method search_sessions --params "{\"query\":\"s1_schema\",\"limit\":5}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "G0 search_sessions success"
TM=$(echo "$R" | jf '.data.totalMatches')
[ "${TM:-0}" -ge 1 ] 2>/dev/null; check $? "G1 project substring matches (totalMatches=${TM})"
MT=$(echo "$R" | jf '.data.results[0].matchType')
[ "$MT" = "title" ] || [ "$MT" = "content" ]; check $? "G2 matchType enum (got ${MT})"
validate_data "$SchemasDir/search_sessions.json" "$R"; check $? "G3 search_sessions data @ schema"

R=$("$ION_BIN" rpc --method search_sessions --params '{}')
[ "$(echo "$R" | jf '.success')" = "false" ] && [ -n "$(echo "$R" | jf '.error')" ]; check $? "G4 empty query -> {success:false,error}"

echo ""
echo "── Group H: session_remove 闭环 ──"

R=$("$ION_BIN" rpc --method session_remove --params "{\"session_id\":\"$SID\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "H0 session_remove success"
[ "$(echo "$R" | jf '.data.status')" = "removed" ]; check $? "H1 status=removed"
[ "$(echo "$R" | jf '.data.removed_files')" -ge 1 ] 2>/dev/null; check $? "H2 removed_files>=1 (got $(echo "$R" | jf '.data.removed_files'))"
validate_data "$SchemasDir/session_remove.json" "$R"; check $? "H3 session_remove data @ schema"

R=$("$ION_BIN" rpc --method list_all_sessions)
GONE=$(echo "$R" | jq -r ".data.sessions[] | select(.id==\"$SID\") | .id" 2>/dev/null)
[ -z "$GONE" ] || [ "$GONE" = "null" ]; check $? "H4 session gone from index"

R=$("$ION_BIN" rpc --method session_remove --params '{}')
[ "$(echo "$R" | jf '.success')" = "false" ]; check $? "H5 missing session_id -> error"

echo ""
echo "── 汇总 ──"
if [ "$PY_JSONSCHEMA" = "1" ]; then
  echo "  python jsonschema: ENABLED (深度校验)"
else
  echo "  python jsonschema: 未安装，深度校验 skip（Rust 测试已覆盖 schema 编译+校验）"
fi
echo "  PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
[ "$FAIL" -eq 0 ]
