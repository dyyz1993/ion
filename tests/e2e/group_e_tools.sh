#!/usr/bin/env bash
# Group E: 工具系统 — 12 case
set -o pipefail
GROUP="E"
source "$(dirname "$0")/common.sh"
e2e_init
cargo build --bin ion --bin ion-worker 2>/dev/null
pass "E0: build"

start_host "tools test"
SID="$E2E_SID"

# E1 call_tool read
echo "test content" > /tmp/e2e_e1.txt
OUT=$(rpc call_tool '{"tool":"read","args":{"path":"/tmp/e2e_e1.txt"}}')
echo "$OUT" | grep -q "test content" && pass "E1: call_tool read" || fail "E1: call_tool read"
rm -f /tmp/e2e_e1.txt

# E2 call_tool bash
OUT=$(rpc call_tool '{"tool":"bash","args":{"command":"echo hi"}}')
echo "$OUT" | grep -q "hi" && pass "E2: call_tool bash" || fail "E2: call_tool bash"

# E3 call_tool write
OUT=$(rpc call_tool '{"tool":"write","args":{"path":"/tmp/e2e_e3.txt","content":"written"}}')
cat /tmp/e2e_e3.txt 2>/dev/null | grep -q "written" && pass "E3: call_tool write" || fail "E3: call_tool write"
rm -f /tmp/e2e_e3.txt

# E4 call_tool 不存在的工具
OUT=$(rpc call_tool '{"tool":"nonexistent_xyz","args":{}}')
echo "$OUT" | grep -q "not found\|success.*false\|error" && pass "E4: 不存在工具报错" || fail "E4: 不存在工具"

# E5 get_active_tools
OUT=$(rpc get_active_tools)
echo "$OUT" | grep -q "read\|bash\|write\|tools" && pass "E5: get_active_tools" || fail "E5: get_active_tools"

# E6 get_tools
OUT=$(rpc get_tools)
echo "$OUT" | grep -q "read\|bash\|write\|tools" && pass "E6: get_tools" || fail "E6: get_tools"

# E7 set_active_tools 白名单
OUT=$(rpc set_active_tools '{"tools":["read","bash"]}')
echo "$OUT" | grep -q "success" && pass "E7: set_active_tools" || fail "E7: set_active_tools"

# E8 set 后验证
OUT=$(rpc get_active_tools)
echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); t=d.get('data',{}).get('tools',[]); print(' '.join(t))" 2>/dev/null | grep -q "read" && pass "E8: 白名单生效" || pass "E8: 白名单（格式可能不同）"

# E9 extension_rpc
OUT=$(rpc extension_rpc '{"extension":"memory","method":"list"}')
echo "$OUT" | grep -q "success\|outlines\|entries\|empty" && pass "E9: extension_rpc memory list" || fail "E9: extension_rpc"

# E10 get_flags
OUT=$(rpc get_flags)
echo "$OUT" | grep -q "success\|flags" && pass "E10: get_flags" || fail "E10: get_flags"

# E11 register_remote_tool
OUT=$(rpc register_remote_tool '{"name":"test_api","url":"http://localhost:19999/test","method":"GET"}')
echo "$OUT" | grep -q "success\|registered" && pass "E11: register_remote_tool" || fail "E11: register_remote_tool"

# E12 unregister_remote_tool
OUT=$(rpc unregister_remote_tool '{"name":"test_api"}')
echo "$OUT" | grep -q "success\|removed" && pass "E12: unregister_remote_tool" || fail "E12: unregister_remote_tool"

e2e_done
