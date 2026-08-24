#!/usr/bin/env bash
# rpc_event_push CI — 用户触发的每条 RPC 都推送事件（多终端实时同步）
#
# 验证两层推送：
#   1. 通用 rpc_response：worker 处理的每条用户 RPC（成功/失败/未知命令）都广播
#      一条 {"type":"rpc_response","method":...,"success":...,"sessionId":...} 事件
#   2. 类型化 permission_changed：权限规则变更（store/remove/clear stored decision）
#      额外广播带明细的事件；查询类不广播
#
# 场景：ion serve（场景 3）+ 两个终端同时 subscribe —— 证明多窗口实时同步。
# 断言用 jq -s 解析订阅流的 JSON 对象序列（subscribe 输出是 pretty JSON，不能 grep 紧凑格式）。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"

source "$(dirname "$0")/ci_host_helper.sh"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }

TEST_DIR="$(mktemp -d /tmp/ion-rpc-event-push-XXXXXX)"
mkdir -p "$TEST_DIR/proj"
printf '# rpc event push ci\n' > "$TEST_DIR/proj/README.md"

rpc() { "$ION_BIN" rpc --method "$1" --params "$2"; }
wrpc() { "$ION_BIN" rpc --session "$SID" --method "$1" --params "$2"; }

# 订阅日志是 pretty-JSON 对象序列 → jq -s 聚合成数组再过滤
evcount() { # evcount <file> <jq-select-expr>
    jq -s "[.[] | select($2)] | length" "$1" 2>/dev/null || echo 0
}
evhas() { [ "$(evcount "$1" "$2")" -gt 0 ]; }
# 等待事件到达（异步转发，最多 5s）
wait_ev() { # wait_ev <file> <jq-select-expr>
    for _ in $(seq 1 25); do
        evhas "$1" "$2" && return 0
        sleep 0.2
    done
    return 1
}

ensure_host || { echo "host 启动失败"; exit 1; }

echo; echo "=== 准备：创建 worker + 两个终端订阅 ==="
CREATE_OUT=$(rpc create_worker "{
  \"relation\":\"child\",
  \"creator\":\"rpc-event-push-ci\",
  \"project_path\":\"$TEST_DIR/proj\",
  \"initial_prompt\":\"CI 事件推送验证\"
}")
SID=$(echo "$CREATE_OUT" | jq -r '.data.sessionId // empty')
if [ -n "$SID" ]; then pass "create_worker 返回 sessionId ($SID)"; else fail "create_worker 返回 sessionId"; fi

"$ION_BIN" subscribe --session "$SID" > "$TEST_DIR/term1.log" 2>&1 &
SUB1=$!
"$ION_BIN" subscribe --session "$SID" > "$TEST_DIR/term2.log" 2>&1 &
SUB2=$!
sleep 1

echo; echo "=== Group A: 通用 rpc_response（每条用户 RPC 都推送）==="
wrpc get_messages '{}' >/dev/null 2>&1
wait_ev "$TEST_DIR/term1.log" '.event.type=="rpc_response" and .event.method=="get_messages"'
check $? "查询类 RPC（get_messages）也推送 rpc_response"
evhas "$TEST_DIR/term1.log" '.event.type=="rpc_response" and .event.method=="get_messages" and .event.success==true'
check $? "成功 RPC 事件 success=true"
evhas "$TEST_DIR/term1.log" '.event.type=="rpc_response" and .event.method=="get_messages" and (.event.sessionId|length>0)'
check $? "事件带 sessionId"

wrpc definitely_not_a_real_method '{}' >/dev/null 2>&1
wait_ev "$TEST_DIR/term1.log" '.event.type=="rpc_response" and .event.method=="definitely_not_a_real_method"'
check $? "未知命令也推送 rpc_response（失败路径不漏）"
evhas "$TEST_DIR/term1.log" '.event.type=="rpc_response" and .event.method=="definitely_not_a_real_method" and .event.success==false'
check $? "未知命令事件 success=false"
evhas "$TEST_DIR/term1.log" '.event.type=="rpc_response" and .event.method=="definitely_not_a_real_method" and ((.event.error // "") | test("Unknown command"))'
check $? "失败事件带 error 文本"

echo; echo "=== Group B: permission_changed（权限变更类型化事件）==="
STORE_OUT=$(wrpc permission_store_decision '{
  "subject":"command.run","pattern":"git status*","decision":"allow","scope":"session"
}')
echo "$STORE_OUT" | jq -e '.data.data.status == "ok"' >/dev/null
check $? "store_decision RPC 成功"
wait_ev "$TEST_DIR/term1.log" '.event.customType=="permission_changed" and .event.data.action=="decision_stored"'
check $? "store 推送 permission_changed(action=decision_stored)"
evhas "$TEST_DIR/term1.log" '.event.customType=="permission_changed" and .event.data.action=="decision_stored" and .event.data.detail.pattern=="git status*"'
check $? "事件带规则明细（pattern）"

STORED_ID=$(echo "$STORE_OUT" | grep -o 'perm_stored_[a-f0-9]*' | head -1)
if [ -n "$STORED_ID" ]; then pass "从响应提取规则 id ($STORED_ID)"; else fail "从响应提取规则 id"; fi

wrpc permission_remove_stored "{\"id\":\"$STORED_ID\"}" >/dev/null 2>&1
wait_ev "$TEST_DIR/term1.log" '.event.customType=="permission_changed" and .event.data.action=="stored_removed"'
check $? "remove 推送 permission_changed(action=stored_removed)"

wrpc permission_store_decision '{
  "subject":"file.write","pattern":"src/*","decision":"allow","scope":"session"
}' >/dev/null 2>&1
wrpc permission_clear_stored '{}' >/dev/null 2>&1
wait_ev "$TEST_DIR/term1.log" '.event.customType=="permission_changed" and .event.data.action=="stored_cleared"'
check $? "clear 推送 permission_changed(action=stored_cleared)"

N_PERM=$(evcount "$TEST_DIR/term1.log" '.event.customType=="permission_changed"')
[ "$N_PERM" -eq 4 ]
check $? "permission_changed 恰好 4 条: stored+removed+stored+cleared (actual=$N_PERM)"

wrpc permission_list_stored '{}' >/dev/null 2>&1
sleep 1
N_PERM2=$(evcount "$TEST_DIR/term1.log" '.event.customType=="permission_changed"')
[ "$N_PERM2" -eq 4 ]
check $? "查询类（list_stored）不产生 permission_changed (still=$N_PERM2)"

echo; echo "=== Group C: 多终端同时实时收到 ==="
wrpc get_messages '{}' >/dev/null 2>&1
wait_ev "$TEST_DIR/term2.log" '.event.type=="rpc_response" and .event.method=="get_messages"'
check $? "第二个终端（term2）同样收到 rpc_response"
evhas "$TEST_DIR/term2.log" '.event.type=="rpc_response" and .event.method=="permission_store_decision"'
check $? "term2 也收到了权限变更 RPC 的事件"
N2=$(evcount "$TEST_DIR/term2.log" '.event.customType=="permission_changed"')
[ "$N2" -eq 4 ]
check $? "term2 的 permission_changed 也是 4 条 (actual=$N2)"

kill "$SUB1" "$SUB2" 2>/dev/null || true
rm -rf "$TEST_DIR"

echo
printf '=== 结果: %d passed / %d failed ===\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
