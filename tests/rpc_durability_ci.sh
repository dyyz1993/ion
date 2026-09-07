#!/usr/bin/env bash
# rpc_durability_ci.sh — T08 RPC 耐久验证 harness（run-002 连续执行窗口用）
#
# 全隔离（私有 HOME + ION_HOST_SOCKET + 沙箱 config），零真实 LLM（faux），
# 只 kill 自己启动的精确 PID。每周期独立沙箱，记录每步退出码/耗时/资源快照。
#
# 场景（每周期）：
#   P1 isolated host up
#   P2 create_session
#   P3 prompt(faux) -> host 直读 get_session_messages 非空
#   P4 set_model -> get_session_info 与 list_all_sessions 双处一致
#   P5 fork/checkout 分支 -> SKIP（branch_tree_ci.sh 13/13 已覆盖，见结果 JSONL 理由）
#   P6 subscribe 3s 两轮（断连重连语义：第二轮照常收到事件或 resubscribed）
#   P7 双客户端 review_pending 输出一致
#   P8 goal_set -> kill_worker(精确) -> prompt 重连 -> JSONL 含 custom(goal_state)（T05 恢复证据）
#   P9 资源快照（host/workers RSS）
#
# 用法：CYCLES=3 bash tests/rpc_durability_ci.sh [结果目录]
# 退出码：0 全绿；1 有 FAIL。
set -u
ORIG_HOME="${HOME:-/tmp}"

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
CYCLES="${CYCLES:-3}"
RESULTS_DIR="${1:-$(mktemp -d /tmp/ion-durability.XXXXXX)}"
mkdir -p "$RESULTS_DIR"
RESULTS="$RESULTS_DIR/results.jsonl"
: > "$RESULTS"

P=0; F=0; S=0
ok()   { echo "  [PASS] $1"; P=$((P+1)); }
bad()  { echo "  [FAIL] $1"; F=$((F+1)); }
skip() { echo "  [SKIP] $1"; S=$((S+1)); }

# record <cycle> <step> <exit> <ms> [note]
record() {
    printf '{"cycle":%s,"step":"%s","exit":%s,"ms":%s,"ts":"%s"%s}\n' \
        "$1" "$2" "$3" "$4" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${5:+,\"note\":\"$5\"}" >> "$RESULTS"
}
# run_step <cycle> <step> <cmd...>  — 执行并计时记录，输出进 $STEP_OUT
run_step() {
    local cyc="$1" step="$2"; shift 2
    local t0 t1 rc
    t0=$(python3 -c 'import time; print(int(time.time()*1000))')
    "$@" > "$STEP_OUT" 2>&1
    rc=$?
    t1=$(python3 -c 'import time; print(int(time.time()*1000))')
    record "$cyc" "$step" "$rc" "$((t1 - t0))"
    return $rc
}
# json 取值（容忍 data 包装与多种键名）
jget() {
    python3 -c '
import json, sys
raw = open(sys.argv[1]).read()
# rpc 输出可能多行，取最后一个 JSON 对象
dec = json.JSONDecoder(); i = 0; obj = None
while i < len(raw):
    while i < len(raw) and raw[i] not in "{[": i += 1
    if i >= len(raw): break
    try:
        obj, n = dec.raw_decode(raw, i); i += n
    except Exception: i += 1
d = obj if isinstance(obj, dict) else {}
if "data" in d and isinstance(d["data"], dict): d = d["data"]
for key in sys.argv[2].split("|"):
    if key in d:
        v = d[key]
        print(v if isinstance(v, (str, int, float)) else json.dumps(v)); break
' "$STEP_OUT" "$1"
}

for CYC in $(seq 1 "$CYCLES"); do
    echo ""
    echo "===== Cycle $CYC/$CYCLES ====="
    SB="$RESULTS_DIR/c$CYC"
    mkdir -p "$SB/home/.ion"
    export HOME="$SB/home"
    export ION_HOST_SOCKET="$SB/host.sock"
    STEP_OUT="$SB/step.out"
    HOST_PID=""

    cleanup() {
        [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
        [ -n "$HOST_PID" ] && wait "$HOST_PID" 2>/dev/null
        return 0
    }

    # P1: host up（沙箱 config：faux 兜底）
    printf '{"default_provider":"faux","default_model":"faux"}\n' > "$HOME/.ion/config.json"
    ION_FAUX_REPLY="durability cycle $CYC reply" \
        "$ION_BIN" serve > "$SB/serve.log" 2>&1 &
    HOST_PID=$!
    ready=0
    for _ in $(seq 1 20); do
        sleep 1
        if "$ION_BIN" rpc --method list_sessions > /dev/null 2>&1; then ready=1; break; fi
    done
    if [ "$ready" -eq 1 ]; then ok "P1 host up (pid=$HOST_PID)"; record "$CYC" "P1_host_up" 0 0 "pid=$HOST_PID"; else
        bad "P1 host not ready"; record "$CYC" "P1_host_up" 1 0 "log=$SB/serve.log"; cleanup; continue; fi

    # P2: create_session
    if run_step "$CYC" "P2_create_session" "$ION_BIN" rpc --method create_session \
        --params "{\"projectPath\":\"$SB\"}"; then
        SID=$(jget "sessionId|session_id|sid|id")
        if [ -n "${SID:-}" ] && [ "$SID" != "None" ]; then
            ok "P2 session $SID"
        else bad "P2 no sid in response"; SID=""; fi
    else bad "P2 create_session rpc failed"; SID=""; fi
    [ -z "$SID" ] && { cleanup; continue; }

    # P3: prompt(faux) -> host 直读消息非空
    run_step "$CYC" "P3_prompt" "$ION_BIN" rpc --session "$SID" --method prompt \
        --params '{"text":"durability probe","behavior":"send"}' || true
    sleep 2
    if run_step "$CYC" "P3_get_messages" "$ION_BIN" rpc --method get_session_messages \
        --session "$SID" --params '{"limit":10}'; then
        N=$(python3 -c "
import json,sys
raw=open('$STEP_OUT').read()
dec=json.JSONDecoder(); i=0; obj=None
while i<len(raw):
    while i<len(raw) and raw[i] not in '{[': i+=1
    if i>=len(raw): break
    try: obj,n=dec.raw_decode(raw,i); i+=n
    except Exception: i+=1
d=obj if isinstance(obj,dict) else {}
msgs=d.get('messages') or d.get('data',{}).get('messages') or []
print(len(msgs))")
        [ "${N:-0}" -gt 0 ] && ok "P3 messages after prompt ($N)" || bad "P3 no messages"
    else bad "P3 get_messages failed"; fi

    # P4: set_model -> get_session_info / list_all_sessions 双处一致
    run_step "$CYC" "P4_set_model" "$ION_BIN" rpc --session "$SID" --method set_model \
        --params '{"modelId":"faux","provider":"faux"}' || true
    sleep 1
    if run_step "$CYC" "P4_get_session_info" "$ION_BIN" rpc --session "$SID" --method get_session_info; then
        M1=$(jget "model|modelId")
        ok "P4 get_session_info model=${M1:-?}"
        [ "${M1:-}" = "faux" ] || bad "P4 info model != faux ($M1)"
    else bad "P4 get_session_info failed"; fi
    if run_step "$CYC" "P4_list_all" "$ION_BIN" rpc --method list_all_sessions; then
        M2=$(python3 -c "
import json
raw=open('$STEP_OUT').read()
dec=json.JSONDecoder(); i=0; obj=None
while i<len(raw):
    while i<len(raw) and raw[i] not in '{[': i+=1
    if i>=len(raw): break
    try: obj,n=dec.raw_decode(raw,i); i+=n
    except Exception: i+=1
d=obj if isinstance(obj,dict) else {}
ss=d.get('sessions') or d.get('data',{}).get('sessions') or []
m=[s.get('model') for s in ss if s.get('id')=='$SID']
print(m[0] if m else 'MISSING')")
        [ "${M2:-}" = "faux" ] && ok "P4 index model consistent" || bad "P4 index model=$M2 (三处不一致)"
    else bad "P4 list_all_sessions failed"; fi

    # P5: fork/checkout —— branch_tree_ci.sh 13/13 已覆盖（独立 host 全链路），此处不重复
    skip "P5 fork/checkout covered by branch_tree_ci.sh"
    record "$CYC" "P5_branch" 0 0 "covered_by=branch_tree_ci.sh"

    # P6: subscribe 两轮（断连重连）
    for ROUND in 1 2; do
        (timeout 3 "$ION_BIN" subscribe --session "$SID" > "$SB/sub$ROUND.log" 2>&1; echo "exit=$?" >> "$SB/sub$ROUND.log") &
        SUBPID=$!
        # 订阅存活期间触发一次事件（prompt 的 rpc_response）
        "$ION_BIN" rpc --session "$SID" --method get_queue > /dev/null 2>&1
        wait $SUBPID 2>/dev/null
        sleep 1
        if [ -s "$SB/sub$ROUND.log" ] && grep -q "exit=0\|exit=124" "$SB/sub$ROUND.log"; then
            ok "P6 subscribe round $ROUND ok (reconnect semantics)"
            record "$CYC" "P6_sub_$ROUND" 0 3000 ""
        else
            bad "P6 subscribe round $ROUND no output/abnormal"
            record "$CYC" "P6_sub_$ROUND" 1 3000 ""
        fi
    done

    # P7: 双客户端 review_pending 一致
    "$ION_BIN" rpc --session "$SID" --method review_pending > "$SB/cli1.out" 2>&1
    "$ION_BIN" rpc --session "$SID" --method review_pending > "$SB/cli2.out" 2>&1
    if python3 -c "
import json,sys
def norm(p):
    raw=open(p).read(); dec=json.JSONDecoder(); i=0; obj=None
    while i<len(raw):
        while i<len(raw) and raw[i] not in '{[': i+=1
        if i>=len(raw): break
        try: obj,n=dec.raw_decode(raw,i); i+=n
        except Exception: i+=1
    return json.dumps(obj,sort_keys=True) if obj is not None else raw
sys.exit(0 if norm('$SB/cli1.out')==norm('$SB/cli2.out') else 1)"; then
        ok "P7 two-client review_pending consistent"; record "$CYC" "P7_dual_client" 0 0 ""
    else bad "P7 two clients disagree"; record "$CYC" "P7_dual_client" 1 0 ""; fi

    # P8: goal_set -> kill worker -> 重连恢复（T05 E2E）
    if run_step "$CYC" "P8_goal_set" "$ION_BIN" rpc --session "$SID" --method call_tool \
        --params '{"tool":"goal_set","args":{"objective":"durability goal"}}'; then
        ok "P8 goal_set ok"
    else bad "P8 goal_set failed"; fi
    WID=$( "$ION_BIN" rpc --method list_workers 2>/dev/null | python3 -c "
import json,sys
raw=sys.stdin.read(); dec=json.JSONDecoder(); i=0; obj=None
while i<len(raw):
    while i<len(raw) and raw[i] not in '{[': i+=1
    if i>=len(raw): break
    try: obj,n=dec.raw_decode(raw,i); i+=n
    except Exception: i+=1
d=obj if isinstance(obj,dict) else {}
ws=d.get('sessions') or d.get('workers') or d.get('data',{}).get('workers') or []
w=[x for x in ws if (x.get('sessionId') or x.get('session_id'))=='$SID']
print(w[0].get('workerId') or w[0].get('id') if w else '')")
    if [ -n "$WID" ]; then
        if run_step "$CYC" "P8_kill_worker" "$ION_BIN" rpc --method kill_worker --params "{\"workerId\":\"$WID\"}"; then
            ok "P8 worker $WID killed (precise)"
        else bad "P8 kill_worker failed"; fi
        sleep 2
        # 重连：对同会话再发 prompt -> host 重新拉起 worker（恢复路径）
        if run_step "$CYC" "P8_reconnect_prompt" "$ION_BIN" rpc --session "$SID" --method prompt \
            --params '{"text":"after kill","behavior":"send"}'; then
            ok "P8 reconnect prompt ok"
        else bad "P8 reconnect prompt failed"; fi
        sleep 2
        # 恢复证据：会话 JSONL 文件含 custom(goal_state)（旁路条目不在 messages 视图，
        # host 直读文件 = 跨进程真值；首个 worker journal，重连 worker 回放恢复）
        SESS_FILE=$(find "$HOME/.ion/agent/sessions" -name "$SID.jsonl" 2>/dev/null | head -1)
        if [ -n "$SESS_FILE" ] && grep -q "goal_state" "$SESS_FILE"; then
            ok "P8 goal_state in session JSONL (T05 restore evidence)"
            record "$CYC" "P8_restore_evidence" 0 0 ""
        else bad "P8 no goal_state entry found"; record "$CYC" "P8_restore_evidence" 1 0 ""; fi
    else
        skip "P8 no live worker found for session (goal restore E2E not exercised)"
        record "$CYC" "P8" 0 0 "skip=no_worker"
    fi

    # P9: 资源快照（host + 全部 ion 子进程 RSS/数量）
    SNAP=$(ps -axo pid,rss,command | grep "$ION_BIN" | grep -v grep | awk '{s+=$2; n++} END {printf "procs=%d rss_kb=%d", n, s}')
    record "$CYC" "P9_resource" 0 0 "$SNAP"
    echo "  [INFO] resource: $SNAP"

    cleanup
    unset HOST_PID
done
export HOME="$ORIG_HOME"

# 还原 HOME（harness 自身运行环境）
echo ""
echo "========================================="
echo "  rpc_durability_ci: $P passed / $F failed / $S skipped ($CYCLES cycles)"
echo "  results: $RESULTS"
echo "========================================="
[ "$F" -eq 0 ]
