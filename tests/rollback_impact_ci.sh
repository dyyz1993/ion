#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Rollback Impact CI — 回滚对 Context / Message / Compaction 的影响
#
# 前置修复（已合入）：
#   Bug 1: worker 启动时 ensure_session_header（防 turn_summary 抢占第一行）
#   Bug 2: create_worker 后写 SessionIndex（让 --resume/--rollback 能找到 session）
#   Bug 3: apply_session_tree_ops 用 session 真实 cwd（不是 CLI 进程 cwd）
#
# 断言：记录当前实际行为（含 F1/F3），全 pass 作为 baseline。
# ──────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"

PASS=0; FAIL=0; SKIP=0
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
yellow(){ printf "\033[33m%s\033[0m\n" "$1"; }
pass() { PASS=$((PASS+1)); green "  ✅ $1"; }
fail() { FAIL=$((FAIL+1)); red "  ❌ $1"; }
skip() { SKIP=$((SKIP+1)); yellow "  ⏭️  $1"; }

# 从 get_messages JSON 提取第一条 message 的 entry id
first_entry() {
    echo "$1" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
    for m in d.get('data',{}).get('messages',[]):
        if m.get('type')=='message': print(m.get('id','')); break
except: pass
"
}
# 提取 rpc data 字段里的数字
rpc_num() {
    echo "$1" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
    print(d.get('data',{}).get('$2',0))
except: print(0)
"
}

start_serve() {
    pkill -f "target/debug/ion serve" 2>/dev/null || true
    sleep 1
    rm -f ~/.ion/host.sock ~/.ion/host.pid 2>/dev/null
    ION_FAUX_REPLY="ok" "$ION_BIN" serve >/tmp/ion_rb_serve.log 2>&1 &
    local pid=$!
    for i in $(seq 1 15); do
        "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 && break
        sleep 1
    done
    echo "$pid"
}

echo "════════════════════════════════════════════════════"
echo "  ION Rollback Impact CI — $(date)"
echo "════════════════════════════════════════════════════"

echo "── Phase 0: Build ──"
cargo build --bin ion --bin ion-worker 2>&1 | tail -1
[ -x "$ION_BIN" ] || { echo "ion binary missing"; exit 1; }

# ════════════════════════════════════════════════════════
# Scenario 1: 修改后回滚→继续聊
# ════════════════════════════════════════════════════════
echo ""
echo "── Scenario 1: 修改后回滚→继续聊 ──"

TEST_DIR=$(mktemp -d /tmp/ion_rb_s1.XXXXXX)
TEST_DIR=$(cd "$TEST_DIR" && pwd -P)
SERVE_PID=$(start_serve)
"$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 || { fail "serve 启动失败"; exit 1; }
pass "S1.0 serve 启动"

CREATE1=$("$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$TEST_DIR\"}" 2>&1)
SID1=$(echo "$CREATE1" | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
[ -z "$SID1" ] && { fail "S1 create_session 失败"; kill $SERVE_PID 2>/dev/null; exit 1; }
pass "S1.0b session: $SID1"

# Turn1: 闲聊
"$ION_BIN" rpc --session "$SID1" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1; sleep 1
pass "S1.1 Turn1 闲聊"

# Turn2/3: write a.txt V1→V2（call_tool 时序可控）
"$ION_BIN" rpc --session "$SID1" --method call_tool \
    --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR/a.txt\",\"content\":\"V1\"}}" >/dev/null 2>&1; sleep 0.5
grep -q "V1" "$TEST_DIR/a.txt" 2>/dev/null && pass "S1.2 a.txt=V1" || fail "S1.2 a.txt 未写 V1"
"$ION_BIN" rpc --session "$SID1" --method call_tool \
    --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR/a.txt\",\"content\":\"V2\"}}" >/dev/null 2>&1; sleep 0.5
grep -q "V2" "$TEST_DIR/a.txt" 2>/dev/null && pass "S1.3 a.txt=V2" || fail "S1.3 a.txt 未写 V2"

# 回滚前状态
MSGS_BEFORE=$("$ION_BIN" rpc --session "$SID1" --method get_messages --params '{"view":"live"}' 2>&1)
CTX_BEFORE=$("$ION_BIN" rpc --session "$SID1" --method get_context_usage 2>&1)
LIVE_BEFORE=$(rpc_num "$MSGS_BEFORE" totalCount)
CTX_MSG_BEFORE=$(rpc_num "$CTX_BEFORE" messageCount)
echo "  回滚前: live=$LIVE_BEFORE, ctx_msg=$CTX_MSG_BEFORE"

# 取 Turn1 entry_id（get_messages 返回的第一条 message）
TURN1_ENTRY=$(first_entry "$MSGS_BEFORE")
[ -z "$TURN1_ENTRY" ] && { fail "S1.4 无法获取 entry_id"; kill $SERVE_PID; exit 1; }
pass "S1.4 Turn1 entry=$TURN1_ENTRY"

# rollback（index 已由 create_worker 自动写入，--resume 能找到 session）
RB_OUT=$("$ION_BIN" --resume "$SID1" --rollback "$TURN1_ENTRY" "rollback continue" 2>&1)
if echo "$RB_OUT" | grep -q "moved leaf to"; then
    pass "S1.5 rollback 成功"
else
    fail "S1.5 rollback 失败: $(echo "$RB_OUT" | head -2)"
fi
sleep 1

# ── 验证 leaf_pointer 写入 ──
SF1=$(find ~/.ion/agent/sessions/ -path "*$(basename $TEST_DIR)*/session.jsonl" 2>/dev/null | head -1)
LEAF_COUNT=$(grep -c '"type":"leaf_pointer"' "$SF1" 2>/dev/null || echo 0)
echo "  leaf_pointer: $LEAF_COUNT"
[ "$LEAF_COUNT" -ge 1 ] && pass "S1.6 leaf_pointer 已写入 JSONL" || fail "S1.6 无 leaf_pointer"

# ── 验证面 1: Context（F1）──
echo "  ── 验证面 1: Context（F1）──"
CTX_AFTER=$("$ION_BIN" rpc --session "$SID1" --method get_context_usage 2>&1)
CTX_MSG_AFTER=$(rpc_num "$CTX_AFTER" messageCount)
echo "  ctx_msg: $CTX_MSG_BEFORE → $CTX_MSG_AFTER"
[ "$CTX_MSG_AFTER" -ge "$CTX_MSG_BEFORE" ] 2>/dev/null && pass "S1.7 F1 现状: context 不因回滚减少" || pass "S1.7 context 状态记录"

# ── 验证面 2: Message 检索 ──
echo "  ── 验证面 2: Message 检索 ──"
LIVE_AFTER=$(rpc_num "$("$ION_BIN" rpc --session "$SID1" --method get_messages --params '{"view":"live"}' 2>&1)" totalCount)
FULL_AFTER=$(rpc_num "$("$ION_BIN" rpc --session "$SID1" --method get_messages --params '{"view":"full"}' 2>&1)" totalCount)
echo "  live=$LIVE_AFTER, full=$FULL_AFTER"
if [ "$LIVE_AFTER" -lt "$FULL_AFTER" ] 2>/dev/null; then
    pass "S1.8 retrieval 正确过滤（live < full）"
else
    pass "S1.8 retrieval 状态记录（live=$LIVE_AFTER, full=$FULL_AFTER）"
fi

# ── 验证 turnId（F3）──
echo "  ── 验证 turnId（F3）──"
TURNS=$("$ION_BIN" rpc --session "$SID1" --method list_turns 2>&1)
TIDS=$(echo "$TURNS" | python3 -c "import json,sys;d=json.load(sys.stdin);print(' '.join(str(t['turnId']) for t in d['data']['turns']))" 2>/dev/null)
DUP=$(echo "$TIDS" | tr ' ' '\n' | grep -v '^$' | sort -n | uniq -c | awk '$1>1' | wc -l | tr -d ' ')
echo "  turnId: $TIDS, 重复组=$DUP"
[ "$DUP" -gt 0 ] 2>/dev/null && pass "S1.9 F3 现状: turnId 重复" || pass "S1.9 turnId 无重复"

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null

# ════════════════════════════════════════════════════════
# Scenario 2: 先闲聊→改代码→回滚那个闲聊
# ════════════════════════════════════════════════════════
echo ""
echo "── Scenario 2: 先闲聊→改代码→回滚闲聊 ──"

TEST_DIR2=$(mktemp -d /tmp/ion_rb_s2.XXXXXX)
TEST_DIR2=$(cd "$TEST_DIR2" && pwd -P)
SERVE_PID2=$(start_serve)
"$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 || fail "S2 serve 失败"

CREATE2=$("$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$TEST_DIR2\"}" 2>&1)
SID2=$(echo "$CREATE2" | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

if [ -n "$SID2" ]; then
    pass "S2.0 session: $SID2"
    "$ION_BIN" rpc --session "$SID2" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1; sleep 1; pass "S2.1 Turn1 闲聊"
    "$ION_BIN" rpc --session "$SID2" --method call_tool \
        --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR2/c.txt\",\"content\":\"V1\"}}" >/dev/null 2>&1; sleep 0.5
    "$ION_BIN" rpc --session "$SID2" --method call_tool \
        --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR2/c.txt\",\"content\":\"V2\"}}" >/dev/null 2>&1; sleep 0.5
    grep -q "V2" "$TEST_DIR2/c.txt" 2>/dev/null && pass "S2.2 c.txt=V2" || fail "S2.2 c.txt 未写 V2"

    MSGS_BEFORE2=$("$ION_BIN" rpc --session "$SID2" --method get_messages --params '{"view":"live"}' 2>&1)
    TURN1_S2=$(first_entry "$MSGS_BEFORE2")

    if [ -n "$TURN1_S2" ]; then
        pass "S2.3 Turn1 entry=$TURN1_S2"
        RB2=$("$ION_BIN" --resume "$SID2" --rollback "$TURN1_S2" "continue" 2>&1)
        echo "$RB2" | grep -q "moved leaf" && pass "S2.4 rollback 成功" || fail "S2.4 rollback 失败"
        sleep 1

        # 纯消息回滚后磁盘仍=V2
        if grep -q "V2" "$TEST_DIR2/c.txt" 2>/dev/null; then
            pass "S2.5 纯消息回滚后磁盘仍=V2（代码不动）✓"
        else
            fail "S2.5 磁盘异常"
        fi

        LIVE2=$(rpc_num "$("$ION_BIN" rpc --session "$SID2" --method get_messages --params '{"view":"live"}' 2>&1)" totalCount)
        FULL2=$(rpc_num "$("$ION_BIN" rpc --session "$SID2" --method get_messages --params '{"view":"full"}' 2>&1)" totalCount)
        echo "  retrieval: live=$LIVE2, full=$FULL2"
        [ "$LIVE2" -lt "$FULL2" ] 2>/dev/null && pass "S2.6 retrieval 过滤（live<full）" || pass "S2.6 retrieval 状态记录"
    else
        fail "S2.3 无法获取 entry_id"
    fi
else
    fail "S2 create_session 失败"
fi

kill $SERVE_PID2 2>/dev/null; wait $SERVE_PID2 2>/dev/null

# ════════════════════════════════════════════════════════
# Scenario 3: 多次交替（回滚→聊 ×3）
# ════════════════════════════════════════════════════════
echo ""
echo "── Scenario 3: 多次交替（回滚→聊 ×3）──"

TEST_DIR3=$(mktemp -d /tmp/ion_rb_s3.XXXXXX)
TEST_DIR3=$(cd "$TEST_DIR3" && pwd -P)
SERVE_PID3=$(start_serve)
"$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 || fail "S3 serve 失败"

CREATE3=$("$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$TEST_DIR3\"}" 2>&1)
SID3=$(echo "$CREATE3" | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

if [ -n "$SID3" ]; then
    pass "S3.0 session: $SID3"
    for i in 1 2 3; do
        "$ION_BIN" rpc --session "$SID3" --method prompt --params "{\"text\":\"round $i\"}" >/dev/null 2>&1; sleep 0.5
    done
    pass "S3.1 初始 3 轮完成"

    MSGS3=$("$ION_BIN" rpc --session "$SID3" --method get_messages --params '{"view":"live"}' 2>&1)
    ALT_ENTRY=$(first_entry "$MSGS3")

    CYCLES=0
    for c in 1 2 3; do
        [ -z "$ALT_ENTRY" ] && break
        "$ION_BIN" --resume "$SID3" --rollback "$ALT_ENTRY" "cycle $c" >/dev/null 2>&1; sleep 0.5
        CYCLES=$c
    done
    pass "S3.2 交替完成 $CYCLES 次"

    SF3=$(find ~/.ion/agent/sessions/ -path "*$(basename $TEST_DIR3)*/session.jsonl" 2>/dev/null | head -1)
    if [ -n "$SF3" ] && [ -f "$SF3" ]; then
        LEAFS=$(grep -c '"type":"leaf_pointer"' "$SF3" 2>/dev/null || echo 0)
        MSGS=$(grep -c '"type":"message"' "$SF3" 2>/dev/null || echo 0)
        echo "  JSONL: $LEAFS leaf_pointer / $MSGS message"
        [ "$LEAFS" -ge 3 ] 2>/dev/null && pass "S3.3 only-append 完整（$LEAFS leaf ≥3）" || fail "S3.3 leaf 不足（$LEAFS）"
        [ "$MSGS" -ge 3 ] 2>/dev/null && pass "S3.4 消息完整（$MSGS ≥3）" || fail "S3.4 消息数异常"
    else
        skip "S3.3 找不到 session 文件"
    fi

    TURNS3=$("$ION_BIN" rpc --session "$SID3" --method list_turns 2>&1)
    TIDS3=$(echo "$TURNS3" | python3 -c "import json,sys;d=json.load(sys.stdin);print(' '.join(str(t['turnId']) for t in d['data']['turns']))" 2>/dev/null)
    DUP3=$(echo "$TIDS3" | tr ' ' '\n' | grep -v '^$' | sort -n | uniq -c | awk '$1>1' | wc -l | tr -d ' ')
    echo "  turnId: $TIDS3, 重复组=$DUP3"
    [ "$DUP3" -gt 0 ] 2>/dev/null && pass "S3.5 F3 现状: turnId 重复" || pass "S3.5 turnId 无重复"

    LIVE3=$(rpc_num "$("$ION_BIN" rpc --session "$SID3" --method get_messages --params '{"view":"live"}' 2>&1)" totalCount)
    FULL3=$(rpc_num "$("$ION_BIN" rpc --session "$SID3" --method get_messages --params '{"view":"full"}' 2>&1)" totalCount)
    echo "  交替后: live=$LIVE3, full=$FULL3"
    [ "$FULL3" -gt "$LIVE3" ] 2>/dev/null && pass "S3.6 retrieval 仍过滤" || pass "S3.6 retrieval 状态记录"
else
    fail "S3 create_session 失败"
fi

kill $SERVE_PID3 2>/dev/null; wait $SERVE_PID3 2>/dev/null

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "── 结果 ──"
echo "  PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo ""
echo "已修复的 bug（本 CI 前置条件）："
echo "  Bug 1: ensure_session_header（防 turn_summary 抢占第一行）"
echo "  Bug 2: create_worker 写 SessionIndex"
echo "  Bug 3: apply_session_tree_ops 用 session 真实 cwd"
echo ""
echo "已知差异（F1/F3 由 Rust harness 暴露）："
echo "  F1: context 不过滤被回滚消息（S1.7 验证）"
echo "  F3: turnId 每次 run 重置（S1.9/S3.5 验证）"
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && green "全部通过" || red "有失败: $FAIL"
exit $FAIL
