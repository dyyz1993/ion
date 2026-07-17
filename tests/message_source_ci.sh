#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：Message Source Tag 验证
#
# 验证 UserMessage.source 字段正确标记 + list_turns/get_messages 返回 + jsonl 落盘
# 对齐 docs/testing/MESSAGE_SOURCE_CLI_TEST.md
#
# 注：steer/followUp 需要 agent 忙时才入队，faux 回复太快难模拟"忙时"，
# 本脚本验证 source 链路通畅（prompt 正确 + 字段存在 + jsonl 落盘）。
# steer/followUp 的标记代码在 ion_worker.rs 已实现，逻辑跟 prompt 一致。
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"

echo "════════════════════════════════════════════════════"
echo "  ION Message Source CI Test — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null && pass "build" || { fail "build"; exit 1; }

# 清理残留
pkill -f "target/debug/ion serve" 2>/dev/null || true
rm -f "$HOME/.ion/host.sock"
sleep 1

# 起 host
ION_FAUX_REPLY="source test" "$ION_BIN" serve >/tmp/ion_ms_host.log 2>&1 &
HOST_PID=$!
sleep 5

D=$(mktemp -d)
SID=$(timeout 15 "$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$D\"}" 2>&1 \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

# ──────────────────────────────────────────────────────────
# Group A：prompt source 标记
# ──────────────────────────────────────────────────────────
echo ""
echo "Group A: prompt source"

if [ -z "$SID" ]; then
    fail "A0: create_session 失败"
else
    pass "A0: create_session ($SID)"
    timeout 30 "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"你好"}' >/dev/null 2>&1 || true
    sleep 3

    # A1: list_turns 返回 source
    LT=$(timeout 15 "$ION_BIN" rpc --session "$SID" --method list_turns 2>&1)
    SRC=$(echo "$LT" | python3 -c "
import json,sys
d=json.load(sys.stdin)
ts=d['data']['turns']
print(ts[0].get('source','') if ts else 'EMPTY')" 2>/dev/null)
    if [ "$SRC" = "prompt" ]; then
        pass "A1: list_turns source = prompt"
    else
        fail "A1: source = '$SRC'（期望 prompt）"
    fi

    # A2: get_messages User.source
    GM=$(timeout 15 "$ION_BIN" rpc --session "$SID" --method get_messages --params '{"view":"full"}' 2>&1)
    GM_SRC=$(echo "$GM" | python3 -c "
import json,sys
d=json.load(sys.stdin)
ms=d['data']['messages']
u=[m for m in ms if m.get('message',{}).get('User')]
print(u[0]['message']['User'].get('source','') if u else 'EMPTY')" 2>/dev/null)
    if [ "$GM_SRC" = "prompt" ]; then
        pass "A2: get_messages User.source = prompt"
    else
        fail "A2: User.source = '$GM_SRC'（期望 prompt）"
    fi
fi

# ──────────────────────────────────────────────────────────
# Group B：steer/followUp RPC 可调用（标记代码已实现，agent 空闲时退化为 prompt）
# ──────────────────────────────────────────────────────────
echo ""
echo "Group B: steer/followUp RPC"

# B1: steer RPC 不报错
STEER_OK=$(timeout 15 "$ION_BIN" rpc --session "$SID" --method steer --params '{"text":"插队消息"}' 2>&1 \
  | python3 -c "import json,sys;d=json.load(sys.stdin);print('ok' if d.get('success') else 'fail')" 2>/dev/null)
if [ "$STEER_OK" = "ok" ]; then
    pass "B1: steer RPC 成功"
else
    fail "B1: steer RPC 失败"
fi

# B2: follow_up RPC 不报错
FU_OK=$(timeout 15 "$ION_BIN" rpc --session "$SID" --method follow_up --params '{"text":"追加消息"}' 2>&1 \
  | python3 -c "import json,sys;d=json.load(sys.stdin);print('ok' if d.get('success') else 'fail')" 2>/dev/null)
if [ "$FU_OK" = "ok" ]; then
    pass "B2: follow_up RPC 成功"
else
    fail "B2: follow_up RPC 失败"
fi

# ──────────────────────────────────────────────────────────
# Group C：jsonl 落盘
# ──────────────────────────────────────────────────────────
echo ""
echo "Group C: jsonl 落盘"

# 找这次测试的 session jsonl（可能在任意 hash 目录下）
SESS_FILE=""
for f in $(find "$HOME/.ion/agent/sessions" -name "session.jsonl" -mmin -2 2>/dev/null); do
    if grep -q "source test\|你好" "$f" 2>/dev/null; then
        SESS_FILE="$f"
        break
    fi
done

if [ -n "$SESS_FILE" ]; then
    # C1: message 含 source
    if grep -q '"source":"prompt"' "$SESS_FILE" 2>/dev/null; then
        pass "C1: jsonl message 含 source=prompt"
    else
        fail "C1: jsonl 无 source=prompt"
    fi

    # C2: 不应该有 source:null
    if grep -q '"source":null' "$SESS_FILE" 2>/dev/null; then
        fail "C2: jsonl 含 source=null（不应出现）"
    else
        pass "C2: jsonl 无 source=null"
    fi

    # C3: User message 结构含 source 字段
    C3=$(grep '"type":"message"' "$SESS_FILE" 2>/dev/null | head -1 | python3 -c "
import json,sys
d=json.loads(sys.stdin.read())
u=d.get('message',{}).get('User',{})
print('has_source' if 'source' in u else 'no_source')" 2>/dev/null)
    if [ "$C3" = "has_source" ]; then
        pass "C3: User message 结构含 source 字段"
    else
        fail "C3: User message 无 source 字段"
    fi
else
    fail "C1-C3: 找不到 session.jsonl"
fi

# ──────────────────────────────────────────────────────────
# 清理
# ──────────────────────────────────────────────────────────
kill "$HOST_PID" 2>/dev/null || true
wait "$HOST_PID" 2>/dev/null || true
pkill -f "target/debug/ion serve" 2>/dev/null || true
rm -f "$HOME/.ion/host.sock"
rm -rf "$D"

echo ""
echo "════════════════════════════════════════"
echo "  PASS=$PASS  FAIL=$FAIL"
echo "════════════════════════════════════════"
exit $FAIL
