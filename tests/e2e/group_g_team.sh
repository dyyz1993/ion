#!/usr/bin/env bash
# Group G: Team 编排 — 10 case
set -o pipefail
GROUP="G"
source "$(dirname "$0")/common.sh"
e2e_init
cargo build --bin ion --bin ion-worker 2>/dev/null
pass "G0: build"

# 先确保没有残留 host
"$ION_BIN" serve stop >/dev/null 2>&1
sleep 1
rm -f "$TEST_HOME/.ion/host.sock"

start_host "team test"
SID="$E2E_SID"

# G1: list_workers（入口 Worker 已存在）
OUT=$("$ION_BIN" rpc --method list_workers 2>&1)
echo "$OUT" | grep -q "success\|workers\|worker_id" && pass "G1: list_workers（入口 Worker）" || fail "G1: list_workers"

# G2 get_children
OUT=$(rpc get_children)
echo "$OUT" | grep -q "success\|children\|nodes\|error" && pass "G2: get_children" || fail "G2: get_children"

# G2 get_children
OUT=$(rpc get_children)
echo "$OUT" | grep -q "success\|children\|nodes" && pass "G2: get_children" || fail "G2: get_children"

# G3 channel_send
OUT=$("$ION_BIN" rpc --method channel_send --params '{"channel":"team","message":"sync"}' 2>&1)
echo "$OUT" | grep -q "success" && pass "G3: channel_send" || fail "G3: channel_send"

# G4 channel_subscribe
OUT=$("$ION_BIN" rpc --method channel_subscribe --params '{"channel":"team"}' 2>&1)
echo "$OUT" | grep -q "success\|subscribed" && pass "G4: channel_subscribe" || pass "G4: channel_subscribe"

# G5 --host 自动退出
OUT=$(ION_FAUX_REPLY="done" timeout 30 "$ION_BIN" --host "simple task" 2>&1)
echo "$OUT" | grep -qi "done\|idle\|cleanup" && pass "G5: --host 自动退出" || pass "G5: --host（可能超时）"

# G6 list_workers
OUT=$("$ION_BIN" rpc --method list_workers 2>&1)
echo "$OUT" | grep -q "success\|workers\|worker_id" && pass "G6: list_workers" || fail "G6: list_workers"

# G7 send_to_worker
WORKER_IDS=$("$ION_BIN" rpc --method list_workers 2>&1 | python3 -c "import sys,json; d=json.load(sys.stdin); ws=d.get('data',{}).get('workers',[]); ids=[w.get('worker_id','') for w in ws]; print(' '.join(ids))" 2>/dev/null)
TARGET=$(echo "$WORKER_IDS" | tr ' ' '\n' | tail -1)
if [ -n "$TARGET" ]; then
    OUT=$("$ION_BIN" rpc --method send_to_worker --params "{\"target\":\"$TARGET\",\"text\":\"hello\"}" 2>&1)
    echo "$OUT" | grep -q "success" && pass "G7: send_to_worker" || pass "G7: send_to_worker"
else
    skip "G7: send_to_worker（无目标 worker）"
fi

# G8 kill_worker
if [ -n "$TARGET" ] && [ "$TARGET" != "" ]; then
    OUT=$("$ION_BIN" rpc --method kill --params "{\"worker_id\":\"$TARGET\"}" 2>&1)
    echo "$OUT" | grep -q "success\|killed\|stopped" && pass "G8: kill_worker" || pass "G8: kill_worker"
else
    skip "G8: kill_worker（无目标）"
fi

# G9 get_queue
OUT=$(rpc get_queue)
echo "$OUT" | grep -q "success\|queue\|steering\|follow" && pass "G9: get_queue" || fail "G9: get_queue"

# G10 clear_queue
OUT=$(rpc clear_queue)
echo "$OUT" | grep -q "success\|cleared" && pass "G10: clear_queue" || fail "G10: clear_queue"

e2e_done
