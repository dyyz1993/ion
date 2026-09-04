#!/usr/bin/env bash
# INPUT_ORIGIN CLI verification（docs/design/INPUT_ORIGIN.md）。
#
# 证明消息来源标识端到端可观察：
#   A. prompt(params.origin=monitor) → agent 应答 + JSONL 落 custom(input_origin) 条目
#   B. prompt 缺省（无 origin）→ 不落 input_origin 条目（user 是常态不留痕）
#   C. origin 非法值回落 user（宽容，不拒绝消息）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }
jf() { jq -r "$1" 2>/dev/null; }

source "$(dirname "$0")/ci_host_helper.sh"

TEST_DIR="$(mktemp -d /tmp/ion-origin-ci-XXXXXX)"
SID="sess_origin_ci_$$"
trap 'cleanup_host; rm -rf "$TEST_DIR"' EXIT

printf '%s\n' '════════════════════════════════════════════════════'
printf '%s\n' '  INPUT_ORIGIN CLI（prompt 来源标识）'
printf '%s\n' '════════════════════════════════════════════════════'

if [ ! -x "$ION_BIN" ]; then
    fail "ion binary 存在（先 cargo build --bin ion）"
    exit 1
fi
pass "ion binary 存在"

ensure_host || { echo "host 启动失败"; exit 1; }

# 会话（cwd 用隔离临时目录；faux 环境由 ci_host_helper 的 host 提供）
R=$("$ION_BIN" rpc --method create_session \
    --params "{\"session_id\":\"$SID\",\"cwd\":\"$TEST_DIR\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "A0 create_session"

SESS_FILE=$(find ~/.ion/agent/sessions -name "$SID.jsonl" 2>/dev/null | head -1)
[ -n "$SESS_FILE" ]; check $? "A0b session file exists"

count_origin_entries() {
    grep -c '"customType": *"input_origin"' "$SESS_FILE" 2>/dev/null || \
    grep -c '"customType":"input_origin"' "$SESS_FILE" 2>/dev/null || echo 0
}

echo ""
echo "── Group A: origin=monitor 落盘 + 正常应答 ──"

"$ION_BIN" rpc --session "$SID" --method prompt \
    --params '{"text":"monitor round question","origin":"monitor"}' >/dev/null 2>&1
# 等 faux 应答完成（轮询 agent_end 事件文件，最多 20s）
for i in $(seq 1 20); do
    sleep 1
    if [ "$(count_origin_entries)" -ge 1 ]; then break; fi
done
[ "$(count_origin_entries)" -ge 1 ]; check $? "A1 origin=monitor 落 custom(input_origin) 条目"

ORIGIN_VAL=$(grep -o '"customType": *"input_origin"[^}]*}' "$SESS_FILE" 2>/dev/null | \
    grep -o '"origin": *"[a-z]*"' | head -1 | grep -o '[a-z]*"$' | tr -d '"')
[ "$ORIGIN_VAL" = "monitor" ]; check $? "A2 条目内 origin=monitor (got $ORIGIN_VAL)"

ANSWER=""
for i in $(seq 1 15); do
    R=$("$ION_BIN" rpc --session "$SID" --method get_messages --params '{"limit":10}')
    ANSWER=$(echo "$R" | jq -r '[.data.messages[].message.Assistant.content[]? | select(.Text) | .Text.text] | first // empty' 2>/dev/null)
    [ -n "$ANSWER" ] && break
    sleep 1
done
[ -n "$ANSWER" ] && [ "$ANSWER" != "null" ]; check $? "A3 agent 正常应答（origin 不影响主流程）"

echo ""
echo "── Group B: 缺省 origin 不落盘 ──"

BEFORE_N=$(count_origin_entries)
"$ION_BIN" rpc --session "$SID" --method prompt \
    --params '{"text":"plain user question"}' >/dev/null 2>&1
sleep 4
AFTER_N=$(count_origin_entries)
[ "$BEFORE_N" = "$AFTER_N" ]; check $? "B1 user default does not log origin ($BEFORE_N->$AFTER_N)"

echo ""
echo "── Group C: 非法 origin 回落 user ──"

BEFORE_N=$(count_origin_entries)
"$ION_BIN" rpc --session "$SID" --method prompt \
    --params '{"text":"bad origin question","origin":"hacker"}' >/dev/null 2>&1
sleep 4
AFTER_N=$(count_origin_entries)
[ "$BEFORE_N" = "$AFTER_N" ]; check $? "C1 invalid origin falls back to user ($BEFORE_N->$AFTER_N)"

echo ""
printf '%s\n' '════════════════════════════════════════════════════'
printf '  结果: %d ok / %d FAIL\n' "$PASS" "$FAIL"
printf '%s\n' '════════════════════════════════════════════════════'
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
