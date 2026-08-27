#!/usr/bin/env bash
# branch_tree_ci.sh — 分叉树闭环 CLI 验证（fork / get_branches / checkout_branch）
#
# 验证点（对齐 prototypes/session-tree.html 十六问 #1/#4/#9）：
#   A. fork {turnId, name}：从该 turn 尾 entry 分叉，落 leaf_pointer+label 两条
#   B. get_branches：分支富列表（label/消息数/brief/当前标记）+ main 虚拟主干
#   C. checkout_branch {name}：切换后 get_messages 只返回该分支路径（leaf 过滤）
#   D. checkout_branch main：回到主干最新链尾，消息恢复全量
#   E. 错误语义：缺 turnId / 未知分支名报错（不静默成功）
#
# 场景：ion serve（场景 3）+ socket RPC。fixture 直接 append JSONL（带 turnId），
# 全封闭不依赖真实会话/LLM。
#
# 用法：bash tests/branch_tree_ci.sh

set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"

source "$(dirname "$0")/ci_host_helper.sh"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }
jf() { jq -r "$1" 2>/dev/null; }

TEST_DIR="$(mktemp -d /tmp/ion-branch-ci-XXXXXX)"
SID="ci_branch_$$"

cleanup() {
  rm -rf "$TEST_DIR" >/dev/null 2>&1
  # CI host 由 helper 管理（CI_HOST_PID 存在则杀）
  if [ -n "${CI_HOST_PID:-}" ]; then kill "$CI_HOST_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

ensure_host

echo "── 分叉树闭环（session=${SID}）──"

# 1) 建 worker 会话（指定 session_id + cwd → <sid>.jsonl 独立文件）
R=$("$ION_BIN" rpc --method create_session \
    --params "{\"session_id\":\"$SID\",\"agent\":\"build\",\"cwd\":\"$TEST_DIR\"}")
[ "$(echo "$R" | jf '.success')" = "true" ]; check $? "A0 create_session"

SESS_FILE=$(find ~/.ion/agent/sessions -name "$SID.jsonl" 2>/dev/null | head -1)
[ -n "$SESS_FILE" ]; check $? "A1 session file exists: ${SESS_FILE##*/}"

# 2) fixture：3 个 turn × 2 条消息（parentId 链）
cat >> "$SESS_FILE" <<EOF
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-25T10:00:01Z","turnId":1,"message":{"role":"user","content":"第一个问题"}}
{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-25T10:00:02Z","turnId":1,"message":{"role":"assistant","content":"第一个回答"}}
{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-08-25T10:00:03Z","turnId":2,"message":{"role":"user","content":"第二个问题"}}
{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-08-25T10:00:04Z","turnId":2,"message":{"role":"assistant","content":"第二个回答"}}
{"type":"message","id":"m5","parentId":"m4","timestamp":"2026-08-25T10:00:05Z","turnId":3,"message":{"role":"user","content":"第三个问题"}}
{"type":"message","id":"m6","parentId":"m5","timestamp":"2026-08-25T10:00:06Z","turnId":3,"message":{"role":"assistant","content":"第三个回答"}}
EOF

# A2 fork turn 2 → 分支 try-parallel @ m4
R=$("$ION_BIN" rpc --session "$SID" --method fork --params '{"turnId":2,"name":"try-parallel"}')
[ "$(echo "$R" | jf '.success')" = "true" ] \
  && [ "$(echo "$R" | jf '.data.branch')" = "try-parallel" ] \
  && [ "$(echo "$R" | jf '.data.forkFromEntry')" = "m4" ] \
  && [ "$(echo "$R" | jf '.data.appended')" = "2" ]; check $? "A2 fork turn2 -> try-parallel @ m4 (leaf_pointer+label)"

# A3 重复 fork 同 turn（不同名）→ 也成功（多分支并存）
R=$("$ION_BIN" rpc --session "$SID" --method fork --params '{"turnId":1}')
[ "$(echo "$R" | jf '.success')" = "true" ] \
  && [ "$(echo "$R" | jf '.data.branch')" = "branch-2" ]; check $? "A3 fork default name branch-2"

# B1 get_branches：两个命名分支 + main，try-parallel 有消息数
R=$("$ION_BIN" rpc --session "$SID" --method get_branches)
NB=$(echo "$R" | jf '.data.branches | length')
[ "$(echo "$R" | jf '.success')" = "true" ] && [ "$NB" = "2" ]; check $? "B1 get_branches returns 2 named branches"
TP=$(echo "$R" | jq -r '.data.branches[] | select(.name=="try-parallel")')
[ "$(echo "$TP" | jf '.messages')" = "4" ] \
  && [ "$(echo "$TP" | jf '.brief')" = "第二个回答" ]; check $? "B2 try-parallel: 4 messages + brief"

# C1 checkout try-parallel → get_messages 只剩 4 条（m1-m4）
"$ION_BIN" rpc --session "$SID" --method checkout_branch --params '{"name":"try-parallel"}' >/dev/null
N=$("$ION_BIN" rpc --method get_messages --params "{\"session\":\"$SID\"}" | jf '.data.totalCount')
[ "$N" = "4" ]; check $? "C1 after checkout get_messages=4 (m5/m6 leaf-filtered)"

R=$("$ION_BIN" rpc --session "$SID" --method checkout_branch --params '{"name":"main"}')
N=$("$ION_BIN" rpc --method get_messages --params "{\"session\":\"$SID\"}" | jf '.data.totalCount')
[ "$(echo "$R" | jf '.success')" = "true" ] && [ "$N" = "6" ]; check $? "D1 checkout main -> get_messages=6 (full trunk)"

# D2 get_branches main.current=true (not on named branch)
R=$("$ION_BIN" rpc --session "$SID" --method get_branches)
[ "$(echo "$R" | jf '.data.main.current')" = "true" ]; check $? "D2 main.current=true (on trunk)"

# D3 get_context_usage 磁盘回落：worker 内存为空时从会话文件估算（复活 worker 场景）
R=$("$ION_BIN" rpc --session "$SID" --method get_context_usage --params '{}')
MC=$(echo "$R" | jf '.data.messageCount')
[ "$(echo "$R" | jf '.success')" = "true" ] && [ "$MC" -ge 4 ] && [ "$MC" -le 8 ]; check $? "D3 context usage disk fallback (messageCount=$MC in 4..8)"

# E1 错误：缺 turnId
R=$("$ION_BIN" rpc --session "$SID" --method fork --params '{}')
[ "$(echo "$R" | jf '.success')" = "false" ] \
  && echo "$R" | jf '.error' | grep -qi "turnId"; check $? "E1 fork missing turnId errors"

# E2 错误：未知 turnId
R=$("$ION_BIN" rpc --session "$SID" --method fork --params '{"turnId":99}')
[ "$(echo "$R" | jf '.success')" = "false" ]; check $? "E2 fork unknown turnId errors"

# E3 错误：未知分支名（列出可用分支）
R=$("$ION_BIN" rpc --session "$SID" --method checkout_branch --params '{"name":"nonexist"}')
[ "$(echo "$R" | jf '.success')" = "false" ] \
  && echo "$R" | jf '.error' | grep -qi "available"; check $? "E3 checkout unknown branch errors + lists available"

echo
echo "═══ branch_tree_ci: PASS=$PASS FAIL=$FAIL ═══"
[ "$FAIL" -eq 0 ]
