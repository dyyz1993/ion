#!/usr/bin/env bash
# rpc_schema_subscribe CI — S3 域（订阅/事件流/host 协议）schema 契约的 raw socket 验证
#
# 与 tests/rpc_schema_subscribe_test.rs（Rust 侧 jsonschema 编译+动态验证）互补：
# 本脚本用 jq 做结构断言，验证真实 host 的帧形状与 schemas/rpc/subscribe/ 契约一致。
#
# 覆盖（对标 docs/design/SUBSCRIBE_PROTOCOL.md + schemas/rpc/subscribe/）：
#   A schema 资产：schemas/rpc/subscribe/ 全部 JSON 合法 + draft 2020-12 + event.type 枚举 19 种
#   B hello 握手帧形状
#   C subscribe → subscribed ack(epoch) → snapshot 帧 → 实时 instance_event 帧
#   D stale_route 帧（epoch 栅栏）+ 重派后新 epoch
#   E ui_respond 同源绑定两种响应帧
#   F host 级查询：list_sessions / get_overview / verb_pending / verb_review
#   G subscribe_overview ack(initial) + overview_snapshot 帧
#
# 隔离铁律：私有 HOME + 私有 ION_HOST_SOCKET + 私有 ION_SESSION_DIR，绝不碰真实 ~/.ion；
# 只 kill 自己启动的精确 PID（绝不 pkill）。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
SCHEMA_DIR="$PROJECT_DIR/schemas/rpc/subscribe"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

# ── 隔离三件套 ──
TEST_DIR="$(mktemp -d /tmp/ion-rpc-schema-sub-XXXXXX)"
export HOME="$TEST_DIR/home"
mkdir -p "$HOME" "$TEST_DIR/proj"
export ION_HOST_SOCKET="$TEST_DIR/host.sock"
export ION_SESSION_DIR="$TEST_DIR/sessions"
printf '# rpc schema subscribe ci\n' > "$TEST_DIR/proj/README.md"
export ION_FAUX_REPLY="${ION_FAUX_REPLY:-rpc schema subscribe ci ready}"
# 静态响应队列只有 1 条；REPEAT=1 让空队列时重复最后一条，避免后续 prompt 全进 auto_retry
export ION_FAUX_REPEAT="${ION_FAUX_REPEAT:-1}"

HOST_PID=""
cleanup() {
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && wait "$HOST_PID" 2>/dev/null
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

rpc() { local p="${2:-}"; [ -z "$p" ] && p='{}'; "$ION_BIN" rpc --method "$1" --params "$p"; }

# ── raw socket 工具（与 subscribe_protocol_ci.sh 同型）──
sock_send_read() { # sock_send_read <socket> <send_line> <timeout_s> <max_lines> → 每行一个 JSON
    SOCK_="$1" SEND_="$2" TMO_="$3" MAX_="$4" python3 -c '
import socket, json, os, time
s = socket.socket(socket.AF_UNIX); s.settimeout(8)
s.connect(os.environ["SOCK_"])
s.sendall((os.environ["SEND_"] + "\n").encode())
buf = b""; lines = []
deadline = time.time() + float(os.environ["TMO_"])
maxn = int(os.environ["MAX_"])
while len(lines) < maxn and time.time() < deadline:
    s.settimeout(max(0.05, deadline - time.time()))
    try:
        d = s.recv(65536)
    except socket.timeout:
        break
    if not d: break
    buf += d
    while b"\n" in buf and len(lines) < maxn:
        raw, buf = buf.split(b"\n", 1)
        t = raw.decode(errors="replace").strip()
        if t: lines.append(t)
s.close()
for l in lines: print(l)
'
}
evjson() { printf '%s' "$1" | jq -r "$2" 2>/dev/null || echo ""; }

echo "=== A schema 资产静态检查 ==="
N_TOTAL=0; N_DRAFT=0; N_INVALID=0
for f in $(find "$SCHEMA_DIR" -name "*.json" | sort); do
    N_TOTAL=$((N_TOTAL + 1))
    if ! python3 -c "import json,sys; json.load(open('$f'))" 2>/dev/null; then
        N_INVALID=$((N_INVALID + 1)); continue
    fi
    if jq -e '."$schema" == "https://json-schema.org/draft/2020-12/schema"' "$f" > /dev/null 2>&1; then
        N_DRAFT=$((N_DRAFT + 1))
    fi
done
if [ "$N_INVALID" -eq 0 ]; then pass "A1 全部 $N_TOTAL 个 schema JSON 合法"; else fail "A1 $N_INVALID 个非法 schema"; fi
if [ "$N_DRAFT" -eq "$N_TOTAL" ]; then pass "A2 全部声明 draft 2020-12（$N_DRAFT/${N_TOTAL}）"; else fail "A2 draft 声明缺失（$N_DRAFT/${N_TOTAL}）"; fi
WE_N=$(jq '.properties.type.enum | length' "$SCHEMA_DIR/events/worker_event.json" 2>/dev/null)
if [ "$WE_N" = "19" ]; then pass "A3 event.type 枚举全集 19 种"; else fail "A3 event.type 枚举数=${WE_N}（期望 19）"; fi
IDX_CMDS=$(jq '.properties.commands.items.enum | length' "$SCHEMA_DIR/_index.json")
IDX_FRAMES=$(jq '.properties.frames.items.enum | length' "$SCHEMA_DIR/_index.json")
TOP_N=$(find "$SCHEMA_DIR" -maxdepth 1 -name "*.json" ! -name "_index.json" | wc -l | tr -d ' ')
EV_N=$(find "$SCHEMA_DIR/events" -name "*.json" | wc -l | tr -d ' ')
if [ "$TOP_N" = "$((IDX_CMDS + 1))" ]; then pass "A4 命令文件数 $TOP_N = _index.commands $IDX_CMDS + 信封"; else fail "A4 命令文件数 $TOP_N vs $((IDX_CMDS + 1))"; fi
if [ "$EV_N" = "$IDX_FRAMES" ]; then pass "A5 帧文件数 $EV_N = _index.frames $IDX_FRAMES"; else fail "A5 帧文件数 $EV_N vs $IDX_FRAMES"; fi

echo
echo "=== 启动隔离 host（HOME=${HOME} sock=${ION_HOST_SOCKET}）==="
"$ION_BIN" serve > "$TEST_DIR/host.log" 2>&1 &
HOST_PID=$!
HOST_READY=0
for _ in $(seq 1 20); do
    sleep 1
    if rpc list_sessions 2>/dev/null | grep -q "sessions"; then HOST_READY=1; break; fi
done
if [ "$HOST_READY" -eq 1 ]; then pass "host 启动（PID=${HOST_PID}）"; else
    fail "host 启动失败"; tail -5 "$TEST_DIR/host.log" | sed 's/^/     /'; exit 1; fi

echo
echo "=== B hello 握手帧形状 ==="
HELLO_OUT=$(sock_send_read "$ION_HOST_SOCKET" '{"id":"h1","method":"hello"}' 4 2)
H1=$(printf '%s\n' "$HELLO_OUT" | head -1)
if [ "$(evjson "$H1" '.type')" = "response" ] && [ "$(evjson "$H1" '.success')" = "true" ] \
   && [ "$(evjson "$H1" '.data.protocolVersion')" = "1" ] && [ "$(evjson "$H1" '.id')" = "h1" ]; then
    pass "B1 hello → response 外壳 + protocolVersion=1（契约 hello.json）"
else fail "B1 hello 帧（got: $H1）"; fi

echo
echo "=== C subscribe：ack(epoch) → snapshot 帧 → 实时 instance_event 帧 ==="
CREATE_OUT=$(rpc create_worker "{\"relation\":\"child\",\"creator\":\"schema-ci\",\"project_path\":\"$TEST_DIR/proj\",\"initial_prompt\":\"schema ci\"}")
SID=$(printf '%s' "$CREATE_OUT" | jq -r '.data.sessionId // empty')
WID=$(printf '%s' "$CREATE_OUT" | jq -r '.data.workerId // empty')
if [ -n "$SID" ]; then pass "C0 create_worker → $SID"; else fail "C0 create_worker"; exit 1; fi
rpc_ok() { rpc "$1" "${2:-}" | jq -e '.success == true' > /dev/null 2>&1; }
wrpc() { local p="${2:-}"; [ -z "$p" ] && p='{}'; "$ION_BIN" rpc --session "$SID" --method "$1" --params "$p"; }
wrpc prompt '{"text":"warmup"}' > /dev/null 2>&1
sleep 1

# 订阅并触发事件流：后台 python 先订阅（保持连接），1s 后主线程 prompt
SUB_LOG="$TEST_DIR/c_frames.log"
python3 - "$ION_HOST_SOCKET" "$SID" "$SUB_LOG" << 'PYEOF' &
import socket, json, sys, time
sock_path, sid, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX); s.settimeout(20)
s.connect(sock_path)
s.sendall(json.dumps({"id":"c1","method":"subscribe","session":sid,"replay":3}).encode() + b"\n")
buf = b""
deadline = time.time() + 14
with open(out_path, "w") as f:
    while time.time() < deadline:
        s.settimeout(max(0.1, deadline - time.time()))
        try:
            d = s.recv(65536)
        except socket.timeout:
            break
        if not d: break
        buf += d
        while b"\n" in buf:
            raw, buf = buf.split(b"\n", 1)
            t = raw.decode(errors="replace").strip()
            if t: f.write(t + "\n"); f.flush()
s.close()
PYEOF
sleep 1.2
wrpc prompt '{"text":"stream events"}' > /dev/null 2>&1
sleep 2

ACK_LINE=$(sed -n '1p' "$SUB_LOG"); SNAP_LINE=$(sed -n '2p' "$SUB_LOG")
if [ "$(evjson "$ACK_LINE" '.type')" = "subscribed" ] && [ "$(evjson "$ACK_LINE" '.stream')" = "instance" ] \
   && [ "$(evjson "$ACK_LINE" '.epoch')" = "1" ] && [ "$(evjson "$ACK_LINE" '.session')" = "$SID" ] \
   && [ "$(evjson "$ACK_LINE" '.replayed')" != "null" ]; then
    pass "C1 ack：type/session/stream=instance/epoch=1/replayed 全齐（契约 subscribed_ack.json）"
else fail "C1 ack 帧（got: $ACK_LINE）"; fi
if [ "$(evjson "$SNAP_LINE" '.type')" = "instance_event" ] && [ "$(evjson "$SNAP_LINE" '.snapshot')" = "true" ] \
   && [ "$(evjson "$SNAP_LINE" '.epoch')" = "1" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.customType')" = "snapshot" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.extension')" = "host" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.visibility')" = "ui_only" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.data.session.sessionId')" = "$SID" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.data.worker.workerId')" = "$WID" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.data.worker.status')" != "" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.data.pendingApprovals.count')" != "null" ] \
   && [ "$(evjson "$SNAP_LINE" '.event.data.generatedAt')" != "null" ]; then
    pass "C2 快照帧：instance_event+snapshot=true+customType=snapshot+worker/session/pendingApprovals/generatedAt（契约 snapshot_frame.json）"
else fail "C2 快照帧（got: $SNAP_LINE）"; fi
# 快照严格先于增量：第 2 帧是 snapshot；replay 帧在快照之后
REPLAY_POS=$(grep -n '"replayed":true' "$SUB_LOG" | head -1 | cut -d: -f1)
if [ -z "$REPLAY_POS" ] || [ "$REPLAY_POS" -gt 2 ]; then
    pass "C3 replay 帧不在快照之前（水合纪律）"
else fail "C3 replay 帧出现在快照前（pos=${REPLAY_POS}）"; fi
# 实时帧形状：除 subscribed ack 外全部为 instance_event 且 epoch=1；text_delta/agent_end/rpc_response 至少各一
BAD_FRAME=$(jq -s -r '[.[] | select(.type != "subscribed" and (.type != "instance_event" or .epoch != 1 or .session != "'"$SID"'"))] | length' "$SUB_LOG" 2>/dev/null)
TOTAL_FRAME=$(jq -s -r '[.[] | select(.type == "instance_event")] | length' "$SUB_LOG" 2>/dev/null)
if [ "${BAD_FRAME:-1}" = "0" ]; then
    pass "C4 全部 ${TOTAL_FRAME} 条 instance_event 帧均 epoch=1+session 一致（契约 instance_event.json；ack 帧除外）"
else fail "C4 有帧不符合 instance_event 形状（bad=${BAD_FRAME}/instance_event=${TOTAL_FRAME}）"; fi
for et in text_delta agent_start agent_end rpc_response; do
    N=$(jq -s --arg t "$et" '[.[] | select(.event.type == $t)] | length' "$SUB_LOG" 2>/dev/null)
    if [ "${N:-0}" -ge 1 ]; then
        pass "C5 内层事件 $et 帧可见（$N 条，契约 $et.json）"
    else fail "C5 缺 $et 帧"; fi
done
# rpc_response 事件摘要字段
RPC_EVT=$(jq -s -r '[.[] | select(.event.type == "rpc_response")][0] // empty' "$SUB_LOG" 2>/dev/null)
if [ -n "$RPC_EVT" ] && [ "$(evjson "$RPC_EVT" '.event.method')" = "prompt" ] \
   && [ "$(evjson "$RPC_EVT" '.event.success')" != "null" ] \
   && [ "$(evjson "$RPC_EVT" '.event.sessionId')" = "$SID" ]; then
    pass "C6 rpc_response 事件带 method/success/sessionId 摘要（契约 rpc_response_event.json）"
else fail "C6 rpc_response 摘要字段（got: $RPC_EVT）"; fi

echo
echo "=== D epoch 栅栏：stale_route 帧形状 + 重派后新 epoch ==="
G2_LOG="$TEST_DIR/d_frames.log"
python3 - "$ION_HOST_SOCKET" "$SID" "$G2_LOG" << 'PYEOF' &
import socket, json, sys, time
sock_path, sid, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX); s.settimeout(25)
s.connect(sock_path)
s.sendall(json.dumps({"id":"d1","method":"subscribe","session":sid}).encode() + b"\n")
buf = b""
deadline = time.time() + 18
with open(out_path, "w") as f:
    while time.time() < deadline:
        s.settimeout(max(0.1, deadline - time.time()))
        try:
            d = s.recv(65536)
        except socket.timeout:
            break
        if not d: break
        buf += d
        while b"\n" in buf:
            raw, buf = buf.split(b"\n", 1)
            t = raw.decode(errors="replace").strip()
            if t: f.write(t + "\n"); f.flush()
s.close()
PYEOF
sleep 1.2
KILL_OUT=$(rpc kill_worker "{\"workerId\":\"$WID\"}")
if printf '%s' "$KILL_OUT" | jq -e '.success == true' > /dev/null 2>&1; then pass "D1 kill_worker 精确击杀"; else fail "D1 kill_worker"; fi
wait $! 2>/dev/null
STALE_N=$(jq -s '[.[] | select(.type == "stale_route")] | length' "$G2_LOG" 2>/dev/null)
STALE_LINE=$(jq -s -r '[.[] | select(.type == "stale_route")][0] // empty' "$G2_LOG" 2>/dev/null)
LAST_TYPE=$(tail -1 "$G2_LOG" | jq -r '.type' 2>/dev/null)
if [ "$STALE_N" = "1" ] && [ "$(evjson "$STALE_LINE" '.customType')" = "stale_route" ] \
   && [ "$(evjson "$STALE_LINE" '.epoch')" = "1" ] && [ "$(evjson "$STALE_LINE" '.currentEpoch')" = "2" ]; then
    pass "D2 恰一条 stale_route：customType+epoch=1+currentEpoch=2（契约 stale_route.json）"
else fail "D2 stale_route 帧（n=$STALE_N, got: $STALE_LINE）"; fi
if [ "$LAST_TYPE" = "stale_route" ]; then
    pass "D3 stale_route 是最后一帧（收后停止转发）"
else fail "D3 最后一帧是 $LAST_TYPE"; fi
wrpc prompt '{"text":"respawn after kill"}' > /dev/null 2>&1
sleep 1
SUB2_OUT=$(sock_send_read "$ION_HOST_SOCKET" "{\"id\":\"d9\",\"method\":\"subscribe\",\"session\":\"$SID\"}" 6 2)
A2=$(printf '%s\n' "$SUB2_OUT" | head -1)
if [ "$(evjson "$A2" '.epoch')" = "2" ]; then
    pass "D4 重派后新订阅 ack epoch=2（epoch 单调）"
else fail "D4 新订阅 epoch（got: $A2）"; fi

echo
echo "=== E ui_respond 同源绑定响应帧 ==="
DENY_OUT=$(sock_send_read "$ION_HOST_SOCKET" '{"id":"u1","method":"ui_respond","params":{"request_id":"nonexistent","response":"allow"}}' 4 2)
D1=$(printf '%s\n' "$DENY_OUT" | head -1)
if printf '%s' "$(evjson "$D1" '.error')" | grep -q "^ui_respond rejected: " && [ "$(evjson "$D1" '.success')" = "false" ]; then
    pass "E1 未订阅连接 → response{success:false, error:'ui_respond rejected: ...'}（契约 ui_respond.json responseRejected）"
else fail "E1 拒绝帧（got: $D1）"; fi
G4_LOG="$TEST_DIR/e_ui.log"
python3 - "$ION_HOST_SOCKET" "$G4_LOG" << 'PYEOF' &
import socket, json, sys, time
sock_path, out_path = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX); s.settimeout(6)
s.connect(sock_path)
s.sendall(b'{"id":"u2","method":"subscribe","ui":true}\n')
time.sleep(0.3)
s.sendall(b'{"id":"u3","method":"ui_respond","params":{"request_id":"nonexistent","response":"allow"}}\n')
buf = b""; lines = []
deadline = time.time() + 4
while len(lines) < 2 and time.time() < deadline:
    s.settimeout(max(0.1, deadline - time.time()))
    try:
        d = s.recv(65536)
    except socket.timeout:
        break
    if not d: break
    buf += d
    while b"\n" in buf and len(lines) < 2:
        raw, buf = buf.split(b"\n", 1)
        t = raw.decode(errors="replace").strip()
        if t: lines.append(t)
with open(out_path, "w") as f:
    for l in lines: f.write(l + "\n")
s.close()
PYEOF
wait $! 2>/dev/null
ACK_UI=$(sed -n '1p' "$G4_LOG"); RESP_UI=$(sed -n '2p' "$G4_LOG")
if [ "$(evjson "$ACK_UI" '.type')" = "subscribed" ] && [ "$(evjson "$ACK_UI" '.stream')" = "ui" ]; then
    pass "E2 ui subscribe ack：type=subscribed+stream=ui（契约 subscribed_ack.json ui 变体）"
else fail "E2 ui ack（got: $ACK_UI）"; fi
if [ "$(evjson "$RESP_UI" '.error')" = "request not found or already expired" ]; then
    pass "E3 同源放行 → 业务层 not found（契约 ui_respond.json responseNotFound）"
else fail "E3 同源应答（got: $RESP_UI）"; fi

echo
echo "=== F host 级查询命令响应形状 ==="
LS_OUT=$(rpc list_sessions)
if printf '%s' "$LS_OUT" | jq -e '.success == true and (.data.sessions | type == "array") and (.data.sessions[0].status | test("^[IBDS]"))' > /dev/null 2>&1; then
    pass "F1 list_sessions：data.sessions[]，status 首字母大写（Display，契约 list_sessions.json）"
else fail "F1 list_sessions（got: $(printf '%s' "$LS_OUT" | head -c 200)）"; fi
GO_OUT=$(rpc get_overview)
if printf '%s' "$GO_OUT" | jq -e '.success == true and .data.total_workers != null and .data.total_projects != null and .data.total_stale != null and .data.total_dead != null and (.data.workers | type == "array") and (.data.projects | type == "array") and (.data.sessions | type == "array")' > /dev/null 2>&1; then
    pass "F2 get_overview：workers/projects/totals 全齐（契约 get_overview.json / events/overview.json）"
else fail "F2 get_overview（got: $(printf '%s' "$GO_OUT" | head -c 200)）"; fi
VP_OUT=$(rpc verb_pending)
if printf '%s' "$VP_OUT" | jq -e '.success == true and (.data.pending | type == "array")' > /dev/null 2>&1; then
    pass "F3 verb_pending：data.pending[]（契约 verb_pending.json）"
else fail "F3 verb_pending"; fi
VR_OUT=$(rpc verb_review "{\"requestId\":\"nonexistent\",\"approve\":false}")
if printf '%s' "$VR_OUT" | jq -e '.success == false and (.error | startswith("verb approval not found: "))' > /dev/null 2>&1; then
    pass "F4 verb_review 未命中：error 'verb approval not found: <id>'（契约 verb_review.json）"
else fail "F4 verb_review（got: $VR_OUT）"; fi

echo
echo "=== G subscribe_overview：ack(initial) + overview_snapshot 帧 ==="
G7_LOG="$TEST_DIR/g_overview.log"
python3 - "$ION_HOST_SOCKET" "$SID" "$G7_LOG" << 'PYEOF' &
import socket, json, sys, time
sock_path, sid, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX); s.settimeout(15)
s.connect(sock_path)
s.sendall(b'{"id":"o1","method":"subscribe_overview"}\n')
buf = b""
deadline = time.time() + 12
with open(out_path, "w") as f:
    while time.time() < deadline:
        s.settimeout(max(0.1, deadline - time.time()))
        try:
            d = s.recv(65536)
        except socket.timeout:
            break
        if not d: break
        buf += d
        while b"\n" in buf:
            raw, buf = buf.split(b"\n", 1)
            t = raw.decode(errors="replace").strip()
            if t: f.write(t + "\n"); f.flush()
s.close()
PYEOF
sleep 1.5
# kill 当前 worker 触发 broadcast_overview
CUR_WID=$(rpc list_workers | jq -r --arg s "$SID" '[.data.workers[] | select(.sessionId == $s)][0].workerId // empty')
[ -n "$CUR_WID" ] && rpc kill_worker "{\"workerId\":\"$CUR_WID\"}" > /dev/null 2>&1
wait $! 2>/dev/null
OV_ACK=$(sed -n '1p' "$G7_LOG")
if [ "$(evjson "$OV_ACK" '.type')" = "response" ] && [ "$(evjson "$OV_ACK" '.data.stream')" = "overview" ] \
   && [ "$(evjson "$OV_ACK" '.data.initial.total_workers')" != "null" ]; then
    pass "G1 subscribe_overview ack：data.stream=overview+initial 概览载荷（契约 subscribe_overview.json）"
else fail "G1 overview ack（got: $OV_ACK）"; fi
OV_SNAP_N=$(jq -s '[.[] | select(.type == "overview_snapshot")] | length' "$G7_LOG" 2>/dev/null)
OV_SNAP=$(jq -s -r '[.[] | select(.type == "overview_snapshot")][0] // empty' "$G7_LOG" 2>/dev/null)
if [ "${OV_SNAP_N:-0}" -ge 1 ] && [ "$(evjson "$OV_SNAP" '.data.total_workers')" != "null" ] \
   && [ "$(evjson "$OV_SNAP" '.data.workers')" != "null" ]; then
    pass "G2 overview_snapshot 推送帧：type+data 概览载荷（契约 events/overview_snapshot.json，$OV_SNAP_N 条）"
else fail "G2 overview_snapshot 缺失"; fi

echo
echo "=========================================="
echo "rpc_schema_subscribe_ci: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
