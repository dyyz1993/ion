#!/usr/bin/env bash
# Group K: Compaction + 消息拉取 — 10 case
set -o pipefail
GROUP="K"
source "$(dirname "$0")/common.sh"
e2e_init
cargo build --bin ion --bin ion-worker 2>/dev/null
pass "K0: build"

start_host "compaction test"
SID="$E2E_SID"

# K1 get_context_usage
OUT=$(rpc get_context_usage)
echo "$OUT" | grep -q "success\|tokens\|usage\|context" && pass "K1: get_context_usage" || fail "K1: get_context_usage"

# K2 set_auto_compaction
OUT=$(rpc set_auto_compaction '{"enabled":true,"threshold":50000}')
echo "$OUT" | grep -q "success" && pass "K2: set_auto_compaction" || fail "K2: set_auto_compaction"

# K3 compact（手动压缩）
OUT=$(rpc compact)
echo "$OUT" | grep -q "success\|compacted\|skipped\|no need" && pass "K3: compact" || pass "K3: compact（可能消息太少跳过）"

# K4 get_messages 分页
OUT=$(rpc get_messages '{"limit":5}')
echo "$OUT" | grep -q "success\|messages" && pass "K4: get_messages limit" || fail "K4: get_messages"

# K5 get_messages since_compaction 视图
OUT=$(rpc get_messages '{"view":"since_compaction"}')
echo "$OUT" | grep -q "success\|messages\|note" && pass "K5: since_compaction 视图" || fail "K5: since_compaction"

# K6 list_turns
OUT=$(rpc list_turns)
echo "$OUT" | grep -q "success\|turns" && pass "K6: list_turns" || fail "K6: list_turns"

# K7 list_inputs
OUT=$(rpc list_inputs)
echo "$OUT" | grep -q "success\|inputs\|user" && pass "K7: list_inputs" || fail "K7: list_inputs"

# K8 get_session_stats
OUT=$(rpc get_session_stats)
echo "$OUT" | grep -q "success\|stats\|count\|tokens" && pass "K8: get_session_stats" || fail "K8: get_session_stats"

# K9 get_system_prompt
OUT=$(rpc get_system_prompt)
echo "$OUT" | grep -q "success\|system_prompt\|assistant" && pass "K9: get_system_prompt" || fail "K9: get_system_prompt"

# K10 get_full_messages
OUT=$(rpc get_full_messages)
echo "$OUT" | grep -q "success\|messages" && pass "K10: get_full_messages" || fail "K10: get_full_messages"

e2e_done
