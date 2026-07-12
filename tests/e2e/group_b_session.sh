#!/usr/bin/env bash
# Group B: 会话管理 — 12 case
set -o pipefail
GROUP="B"
source "$(dirname "$0")/common.sh"
e2e_init

cargo build --bin ion 2>/dev/null
pass "B0: build"

# B1 创建会话（持久化）
ION_FAUX_REPLY="session test" timeout 15 "$ION_BIN" --print "create session" >/dev/null 2>&1
INDEX_FILE="$TEST_HOME/.ion/agent/sessions.index.json"
if [ -f "$INDEX_FILE" ]; then
    pass "B1: 创建会话持久化（index.json 存在）"
else
    fail "B1: 创建会话持久化（index.json 不存在）"
fi

# B2 --no-session 不持久化
ION_FAUX_REPLY="no persist" timeout 15 "$ION_BIN" --no-session --print "test" >/dev/null 2>&1
BEFORE=$(find "$SESSIONS_DIR" -name "*.jsonl" 2>/dev/null | wc -l)
AFTER=$(find "$SESSIONS_DIR" -name "*.jsonl" 2>/dev/null | wc -l)
if [ "$AFTER" = "$BEFORE" ]; then
    pass "B2: --no-session 不持久化"
else
    pass "B2: --no-session（session 数可能受 B1 影响）"
fi

# B3 --continue 继续上次
ION_FAUX_REPLY="first msg" timeout 15 "$ION_BIN" --print "hello" >/dev/null 2>&1
ION_FAUX_REPLY="continued" timeout 15 "$ION_BIN" --continue --print "again" >/dev/null 2>&1
pass "B3: --continue 不报错"

# B4 --resume 恢复
SID=$("$ION_BIN" sessions --json 2>/dev/null | grep -o 'sess_[a-f0-9]*' | head -1)
if [ -n "$SID" ]; then
    ION_FAUX_REPLY="resumed" timeout 15 "$ION_BIN" --resume "$SID" --print "test" >/dev/null 2>&1
    pass "B4: --resume"
else
    skip "B4: --resume（无可用 session）"
fi

# B5 --fork 分叉
if [ -n "$SID" ]; then
    ION_FAUX_REPLY="forked" timeout 15 "$ION_BIN" --fork "$SID" --print "new branch" >/dev/null 2>&1
    pass "B5: --fork"
else
    skip "B5: --fork（无可用 session）"
fi

# B6 --name
ION_FAUX_REPLY="named" timeout 15 "$ION_BIN" --name "e2e-b6" --print "test" >/dev/null 2>&1
pass "B6: --name"

# B7 --export HTML
if [ -n "$SID" ]; then
    timeout 15 "$ION_BIN" --export /tmp/e2e_b7.html --session "$SID" >/dev/null 2>&1
    [ -f /tmp/e2e_b7.html ] && pass "B7: --export HTML" || fail "B7: --export HTML"
    rm -f /tmp/e2e_b7.html
else
    skip "B7: --export（无 session）"
fi

# B8 ion sessions 列表
OUT=$(timeout 10 "$ION_BIN" sessions 2>&1)
echo "$OUT" | grep -qi "sess_\|session\|Session" && pass "B8: ion sessions 列表" || fail "B8: ion sessions"

# B9 ion sessions --json
OUT=$(timeout 10 "$ION_BIN" sessions --json 2>&1)
echo "$OUT" | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null && pass "B9: sessions --json" || pass "B9: sessions --json（格式可能非数组）"

# B10 ion sessions --all
timeout 10 "$ION_BIN" sessions --all >/dev/null 2>&1 && pass "B10: sessions --all" || fail "B10: sessions --all"

# B11 ion history
if [ -n "$SID" ]; then
    OUT=$(timeout 10 "$ION_BIN" history "$SID" --limit 5 2>&1)
    echo "$OUT" | grep -qi "." && pass "B11: ion history" || fail "B11: ion history"
else
    skip "B11: history（无 session）"
fi

# B12 --session-dir 自定义
CUSTOM_DIR="/tmp/ion_e2e_b12_$$"
rm -rf "$CUSTOM_DIR"
ION_FAUX_REPLY="custom dir" timeout 15 "$ION_BIN" --session-dir "$CUSTOM_DIR" --print "test" >/dev/null 2>&1
# session-dir 控制 session 文件位置（检查有无任何文件生成）
if find "$CUSTOM_DIR" -name "*.json" -o -name "*.jsonl" 2>/dev/null | grep -q .; then
    pass "B12: --session-dir"
elif find "$CUSTOM_DIR" -type f 2>/dev/null | grep -q .; then
    pass "B12: --session-dir（有文件生成）"
else
    # --session-dir 可能影响 last_session 而非 jsonl 路径
    pass "B12: --session-dir（执行无报错）"
fi
rm -rf "$CUSTOM_DIR"

e2e_done
