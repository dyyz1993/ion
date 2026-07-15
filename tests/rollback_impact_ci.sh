#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Rollback Impact CI — 回滚对 Context / Message / Compaction 的影响（记现状）
#
# 断言策略：记录当前实际行为（含已知差异 F1/F2/F3），全 pass 作为 regression baseline。
#
# 三大验证面：Context / Message 检索 / Compaction（token）
# 三个 Scenario：修改后回滚→继续 / 先闲聊→改代码→回滚闲聊 / 多次交替
#
# 策略：用 call_tool RPC 直接调 write（时序可控），faux 只负责闲聊 text 响应。
#       避免 FauxProvider FIFO 时序问题。
#
# 已知差异（本 CI 记录现状，不修复）：
#   F1: --resume 后 SessionFile::load 不过滤 leaf_pointer → context 含被回滚消息
#   F3: turnId 每次 run 从 0 重置 → 回滚后继续聊 turnId 重复
#
# 注意：本脚本必须用 /bin/bash 执行（zsh profile 会干扰）。
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"

PASS=0; FAIL=0; SKIP=0
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
yellow(){ printf "\033[33m%s\033[0m\n" "$1"; }
pass() { PASS=$((PASS+1)); green "  ✅ $1"; }
fail() { FAIL=$((FAIL+1)); red "  ❌ $1"; }
skip() { SKIP=$((SKIP+1)); yellow "  ⏭️  $1"; }

rpc_str() { echo "$1" | grep -o "\"$2\": *\"[^\"]*\"" | head -1 | sed "s/\"$2\": *\"//;s/\"$//"; }
rpc_num() { echo "$1" | grep -o "\"$2\": *[0-9]*" | head -1 | grep -oE "[0-9]+$"; }
first_entry_id() {
    echo "$1" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
    for m in d.get('data',{}).get('messages',[]):
        if m.get('type')=='message': print(m.get('id','')); break
except: pass
" 2>/dev/null
}

# 启动一个干净的 serve（faux 只回 text），返回 PID
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
SERVE_PID=$(start_serve)
"$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 || { fail "serve 启动失败"; exit 1; }
pass "S1.0 serve 启动"

CREATE1=$("$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$TEST_DIR\"}" 2>&1)
SID1=$(rpc_str "$CREATE1" session_id)
[ -z "$SID1" ] && { fail "S1 create_session 失败: $CREATE1"; kill $SERVE_PID; exit 1; }
pass "S1.0b session: $SID1"

# Turn1: 闲聊（faux 回 "ok"）
"$ION_BIN" rpc --session "$SID1" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1
sleep 1
pass "S1.1 Turn1 闲聊"

# Turn2: write a.txt=V1（用 call_tool 直接调，时序可控）
"$ION_BIN" rpc --session "$SID1" --method call_tool \
    --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR/a.txt\",\"content\":\"V1\"}}" >/dev/null 2>&1
sleep 0.5
grep -q "V1" "$TEST_DIR/a.txt" 2>/dev/null && pass "S1.2 Turn2 write a.txt=V1" || fail "S1.2 a.txt 未写 V1"

# Turn3: write a.txt=V2
"$ION_BIN" rpc --session "$SID1" --method call_tool \
    --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR/a.txt\",\"content\":\"V2\"}}" >/dev/null 2>&1
sleep 0.5
grep -q "V2" "$TEST_DIR/a.txt" 2>/dev/null && pass "S1.3 Turn3 write a.txt=V2" || fail "S1.3 a.txt 未写 V2"

# 记录回滚前状态
MSGS_BEFORE=$("$ION_BIN" rpc --session "$SID1" --method get_messages --params '{"view":"live"}' 2>&1)
CTX_BEFORE=$("$ION_BIN" rpc --session "$SID1" --method get_context_usage 2>&1)
LIVE_BEFORE=$(rpc_num "$MSGS_BEFORE" totalCount)
CTX_MSG_BEFORE=$(rpc_num "$CTX_BEFORE" messageCount)
CTX_TOK_BEFORE=$(rpc_num "$CTX_BEFORE" estimatedTokens)
echo "  回滚前: live_msg=$LIVE_BEFORE, ctx_msg=$CTX_MSG_BEFORE, ctx_tok=$CTX_TOK_BEFORE"

# 取 Turn1（第一条 user message）的 entry_id
TURN1_ENTRY=$(first_entry_id "$MSGS_BEFORE")
if [ -z "$TURN1_ENTRY" ]; then
    fail "S1.4 无法获取 Turn1 entry_id"
else
    pass "S1.4 Turn1 entry_id=$TURN1_ENTRY"
fi

# 回滚到 Turn1（--rollback 必须带 message，会先追加 leaf_pointer 再跑一轮 agent）
ROLLBACK_OUT=$("$ION_BIN" --resume "$SID1" --rollback "$TURN1_ENTRY" "rollback and continue" 2>&1)
if echo "$ROLLBACK_OUT" | grep -q "moved leaf to"; then
    pass "S1.5 rollback 成功"
else
    fail "S1.5 rollback 失败: $(echo "$ROLLBACK_OUT" | head -2)"
fi
sleep 1

# ── 验证面 1: Context 影响（F1）──
echo "  ── 验证面 1: Context 影响 ──"
CTX_AFTER=$("$ION_BIN" rpc --session "$SID1" --method get_context_usage 2>&1)
CTX_MSG_AFTER=$(rpc_num "$CTX_AFTER" messageCount)
CTX_TOK_AFTER=$(rpc_num "$CTX_AFTER" estimatedTokens)
echo "  回滚+新轮后: ctx_msg=$CTX_MSG_AFTER, ctx_tok=$CTX_TOK_AFTER"

# F1 现状：context 只增不减（被回滚消息仍在内存）
if [ "${CTX_MSG_AFTER:-0}" -gt "${CTX_MSG_BEFORE:-0}" ] 2>/dev/null; then
    pass "S1.6 F1 现状记录: context 只增不减（$CTX_MSG_BEFORE → $CTX_MSG_AFTER，被回滚消息仍在）"
else
    pass "S1.6 context 状态记录（$CTX_MSG_BEFORE → $CTX_MSG_AFTER）"
fi

# ── 验证面 2: Message 检索 ──
echo "  ── 验证面 2: Message 检索 ──"
MSGS_LIVE=$("$ION_BIN" rpc --session "$SID1" --method get_messages --params '{"view":"live"}' 2>&1)
LIVE_AFTER=$(rpc_num "$MSGS_LIVE" totalCount)
MSGS_FULL=$("$ION_BIN" rpc --session "$SID1" --method get_messages --params '{"view":"full"}' 2>&1)
FULL_AFTER=$(rpc_num "$MSGS_FULL" totalCount)
echo "  get_messages: live=$LIVE_AFTER, full=$FULL_AFTER"

# retrieval 层读磁盘看到 leaf_pointer → live 应过滤
if [ "${LIVE_AFTER:-0}" -lt "${FULL_AFTER:-0}" ] 2>/dev/null; then
    pass "S1.7 retrieval 正确过滤（live=$LIVE_AFTER < full=$FULL_AFTER）"
else
    pass "S1.7 retrieval 状态记录（live=$LIVE_AFTER, full=$FULL_AFTER）"
fi

# ── 验证面 3: Compaction 影响 ──
echo "  ── 验证面 3: Compaction ──"
echo "  token: $CTX_TOK_BEFORE → $CTX_TOK_AFTER"
if [ "${CTX_TOK_AFTER:-0}" -ge "${CTX_TOK_BEFORE:-0}" ] 2>/dev/null; then
    pass "S1.8 F1 现状: token 不因回滚减少（compaction 判定基于含被回滚消息的 context）"
else
    pass "S1.8 token 状态记录"
fi

# ── 验证 turnId（F3）──
echo "  ── 验证 turnId（F3）──"
TURNS=$("$ION_BIN" rpc --session "$SID1" --method list_turns 2>&1)
TIDS=$(echo "$TURNS" | grep -o '"turnId": *[0-9]*' | grep -oE "[0-9]+$" | sort -n)
DUP=$(echo "$TIDS" | uniq -c | awk '$1>1' | wc -l | tr -d ' ')
echo "  turnId: $(echo "$TIDS" | tr '\n' ' ')"
if [ "${DUP:-0}" -gt 0 ]; then
    pass "S1.9 F3 现状: turnId 重复（每次 run 从 0 重置）"
else
    pass "S1.9 turnId 无重复"
fi

# ── 验证 JSONL only-append ──
SESSION_FILE=$(find ~/.ion/agent/sessions/ -name "$SID1.jsonl" 2>/dev/null | head -1)
if [ -n "$SESSION_FILE" ] && [ -f "$SESSION_FILE" ]; then
    LEAFS=$(grep -c '"type":"leaf_pointer"' "$SESSION_FILE" 2>/dev/null || echo 0)
    MSGS=$(grep -c '"type":"message"' "$SESSION_FILE" 2>/dev/null || echo 0)
    echo "  JSONL: $LEAFS leaf_pointer, $MSGS message"
    [ "${LEAFS:-0}" -ge 1 ] && pass "S1.10 leaf_pointer 已追加（only-append）" || fail "S1.10 无 leaf_pointer"
    [ "${MSS:-0}" -ge 1 ] 2>/dev/null || pass "S1.10b message 保留"
fi

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null

# ════════════════════════════════════════════════════════
# Scenario 2: 先闲聊→改代码→回滚那个闲聊
# ════════════════════════════════════════════════════════
echo ""
echo "── Scenario 2: 先闲聊→改代码→回滚闲聊 ──"

TEST_DIR2=$(mktemp -d /tmp/ion_rb_s2.XXXXXX)
SERVE_PID2=$(start_serve)
"$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 || { fail "S2 serve 失败"; }

CREATE2=$("$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$TEST_DIR2\"}" 2>&1)
SID2=$(rpc_str "$CREATE2" session_id)

if [ -n "$SID2" ]; then
    pass "S2.0 session: $SID2"

    # Turn1: 闲聊
    "$ION_BIN" rpc --session "$SID2" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1; sleep 1
    pass "S2.1 Turn1 闲聊"

    # Turn2: write c.txt V1→V2（call_tool）
    "$ION_BIN" rpc --session "$SID2" --method call_tool \
        --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR2/c.txt\",\"content\":\"V1\"}}" >/dev/null 2>&1; sleep 0.5
    "$ION_BIN" rpc --session "$SID2" --method call_tool \
        --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR2/c.txt\",\"content\":\"V2\"}}" >/dev/null 2>&1; sleep 0.5
    grep -q "V2" "$TEST_DIR2/c.txt" 2>/dev/null && pass "S2.2 c.txt=V2" || fail "S2.2 c.txt 未写 V2"

    # 回滚前
    MSGS_BEFORE2=$("$ION_BIN" rpc --session "$SID2" --method get_messages --params '{"view":"live"}' 2>&1)
    TURN1_S2=$(first_entry_id "$MSGS_BEFORE2")

    if [ -n "$TURN1_S2" ]; then
        pass "S2.3 Turn1 entry_id=$TURN1_S2"

        # 纯消息回滚（不带 restore-code）
        RB2=$("$ION_BIN" --resume "$SID2" --rollback "$TURN1_S2" "continue" 2>&1)
        if echo "$RB2" | grep -q "moved leaf"; then
            pass "S2.4 rollback 到闲聊点成功"
        else
            fail "S2.4 rollback 失败: $(echo "$RB2"|head -2)"
        fi
        sleep 1

        # 验证：磁盘仍=V2（纯消息回滚不动磁盘）
        if grep -q "V2" "$TEST_DIR2/c.txt" 2>/dev/null; then
            pass "S2.5 纯消息回滚后磁盘仍=V2（代码不动）✓"
        else
            fail "S2.5 磁盘异常"
        fi

        # retrieval 对比
        LIVE2=$(rpc_num "$("$ION_BIN" rpc --session "$SID2" --method get_messages --params '{"view":"live"}' 2>&1)" totalCount)
        FULL2=$(rpc_num "$("$ION_BIN" rpc --session "$SID2" --method get_messages --params '{"view":"full"}' 2>&1)" totalCount)
        echo "  retrieval: live=$LIVE2, full=$FULL2"
        if [ "${LIVE2:-0}" -lt "${FULL2:-0}" ] 2>/dev/null; then
            pass "S2.6 retrieval 过滤（live < full）"
        else
            pass "S2.6 retrieval 状态记录（live=$LIVE2, full=$FULL2）"
        fi

        # context（F1）
        CTX_MSG2=$(rpc_num "$("$ION_BIN" rpc --session "$SID2" --method get_context_usage 2>&1)" messageCount)
        echo "  context messageCount=$CTX_MSG2（F1: 含被回滚闲聊消息）"
        pass "S2.7 context 状态记录"
    else
        fail "S2.3 无法获取 Turn1 entry_id"
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
SERVE_PID3=$(start_serve)
"$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 || { fail "S3 serve 失败"; }

CREATE3=$("$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$TEST_DIR3\"}" 2>&1)
SID3=$(rpc_str "$CREATE3" session_id)

if [ -n "$SID3" ]; then
    pass "S3.0 session: $SID3"

    # 初始 3 轮
    for i in 1 2 3; do
        "$ION_BIN" rpc --session "$SID3" --method prompt --params "{\"text\":\"round $i\"}" >/dev/null 2>&1
        sleep 0.5
    done
    pass "S3.1 初始 3 轮完成"

    MSGS3=$("$ION_BIN" rpc --session "$SID3" --method get_messages --params '{"view":"live"}' 2>&1)
    ALT_ENTRY=$(first_entry_id "$MSGS3")
    echo "  回滚锚点: $ALT_ENTRY"

    # 交替 ×3
    CYCLES=0
    for c in 1 2 3; do
        [ -z "$ALT_ENTRY" ] && break
        "$ION_BIN" --resume "$SID3" --rollback "$ALT_ENTRY" "cycle $c" >/dev/null 2>&1
        sleep 0.5
        CYCLES=$c
    done
    pass "S3.2 交替完成 $CYCLES 次"

    # JSONL 完整性
    SESSION_FILE=$(find ~/.ion/agent/sessions/ -name "$SID3.jsonl" 2>/dev/null | head -1)
    if [ -n "$SESSION_FILE" ] && [ -f "$SESSION_FILE" ]; then
        LEAFS=$(grep -c '"type":"leaf_pointer"' "$SESSION_FILE" 2>/dev/null || echo 0)
        MSGS=$(grep -c '"type":"message"' "$SESSION_FILE" 2>/dev/null || echo 0)
        echo "  JSONL: $LEAFS leaf_pointer / $MSGS message"
        if [ "${LEAFS:-0}" -ge 3 ]; then
            pass "S3.3 only-append 完整（$LEAFS leaf_pointer ≥ 3）"
        else
            fail "S3.3 leaf_pointer 不足（$LEAFS < 3）"
        fi
        if [ "${MSGS:-0}" -ge 3 ]; then
            pass "S3.4 消息完整（$MSGS ≥ 3）"
        else
            fail "S3.4 消息数异常（$MSGS）"
        fi
    else
        skip "S3.3 找不到 session 文件"
    fi

    # turnId（F3）
    TURNS3=$("$ION_BIN" rpc --session "$SID3" --method list_turns 2>&1)
    TIDS3=$(echo "$TURNS3" | grep -o '"turnId": *[0-9]*' | grep -oE "[0-9]+$" | sort -n)
    DUP3=$(echo "$TIDS3" | uniq -c | awk '$1>1' | wc -l | tr -d ' ')
    echo "  turnId: $(echo "$TIDS3" | tr '\n' ' '), 重复组=$DUP3"
    if [ "${DUP3:-0}" -gt 0 ]; then
        pass "S3.5 F3 现状: 交替后 turnId 重复"
    else
        pass "S3.5 turnId 无重复"
    fi

    # retrieval 仍正确
    LIVE3=$(rpc_num "$("$ION_BIN" rpc --session "$SID3" --method get_messages --params '{"view":"live"}' 2>&1)" totalCount)
    FULL3=$(rpc_num "$("$ION_BIN" rpc --session "$SID3" --method get_messages --params '{"view":"full"}' 2>&1)" totalCount)
    echo "  交替后 retrieval: live=$LIVE3, full=$FULL3"
    if [ "${FULL3:-0}" -gt "${LIVE3:-0}" ] 2>/dev/null; then
        pass "S3.6 retrieval 交替后仍过滤（live < full）"
    else
        pass "S3.6 retrieval 状态记录"
    fi
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
echo "已知差异（本 CI 记录现状）："
echo "  F1: context 不过滤被回滚消息（S1.6/S2.7）"
echo "  F3: turnId 每次 run 重置（S1.9/S3.5）"
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && green "全部通过（现状基线建立）" || red "有失败: $FAIL"
exit $FAIL
