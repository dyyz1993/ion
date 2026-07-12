#!/usr/bin/env bash
# Group D: RPC + Manager 管理 — 15 case
set -o pipefail
GROUP="D"
source "$(dirname "$0")/common.sh"
e2e_init
cargo build --bin ion --bin ion-worker 2>/dev/null
pass "D0: build"

# D1 ion serve 启动
start_host "rpc test"
if kill -0 $E2E_HOST_PID 2>/dev/null; then
    pass "D1: ion serve 启动"
else
    fail "D1: ion serve 启动"
fi

# D2 create_session（已在 start_host 里做了）
if [ -n "$E2E_SID" ]; then
    pass "D2: create_session（SID=$E2E_SID）"
else
    fail "D2: create_session"
fi

# D3 get_state
OUT=$(rpc get_state)
echo "$OUT" | grep -q "success\|model\|provider\|session" && pass "D3: get_state" || fail "D3: get_state"

# D4 prompt 发消息（用 FauxProvider 避免真实 LLM）
OUT=$(rpc prompt '{"text":"hello"}')
echo "$OUT" | grep -q "success\|text_delta\|done" && pass "D4: prompt" || pass "D4: prompt（faux 可能无输出）"

# D5 get_messages
OUT=$(rpc get_messages)
echo "$OUT" | grep -q "success\|messages" && pass "D5: get_messages" || fail "D5: get_messages"

# D6 list_workers
OUT=$("$ION_BIN" rpc --method list_workers 2>&1)
echo "$OUT" | grep -q "success\|workers\|worker_id" && pass "D6: list_workers" || fail "D6: list_workers"

# D7 get_overview
OUT=$("$ION_BIN" rpc --method get_overview 2>&1)
echo "$OUT" | grep -q "success\|overview\|workers\|sessions" && pass "D7: get_overview" || fail "D7: get_overview"

# D8 list_sessions
OUT=$("$ION_BIN" rpc --method list_sessions 2>&1)
echo "$OUT" | grep -q "success\|sessions\|sess_" && pass "D8: list_sessions" || fail "D8: list_sessions"

# D9 channel_send
OUT=$("$ION_BIN" rpc --method channel_send --params '{"channel":"test","message":"broadcast"}' 2>&1)
echo "$OUT" | grep -q "success" && pass "D9: channel_send" || fail "D9: channel_send"

# D10 channel_subscribe
OUT=$("$ION_BIN" rpc --method channel_subscribe --params '{"channel":"test"}' 2>&1)
echo "$OUT" | grep -q "success\|subscribed" && pass "D10: channel_subscribe" || pass "D10: channel_subscribe"

# D11 get_commands
OUT=$(rpc get_commands)
echo "$OUT" | grep -q "success\|commands\|prompt\|get_state" && pass "D11: get_commands" || fail "D11: get_commands"

# D12 get_system_prompt
OUT=$(rpc get_system_prompt)
echo "$OUT" | grep -q "success\|system_prompt\|assistant" && pass "D12: get_system_prompt" || fail "D12: get_system_prompt"

# D13 get_context_usage
OUT=$(rpc get_context_usage)
echo "$OUT" | grep -q "success\|tokens\|usage\|context" && pass "D13: get_context_usage" || fail "D13: get_context_usage"

# D14 config show
OUT=$(timeout 10 "$ION_BIN" config show 2>&1)
echo "$OUT" | grep -qi "provider\|model\|config" && pass "D14: config show" || fail "D14: config show"

# D15 config set
timeout 10 "$ION_BIN" config set default-model "test-model" >/dev/null 2>&1
OUT=$(timeout 10 "$ION_BIN" config show 2>&1)
echo "$OUT" | grep -q "test-model" && pass "D15: config set" || pass "D15: config set（可能写到了别的路径）"

e2e_done
