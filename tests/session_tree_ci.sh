#!/usr/bin/env bash
# Session Tree 验证 — 分支/回滚/切换/树展示/only-append 审计
# 用 faux 作为 LLM mock，不调真实 API
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green(){ printf "\033[32m%s\033[0m\n" "$1"; }
red(){ printf "\033[31m%s\033[0m\n" "$1"; }
pass(){ green "✅ PASS: $1"; ((PASS++)); }
fail(){ red "❌ FAIL: $1"; ((FAIL++)); }

echo "── Phase 0: Build ──"
cargo build --bin ion 2>&1 | tail -2
ION_BIN="$PROJECT_DIR/target/debug/ion"

TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"

echo ""
echo "── Group A: session tree 基础展示 ──"

# 造一个 2 轮会话
ION_FAUX_REPLY="first reply" timeout 20 "$ION_BIN" --no-tools "msg1" >/dev/null 2>&1
ION_FAUX_REPLY="second reply" timeout 20 "$ION_BIN" -c --no-tools "msg2" >/dev/null 2>&1

# 找 session 文件
SESSION_FILE=$(find ~/.ion/agent/sessions -name "session.jsonl" -newer "$TEST_DIR" 2>/dev/null | head -1)
if [ -z "$SESSION_FILE" ]; then
    # fallback: 找最近的
    SESSION_FILE=$(find ~/.ion/agent/sessions -name "session.jsonl" -mmin -1 2>/dev/null | head -1)
fi

if [ -n "$SESSION_FILE" ] && [ -f "$SESSION_FILE" ]; then
    pass "A1 session 文件存在"
else
    fail "A1 session 文件存在"
    echo "测试中止（无 session 文件）"
    exit 1
fi

# 统计 message 数
MSG_COUNT=$(grep -c '"type":"message"' "$SESSION_FILE" 2>/dev/null || true)
MSG_COUNT=${MSG_COUNT:-0}
if [ "$MSG_COUNT" -ge 2 ]; then
    pass "A2 至少 2 条消息（实际 $MSG_COUNT）"
else
    fail "A2 至少 2 条消息（实际 $MSG_COUNT）"
fi

# session tree 命令能跑
OUTPUT=$(timeout 10 "$ION_BIN" session tree "" 2>&1)
if echo "$OUTPUT" | grep -qE "└─|├─"; then
    pass "A3 session tree 树展示"
else
    fail "A3 session tree 树展示"
fi

echo ""
echo "── Group B: only-append 不变量审计（核心红线） ──"

# 记录操作前的文件指纹
INITIAL_HASH=$(shasum -a 256 "$SESSION_FILE" 2>/dev/null | awk '{print $1}')
INITIAL_LINES=$(wc -l < "$SESSION_FILE")
INITIAL_MSG_COUNT=$(grep -c '"type":"message"' "$SESSION_FILE" 2>/dev/null || echo 0)

# 取第一个 message 的 entry id（用于 branch）
ENTRY_ID=$(grep '"type":"message"' "$SESSION_FILE" | head -1 | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])" 2>/dev/null)

if [ -n "$ENTRY_ID" ]; then
    # 执行 branch + 后续消息
    ION_FAUX_REPLY="branch reply" timeout 20 "$ION_BIN" -c --no-tools --branch "$ENTRY_ID" --branch-name "try-x" "branch msg" >/dev/null 2>&1

    FINAL_LINES=$(wc -l < "$SESSION_FILE")
    FINAL_MSG_COUNT=$(grep -c '"type":"message"' "$SESSION_FILE" 2>/dev/null || echo 0)

    # only-append 验证 1: 行数只增不减
    if [ "$FINAL_LINES" -gt "$INITIAL_LINES" ]; then
        pass "B1 文件行数只增不减（$INITIAL_LINES → $FINAL_LINES）"
    else
        fail "B1 文件行数只增不减（$INITIAL_LINES → $FINAL_LINES）"
    fi

    # only-append 验证 2: 消息数不丢
    if [ "$FINAL_MSG_COUNT" -ge "$INITIAL_MSG_COUNT" ]; then
        pass "B2 消息数不丢（$INITIAL_MSG_COUNT → $FINAL_MSG_COUNT）"
    else
        fail "B2 消息数不丢（$INITIAL_MSG_COUNT → $FINAL_MSG_COUNT）"
    fi

    # only-append 验证 3: 前 N 行内容不变（sha256）
    PREFIX_HASH=$(head -n "$INITIAL_LINES" "$SESSION_FILE" | shasum -a 256 | awk '{print $1}')
    if [ "$PREFIX_HASH" = "$INITIAL_HASH" ]; then
        pass "B3 前 $INITIAL_LINES 行 sha256 不变（only-append 核心证明）"
    else
        fail "B3 前 $INITIAL_LINES 行内容被修改（违反 only-append！）"
    fi

    # 验证 leaf_pointer 写入了
    if grep -q '"type":"leaf_pointer"' "$SESSION_FILE"; then
        pass "B4 leaf_pointer entry 已写入"
    else
        fail "B4 leaf_pointer entry 已写入"
    fi

    # 验证 label 写入了
    if grep -q '"type":"label"' "$SESSION_FILE"; then
        pass "B5 label entry 已写入"
    else
        fail "B5 label entry 已写入"
    fi
else
    fail "B* 无法提取 entry id"
fi

echo ""
echo "── Group C: rollback + tombstone ──"

if [ -n "$ENTRY_ID" ]; then
    BEFORE_TOMBSTONE=$(wc -l < "$SESSION_FILE")
    # rollback 到第一个 entry，带 reason
    ION_FAUX_REPLY="after rollback" timeout 20 "$ION_BIN" -c --no-tools --rollback "$ENTRY_ID" --rollback-reason "test rollback" "rollback msg" >/dev/null 2>&1

    AFTER_TOMBSTONE=$(wc -l < "$SESSION_FILE")

    if [ "$AFTER_TOMBSTONE" -gt "$BEFORE_TOMBSTONE" ]; then
        pass "C1 rollback 后行数增加（only-append）"
    else
        fail "C1 rollback 后行数增加"
    fi

    if grep -q '"type":"branch_summary"' "$SESSION_FILE"; then
        pass "C2 branch_summary tombstone 已写入"
    else
        fail "C2 branch_summary tombstone 已写入"
    fi

    if grep -q "test rollback" "$SESSION_FILE"; then
        pass "C3 tombstone 含 reason 文本"
    else
        fail "C3 tombstone 含 reason 文本"
    fi
fi

echo ""
echo "── Group D: 错误处理 ──"

# branch 不存在的 entry
OUTPUT=$(timeout 10 "$ION_BIN" -c --no-tools --branch "nonexistent_xxx " "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "not found"; then
    pass "D1 branch 不存在的 entry 报错"
else
    fail "D1 branch 不存在的 entry 报错"
fi

# checkout 不存在的分支名
OUTPUT=$(timeout 10 "$ION_BIN" -c --no-tools --checkout "nonexist_branch" "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "not found\|Available"; then
    pass "D2 checkout 不存在的分支报错"
else
    fail "D2 checkout 不存在的分支报错"
fi

echo ""
echo "── 结果 ──"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && green "全部通过" || red "有失败"
exit $FAIL
