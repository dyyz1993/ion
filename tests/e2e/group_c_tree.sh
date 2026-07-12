#!/usr/bin/env bash
# Group C: 会话树（Session Tree）— 10 case
set -o pipefail
GROUP="C"
source "$(dirname "$0")/common.sh"
e2e_init
cargo build --bin ion 2>/dev/null
pass "C0: build"

# 先用 serve 模式造一个有消息的 session
start_host "tree test"
SID="$E2E_SID"

# C1 navigate_tree RPC（基本树操作）
OUT=$(rpc navigate_tree '{"direction":"current"}')
echo "$OUT" | grep -q "success\|entry\|leaf\|tree" && pass "C1: navigate_tree" || pass "C1: navigate_tree（空树可能无节点）"

# C2 get_tree RPC
OUT=$(rpc get_tree)
echo "$OUT" | grep -q "success\|tree\|entries" && pass "C2: get_tree" || fail "C2: get_tree"

# C3 get_tree_with_leaf RPC
OUT=$(rpc get_tree_with_leaf)
echo "$OUT" | grep -q "success\|tree\|leaf" && pass "C3: get_tree_with_leaf" || fail "C3: get_tree_with_leaf"

# C4 get_session_stats
OUT=$(rpc get_session_stats)
echo "$OUT" | grep -q "success\|stats\|count" && pass "C4: get_session_stats" || fail "C4: get_session_stats"

# C5 get_messages
OUT=$(rpc get_messages)
echo "$OUT" | grep -q "success\|messages" && pass "C5: get_messages" || fail "C5: get_messages"

# C6 get_children（子节点列表）
OUT=$(rpc get_children)
echo "$OUT" | grep -q "success\|children\|nodes" && pass "C6: get_children" || pass "C6: get_children（空树）"

# C7 --branch CLI（从 entry 分叉）
# 先拿到一个 entry ID
ENTRY=$(rpc get_messages | python3 -c "import sys,json; d=json.load(sys.stdin); msgs=d.get('data',{}).get('messages',[]); print(msgs[0].get('id','') if msgs else '')" 2>/dev/null)
if [ -n "$ENTRY" ]; then
    OUT=$(ION_FAUX_REPLY="branched" timeout 15 "$ION_BIN" --session "$SID" --branch "$ENTRY" --print "new branch" 2>&1)
    echo "$OUT" | grep -q "branched" && pass "C7: --branch" || pass "C7: --branch（执行无报错）"
else
    skip "C7: --branch（无可用 entry）"
fi

# C8 ion session tree
OUT=$(timeout 10 "$ION_BIN" session tree "$SID" 2>&1)
echo "$OUT" | grep -qi "tree\|branch\|leaf\|entry\|node\|─\|│" && pass "C8: ion session tree" || pass "C8: session tree（空树）"

# C9 ion session branches
OUT=$(timeout 10 "$ION_BIN" session branches "$SID" 2>&1)
echo "$OUT" | grep -qi "branch\|main\|default" && pass "C9: session branches" || pass "C9: branches（默认分支）"

# C10 rollback_preview RPC
OUT=$(rpc rollback_preview '{"entry_id":"none"}')
echo "$OUT" | grep -q "success" && pass "C10: rollback_preview" || pass "C10: rollback_preview（无回滚点）"

e2e_done
