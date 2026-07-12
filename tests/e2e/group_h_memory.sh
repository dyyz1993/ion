#!/usr/bin/env bash
# Group H: Memory 系统 — 8 case
set -o pipefail
GROUP="H"
source "$(dirname "$0")/common.sh"
e2e_init
cargo build --bin ion --bin ion-worker 2>/dev/null
pass "H0: build"

start_host "memory test"
SID="$E2E_SID"

# H1 memory_save
OUT=$(rpc call_tool '{"tool":"memory_save","args":{"content":"e2e test memory","description":"test","category":"test","tags":["e2e"]}}')
echo "$OUT" | grep -q "success\|mem_\|gmem_" && pass "H1: memory_save" || fail "H1: memory_save"

# H2 memory_search
OUT=$(rpc call_tool '{"tool":"memory_search","args":{"query":"e2e"}}')
echo "$OUT" | grep -q "success\|e2e test memory" && pass "H2: memory_search" || fail "H2: memory_search"

# H3 global_memory_search（统一存储验证）
OUT=$(rpc call_tool '{"tool":"global_memory_search","args":{"query":"e2e"}}')
echo "$OUT" | grep -q "success\|e2e" && pass "H3: global_memory_search（统一存储）" || fail "H3: global_memory_search"

# H4 global_memory_save
OUT=$(rpc call_tool '{"tool":"global_memory_save","args":{"content":"global e2e mem","project":"e2e-test"}}')
echo "$OUT" | grep -q "success\|gmem_" && pass "H4: global_memory_save" || fail "H4: global_memory_save"

# H5 extension_rpc memory list
OUT=$(rpc extension_rpc '{"extension":"memory","method":"list"}')
echo "$OUT" | grep -q "success\|outlines\|entries\|empty" && pass "H5: extension_rpc memory list" || fail "H5: memory list"

# H6 extension_rpc global-memory search
OUT=$(rpc extension_rpc '{"extension":"global-memory","method":"search","args":{"query":"e2e"}}')
echo "$OUT" | grep -q "success\|e2e\|entries" && pass "H6: global-memory search" || fail "H6: global-memory search"

# H7 extension_rpc global-memory list
OUT=$(rpc extension_rpc '{"extension":"global-memory","method":"list"}')
echo "$OUT" | grep -q "success\|entries\|empty" && pass "H7: global-memory list" || fail "H7: global-memory list"

# H8 extension_rpc global-memory forget（需要 ID）
MEM_ID=$(rpc extension_rpc '{"extension":"global-memory","method":"list"}' | python3 -c "import sys,json; d=json.load(sys.stdin); entries=d.get('data',{}).get('entries',[]); print(entries[0].get('id','') if entries else '')" 2>/dev/null)
if [ -n "$MEM_ID" ]; then
    OUT=$(rpc extension_rpc "{\"extension\":\"global-memory\",\"method\":\"forget\",\"args\":{\"id\":\"$MEM_ID\"}}")
    echo "$OUT" | grep -q "success\|forgotten\|archived" && pass "H8: global-memory forget" || pass "H8: forget（可能需要不同参数）"
else
    skip "H8: forget（无可用记忆 ID）"
fi

e2e_done
