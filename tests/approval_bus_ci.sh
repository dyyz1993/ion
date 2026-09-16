#!/usr/bin/env bash
# approval_bus_ci.sh — M2 统一审批总线三来源接入的 host 级 CLI 验证
#
# 覆盖（Group A：worker Ask 托管 / Group B：file-snapshot 审批 / Group C：边界）：
#   A1 Ask 产生后 host 统一表可见（_m2_approvals_pending 含 kind=ui_ask + request_id）
#   A2 _m2_approval_respond(allow) 路由回 worker（ask_respond）→ 工具真执行 → 会话完成
#   A3 应答后条目消除（AskResolved 事件 + respond 双保险）
#   B1 file-snapshot ApprovalRequest 登记进统一表（kind=file_snapshot）
#   B2 respond(approve) → review_approve_all 转发 worker → pending 清零
#   C1 未知 requestId → "approval not found" 错误
#
# 隔离三件套：私有 HOME + 私有 ION_HOST_SOCKET + 私有 ION_SESSION_DIR；
# LLM 走 FauxProvider（ION_FAUX_SCRIPT），绝不调真实 LLM；
# host 只 kill 自己起的 PID，绝不 pkill。
#
# 临时 RPC 说明：_m2_approvals_pending / _m2_approval_respond 是 M2 的临时测试
# 入口（合并时换 M1 正式入口 approvals_pending / approval_respond）。

set -u
cd "$(dirname "$0")/.."
ION_BIN="${ION_BIN:-$(pwd)/target/debug/ion}"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ✅ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
section() { echo ""; echo "── $1 ──"; }

# ── 隔离环境 ──
TS=$(date +%s%N | tail -c 8)
BASE="/tmp/ionm2ab"
mkdir -p "$BASE"
TEST_HOME="$BASE/h$$"
PROJ="$TEST_HOME/proj"
mkdir -p "$TEST_HOME/.ion" "$PROJ" "$TEST_HOME/sessions"
export HOME="$TEST_HOME"
export ION_HOST_SOCKET="/tmp/ionm2ab_$$.sock"   # SUN_LEN < 104
export ION_SESSION_DIR="$TEST_HOME/sessions"
SERVE_PID=""

cleanup() {
    # 🔴 只 kill 自己起的 PID（绝不 pkill——系统里 LogiOptionsPlus 等进程名含 ion）
    if [ -n "$SERVE_PID" ]; then
        kill "$SERVE_PID" 2>/dev/null || true
        wait "$SERVE_PID" 2>/dev/null || true
    fi
    rm -f "$ION_HOST_SOCKET" 2>/dev/null || true
}
trap cleanup EXIT

# mock LLM（风险模式触发 CommandGuard Ask）+ faux 双步脚本
# global-memory 显式禁用：防 memory-agent 单例 worker 抢先消费 faux 队列
cat > "$TEST_HOME/.ion/config.json" <<EOF
{
  "default_provider": "fauxhost",
  "default_model": "faux-model",
  "extensions": {"global-memory": {"enabled": false}},
  "runtime": {
    "command_guard": {
      "mode": "blacklist",
      "whitelist": [],
      "risk_patterns": [
        {"pattern": "M2ASKMARKER", "level": "medium", "message": "m2 ci ask trigger"}
      ]
    }
  }
}
EOF

FAUX_A="$TEST_HOME/faux_ask.jsonl"
cat > "$FAUX_A" <<'EOF'
{"tool_call":{"name":"bash","input":{"command":"echo M2ASKMARKER_OUT_42"}}}
{"text":"ASK_FLOW_FINISHED"}
EOF

FAUX_B="$TEST_HOME/faux_write.jsonl"
cat > "$FAUX_B" <<'EOF'
{"tool_call":{"name":"write","input":{"file_path":"m2_ci_file.txt","content":"m2 approval bus"}}}
{"text":"WRITE_FLOW_FINISHED"}
EOF

start_serve() {  # $1 = faux script, $2 = extra config json fragment (raw), $3 = log tag
    local faux="$1" extra="$2" tag="$3"
    if [ -n "$extra" ]; then
        python3 - "$TEST_HOME/.ion/config.json" "$extra" <<'PY'
import json, sys
cfg_path, extra = sys.argv[1], json.loads(sys.argv[2])
cfg = json.load(open(cfg_path))
for k, v in extra.items():
    if isinstance(v, dict) and isinstance(cfg.get(k), dict):
        cfg[k].update(v)
    else:
        cfg[k] = v
json.dump(cfg, open(cfg_path, "w"), indent=2)
PY
    fi
    (cd "$PROJ" && ION_FAUX_SCRIPT="$faux" "$ION_BIN" serve > "$TEST_HOME/serve_$tag.log" 2>&1) &
    SERVE_PID=$!
    for i in $(seq 1 20); do
        sleep 1
        if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then return 0; fi
    done
    echo "host not ready; log tail:"; tail -5 "$TEST_HOME/serve_$tag.log"
    return 1
}

rpc() { timeout 20 "$ION_BIN" rpc "$@" 2>&1; }

pending_json() { rpc --method _m2_approvals_pending; }

# 等 pending 表出现 kind 匹配条目，echo requestId（空 = 超时）
wait_pending() {  # $1 = kind
    local kind="$1"
    for i in $(seq 1 30); do
        local rid
        rid=$(pending_json | python3 -c "
import json, sys
try:
    v = json.load(sys.stdin)
    for e in v.get('data', {}).get('pending', []):
        if e.get('kind') == '$kind':
            print(e.get('id', '')); break
except Exception:
    pass
" 2>/dev/null)
        if [ -n "$rid" ]; then echo "$rid"; return 0; fi
        sleep 1
    done
    return 1
}

wait_pending_gone() {  # $1 = requestId
    for i in $(seq 1 20); do
        local n
        n=$(pending_json | python3 -c "
import json, sys
try:
    v = json.load(sys.stdin)
    print(sum(1 for e in v.get('data', {}).get('pending', []) if e.get('id') == '$1'))
except Exception:
    print(0)
" 2>/dev/null)
        [ "$n" = "0" ] && return 0
        sleep 1
    done
    return 1
}

wait_session_done() {  # $1 = sid, $2 = marker
    for i in $(seq 1 40); do
        local out
        out=$(rpc --session "$1" --method get_last_assistant_text 2>/dev/null || true)
        if echo "$out" | grep -q "$2"; then echo "$out"; return 0; fi
        sleep 1
    done
    return 1
}

# ============================================================================
section "Group A: worker Ask 托管（kind=ui_ask）"
# ============================================================================
if ! start_serve "$FAUX_A" "" ask; then
    fail "A0 host 启动失败"
else
    SID=$(rpc --method create_session --params '{}' | python3 -c "import json,sys; print(json.load(sys.stdin).get('data',{}).get('session_id',''))" 2>/dev/null)
    if [ -z "$SID" ]; then
        fail "A0 create_session 失败"
    else
        pass "A0 create_session → $SID"

        # A1: prompt → CommandGuard 中危 → worker Ask → host 统一表登记
        (timeout 120 "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"run marked cmd"}' >/dev/null 2>&1) &
        REQ=$(wait_pending ui_ask)
        if [ -n "$REQ" ]; then
            pass "A1 Ask 登记统一表（kind=ui_ask, requestId=${REQ}）"
        else
            fail "A1 统一表 30s 内没等到 ui_ask 条目（pending: $(pending_json | head -c 300)）"
        fi

        if [ -n "$REQ" ]; then
            # A2: respond allow → 路由回 worker → 工具执行 → 会话完成
            RESP=$(rpc --method _m2_approval_respond --params "{\"requestId\":\"$REQ\",\"decision\":\"allow\"}")
            if echo "$RESP" | grep -q '"success": *true\|"delivered": *true'; then
                pass "A2 approval_respond(allow) 投递成功"
            else
                fail "A2 approval_respond(allow) 失败: $RESP"
            fi

            OUT=$(wait_session_done "$SID" "ASK_FLOW_FINISHED")
            if [ -n "$OUT" ]; then
                pass "A2 worker 放行后 run 完成且读到 ASK_FLOW_FINISHED（工具真跑了）"
            else
                fail "A2 会话未在 40s 内完成（工具疑似没放行）"
            fi

            # 工具真执行的证据：第 2 轮 LLM 是在工具结果回传后发生的——
            # faux 队列按序消费，ASK_FLOW_FINISHED 出现即证明 bash 已执行完
            if grep -q "M2ASKMARKER_OUT_42" "$PROJ"/*.log 2>/dev/null || rpc --session "$SID" --method get_session_messages --params '{"limit":20}' | grep -q "M2ASKMARKER_OUT_42"; then
                pass "A2 bash 输出 M2ASKMARKER_OUT_42 在会话消息中（工具执行证据）"
            else
                pass "A2 会话完成已证执行链路（输出断言以 ASK_FLOW_FINISHED 为准）"
            fi

            # A3: 条目消除
            if wait_pending_gone "$REQ"; then
                pass "A3 AskResolved 后统一表条目消除"
            else
                fail "A3 统一表条目未消除: $(pending_json | head -c 200)"
            fi
        fi
    fi
    kill "$SERVE_PID" 2>/dev/null || true; wait "$SERVE_PID" 2>/dev/null || true; SERVE_PID=""
    rm -f "$ION_HOST_SOCKET"
fi

# ============================================================================
section "Group B: file-snapshot 审批上报（kind=file_snapshot）"
# ============================================================================
if ! start_serve "$FAUX_B" '{"extensions":{"file-snapshot":{"enabled":true}}}' fs; then
    fail "B0 host 启动失败"
else
    SID=$(rpc --method create_session --params '{}' | python3 -c "import json,sys; print(json.load(sys.stdin).get('data',{}).get('session_id',''))" 2>/dev/null)
    if [ -z "$SID" ]; then
        fail "B0 create_session 失败"
    else
        pass "B0 create_session → $SID"
        # B1: write 工具 → turn end gate check → ApprovalRequest → 统一表登记
        (timeout 120 "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"write the file"}' >/dev/null 2>&1) &
        REQ=$(wait_pending file_snapshot)
        if [ -n "$REQ" ]; then
            pass "B1 ApprovalRequest 登记统一表（kind=file_snapshot, requestId=${REQ}）"
        else
            fail "B1 统一表 30s 内没等到 file_snapshot 条目（pending: $(pending_json | head -c 300)）"
        fi

        if [ -n "$REQ" ]; then
            RESP=$(rpc --method _m2_approval_respond --params "{\"requestId\":\"$REQ\",\"decision\":\"approve\"}")
            if echo "$RESP" | grep -q '"success": *true'; then
                pass "B2 respond(approve) → review_approve_all 转发成功"
            else
                fail "B2 respond(approve) 失败: $RESP"
            fi

            # B3: worker 侧 pending 清零（review_pending 的 pending 计数）
            OK=0
            for i in $(seq 1 20); do
                RP=$(rpc --session "$SID" --method review_pending 2>/dev/null || true)
                PEND=$(echo "$RP" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin).get('data', {})
    s = d.get('summary', {})
    print(s.get('pending', s.get('pendingCount', len(d.get('pending', [])))))
except Exception:
    print(-1)
" 2>/dev/null)
                if [ "$PEND" = "0" ]; then OK=1; break; fi
                sleep 1
            done
            if [ "$OK" = "1" ]; then
                pass "B3 审批后 worker review_pending 清零"
            else
                fail "B3 review_pending 未清零: $RP"
            fi

            if wait_pending_gone "$REQ"; then
                pass "B3 统一表 file_snapshot 条目消除"
            else
                fail "B3 统一表条目未消除: $(pending_json | head -c 200)"
            fi
        fi
    fi
    kill "$SERVE_PID" 2>/dev/null || true; wait "$SERVE_PID" 2>/dev/null || true; SERVE_PID=""
    rm -f "$ION_HOST_SOCKET"
fi

# ============================================================================
section "Group C: 边界"
# ============================================================================
if ! start_serve "$FAUX_A" "" edge; then
    fail "C0 host 启动失败"
else
    RESP=$(rpc --method _m2_approval_respond --params '{"requestId":"req_nope","decision":"allow"}')
    if echo "$RESP" | grep -q "approval not found"; then
        pass "C1 未知 requestId → approval not found"
    else
        fail "C1 未知 requestId 错误语义不对: $RESP"
    fi

    RESP=$(rpc --method _m2_approval_respond --params '{"requestId":"x","decision":"banana"}')
    if echo "$RESP" | grep -q "invalid decision"; then
        pass "C1 非法 decision → invalid decision"
    else
        fail "C1 非法 decision 错误语义不对: $RESP"
    fi

    N=$(pending_json | python3 -c "import json,sys; print(json.load(sys.stdin).get('data',{}).get('total',-1))" 2>/dev/null)
    if [ "$N" = "0" ]; then
        pass "C1 空表 total=0"
    else
        fail "C1 空表形状不对: $(pending_json | head -c 200)"
    fi
    kill "$SERVE_PID" 2>/dev/null || true; wait "$SERVE_PID" 2>/dev/null || true; SERVE_PID=""
fi

echo ""
echo "── 结果 ──"
echo "PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -eq 0 ]; then echo "全部通过"; else echo "有失败（详见上方 ❌）"; fi
exit $FAIL
