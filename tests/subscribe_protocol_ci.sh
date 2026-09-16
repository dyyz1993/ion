#!/usr/bin/env bash
# subscribe_protocol CI — subscribe 协议升级四件套验证（raw socket 直连）
#
# 验证（对标 docs/design/SUBSCRIBE_PROTOCOL.md）：
#   G1 快照先行：subscribe 后先收 customType:"snapshot" 快照帧（含 epoch），再收实时增量
#   G2 epoch 栅栏：worker 重派（kill 后 session 重建）→ 旧 epoch 订阅收一条 stale_route 并断开；
#      新订阅 epoch 递增
#   G3 hello 握手：{"method":"hello"} → protocolVersion:1 + hostId（逻辑实例身份）；
#      hello 后同一连接可继续 subscribe；hostId 跨连接稳定、host 重启即换
#   G4 审批同源绑定：未 subscribe(ui) 的连接 ui_respond 被拒；同连接 ui subscribe 后放行
#   G5 旧客户端兼容：不发 hello 的 ion rpc / ion subscribe 行为不变
#   G6 hostId 重启换新：host 重启后 hello 的 hostId 变化；CLI ION_EXPECT_HOST_ID pin 旧值被拒、
#      pin 新值放行
#
# 隔离铁律：私有 HOME + 私有 ION_HOST_SOCKET + 私有 ION_SESSION_DIR，绝不碰真实 ~/.ion；
# 只 kill 自己启动的精确 PID。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

# ── 隔离三件套 ──
TEST_DIR="$(mktemp -d /tmp/ion-sub-proto-XXXXXX)"
export HOME="$TEST_DIR/home"
mkdir -p "$HOME" "$TEST_DIR/proj"
export ION_HOST_SOCKET="$TEST_DIR/host.sock"
export ION_SESSION_DIR="$TEST_DIR/sessions"
printf '# subscribe protocol ci\n' > "$TEST_DIR/proj/README.md"
# faux provider：host 起默认 session 不调真 LLM
export ION_FAUX_REPLY="${ION_FAUX_REPLY:-subscribe protocol ci ready}"

cleanup() {
    [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "${HOST_PID:-}" ] && wait "$HOST_PID" 2>/dev/null
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

# ── raw socket 客户端（不依赖 ion 二进制， immune target 清理）──
# 用法: sockrpc <socket> <发送行(可空)> <读超时秒> <最多读行数> → 每行一个 JSON 打到 stdout
sock_read() { # sock_read <socket> <timeout_s> <max_lines>  （只读不发）
    SOCK_="$1" TMO_="$2" MAX_="$3" python3 -c '
import socket, json, os, sys, time
s = socket.socket(socket.AF_UNIX); s.settimeout(float(os.environ["TMO_"]))
s.connect(os.environ["SOCK_"])
buf = b""; lines = []; deadline = time.time() + float(os.environ["TMO_"])
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
sock_send_read() { # sock_send_read <socket> <send_line> <timeout_s> <max_lines>
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

evjson() { # evjson <line> <jq 路径> → 取值（失败输出空）
    printf '%s' "$1" | jq -r "$2" 2>/dev/null || echo ""
}

rpc() { local p="${2:-}"; [ -z "$p" ] && p='{}'; "$ION_BIN" rpc --method "$1" --params "$p"; }
wrpc() { local p="${2:-}"; [ -z "$p" ] && p='{}'; "$ION_BIN" rpc --session "$SID" --method "$1" --params "$p"; }
rpc_ok() { rpc "$1" "${2:-}" | jq -e '.success == true' > /dev/null 2>&1; }

# ── 起 host（精确 PID 记录，绝不 pkill）──
echo "=== 启动隔离 host（HOME=HOME=${HOME} sock=${ION_HOST_SOCKET}）==="
"$ION_BIN" serve > "$TEST_DIR/host.log" 2>&1 &
HOST_PID=$!
HOST_READY=0
for _ in $(seq 1 20); do
    sleep 1
    if rpc list_sessions 2>/dev/null | grep -q "sessions"; then HOST_READY=1; break; fi
done
if [ "$HOST_READY" -eq 1 ]; then pass "host 启动（PID=${HOST_PID}，私有 HOME/socket）"; else
    fail "host 启动失败"; tail -5 "$TEST_DIR/host.log" | sed 's/^/     /'; exit 1; fi

cleanup() {
    [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "${HOST_PID:-}" ] && wait "$HOST_PID" 2>/dev/null
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

echo
echo "=== 准备：create_worker（faux）拿 sessionId ==="
CREATE_OUT=$(rpc create_worker "{\"relation\":\"child\",\"creator\":\"sub-proto-ci\",\"project_path\":\"$TEST_DIR/proj\",\"initial_prompt\":\"proto ci\"}")
SID=$(printf '%s' "$CREATE_OUT" | jq -r '.data.sessionId // empty')
if [ -n "$SID" ]; then pass "create_worker → $SID"; else fail "create_worker 拿 sessionId"; echo "$CREATE_OUT"; exit 1; fi
WID=$(printf '%s' "$CREATE_OUT" | jq -r '.data.workerId // empty')

echo
echo "=== G1 快照先行：subscribe → snapshot 帧 → 实时增量 ==="
# 先产生一条可回放的历史事件（prompt 跑 faux 一轮）
wrpc prompt '{"text":"hello proto"}' > /dev/null 2>&1
sleep 1
SUB_OUT=$(sock_send_read "$ION_HOST_SOCKET" "{\"id\":\"s1\",\"method\":\"subscribe\",\"session\":\"$SID\",\"replay\":3}" 6 12)
ACK_LINE=$(printf '%s\n' "$SUB_OUT" | head -1)
SNAP_LINE=$(printf '%s\n' "$SUB_OUT" | sed -n '2p')
if [ "$(evjson "$ACK_LINE" '.type')" = "subscribed" ] && [ "$(evjson "$ACK_LINE" '.epoch')" = "1" ]; then
    pass "G1.1 subscribed ack 带 epoch=1"
else fail "G1.1 subscribed ack 带 epoch=1（got: ${ACK_LINE}）"; fi
if [ "$(evjson "$SNAP_LINE" '.event.customType')" = "snapshot" ] && [ "$(evjson "$SNAP_LINE" '.snapshot')" = "true" ]; then
    pass "G1.2 第 2 帧是 customType=snapshot 快照（snapshot 先于增量）"
else fail "G1.2 快照帧（got: ${SNAP_LINE}）"; fi
if [ "$(evjson "$SNAP_LINE" '.epoch')" = "1" ] && [ "$(evjson "$SNAP_LINE" '.event.data.session.sessionId')" = "$SID" ]; then
    pass "G1.3 快照帧带 epoch + session.sessionId"
else fail "G1.3 快照帧 epoch/sessionId"; fi
if [ "$(evjson "$SNAP_LINE" '.event.data.worker.workerId')" != "" ] && [ "$(evjson "$SNAP_LINE" '.event.data.worker.status')" != "" ]; then
    pass "G1.4 快照含 worker 状态（workerId/status/model）"
else fail "G1.4 快照 worker 字段（got: ${SNAP_LINE}）"; fi
REPLAYED_N=$(printf '%s\n' "$SUB_OUT" | jq -s '[.[] | select(.replayed == true)] | length' 2>/dev/null || echo 0)
if [ "${REPLAYED_N:-0}" -ge 1 ]; then
    pass "G1.5 replay 历史帧在快照之后到达（$REPLAYED_N 条，replayed:true）"
else fail "G1.5 replay 帧缺失"; fi

echo
echo "=== G2 epoch 栅栏：kill worker → 旧订阅收 stale_route；重派后新订阅 epoch+1 ==="
SUB_LOG="$TEST_DIR/g2_sub.log"
# 后台订阅（python 保持连接持续收帧），1.5s 后杀 worker → router 重绑
python3 - "$ION_HOST_SOCKET" "$SID" "$SUB_LOG" << 'PYEOF' &
import socket, json, os, sys, time
sock_path, sid, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX); s.settimeout(25)
s.connect(sock_path)
s.sendall(json.dumps({"id":"g2","method":"subscribe","session":sid}).encode() + b"\n")
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
G2_PID=$!
# 等 ack 到达再杀（重负载下 python 启动可能慢于脚本的 kill——先确认订阅已建立，
# 否则 subscribe 会撞进 router 的重绑等待窗口，收到 "router dropped" 错误帧而非 stale_route）
for i in $(seq 1 20); do
    grep -q '"type":"subscribed"' "$SUB_LOG" 2>/dev/null && break
    sleep 0.5
done
# 杀掉 worker（精确 RPC，非 pkill）→ router 重绑：epoch 推进 → 旧订阅收 stale_route
KILL_OUT=$(rpc kill_worker "{\"workerId\":\"$WID\"}")
if printf '%s' "$KILL_OUT" | jq -e '.success == true' > /dev/null 2>&1; then pass "G2.1 kill_worker 精确击杀 $WID"; else fail "G2.1 kill_worker"; fi
# 旧订阅应收到一条 stale_route 然后连接关闭
wait $G2_PID 2>/dev/null
STALE_LINE=$(jq -s -r '[.[] | select(.type == "stale_route")][0] // empty' "$SUB_LOG" 2>/dev/null)
if [ -n "$STALE_LINE" ] && [ "$(printf '%s' "$STALE_LINE" | jq -r '.epoch')" = "1" ] && [ "$(printf '%s' "$STALE_LINE" | jq -r '.currentEpoch')" = "2" ]; then
    pass "G2.2 旧 epoch 订阅收 stale_route（epoch 1 → currentEpoch 2）"
else fail "G2.2 stale_route 缺失或字段不对（log: $(wc -l < "$SUB_LOG" 2>/dev/null) 行）"; fi
# stale_route 是最后一条（收后停止转发）
LAST_TYPE=$(tail -1 "$SUB_LOG" | jq -r '.type' 2>/dev/null)
if [ "$LAST_TYPE" = "stale_route" ]; then
    pass "G2.3 stale_route 后停止转发（是最后一条帧）"
else fail "G2.3 stale_route 不是最后一条（last=${LAST_TYPE}）"; fi
# 重派：同 session 再 prompt（auto-create 拉起新 worker）→ 新订阅 epoch=2
wrpc prompt '{"text":"respawn after kill"}' > /dev/null 2>&1
sleep 1
SUB2_OUT=$(sock_send_read "$ION_HOST_SOCKET" "{\"id\":\"s2\",\"method\":\"subscribe\",\"session\":\"$SID\"}" 6 6)
ACK2=$(printf '%s\n' "$SUB2_OUT" | head -1)
if [ "$(evjson "$ACK2" '.type')" = "subscribed" ] && [ "$(evjson "$ACK2" '.epoch')" = "2" ]; then
    pass "G2.4 重派后新订阅 ack epoch=2"
else fail "G2.4 新订阅 epoch（got: ${ACK2}）"; fi
SNAP2_LINE=$(printf '%s\n' "$SUB2_OUT" | sed -n '2p')
if [ "$(evjson "$SNAP2_LINE" '.event.customType')" = "snapshot" ] && [ "$(evjson "$SNAP2_LINE" '.epoch')" = "2" ]; then
    pass "G2.5 新订阅同样先收 snapshot（epoch=2）"
else fail "G2.5 新订阅快照帧（got: ${SNAP2_LINE}）"; fi

echo
echo "=== G3 hello 握手：protocolVersion + 同连接继续 subscribe ==="
HELLO_OUT=$(sock_send_read "$ION_HOST_SOCKET" '{"id":"h1","method":"hello"}' 4 2)
H1=$(printf '%s\n' "$HELLO_OUT" | head -1)
if [ "$(evjson "$H1" '.data.protocolVersion')" = "1" ] && [ "$(evjson "$H1" '.success')" = "true" ]; then
    pass "G3.1 hello → protocolVersion=1"
else fail "G3.1 hello 响应（got: ${H1}）"; fi
# hello + subscribe 同连接连发（python 保持连接）
G3_LOG="$TEST_DIR/g3.log"
python3 - "$ION_HOST_SOCKET" "$SID" "$G3_LOG" << 'PYEOF' &
import socket, json, sys, time
sock_path, sid, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX); s.settimeout(8)
s.connect(sock_path)
s.sendall(b'{"id":"h2","method":"hello"}\n')
s.sendall(json.dumps({"id":"s3","method":"subscribe","session":sid}).encode() + b"\n")
buf = b""; lines = []
deadline = time.time() + 6
while len(lines) < 3 and time.time() < deadline:
    s.settimeout(max(0.1, deadline - time.time()))
    try:
        d = s.recv(65536)
    except socket.timeout:
        break
    if not d: break
    buf += d
    while b"\n" in buf and len(lines) < 3:
        raw, buf = buf.split(b"\n", 1)
        t = raw.decode(errors="replace").strip()
        if t: lines.append(t)
with open(out_path, "w") as f:
    for l in lines: f.write(l + "\n")
s.close()
PYEOF
wait $! 2>/dev/null
H2=$(sed -n '1p' "$G3_LOG"); A3=$(sed -n '2p' "$G3_LOG")
if [ "$(evjson "$H2" '.data.protocolVersion')" = "1" ] && [ "$(evjson "$A3" '.type')" = "subscribed" ]; then
    pass "G3.2 hello 后同连接 subscribe 正常（ack + 后续快照帧）"
else fail "G3.2 hello+subscribe 同连接（h2=$H2 a3=${A3}）"; fi

# G3.3/G3.4 hostId：hello 携带逻辑实例身份（规范 UUIDv4），跨连接稳定
HOST_ID1=$(evjson "$H1" '.data.hostId')
if printf '%s' "$HOST_ID1" | grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'; then
    pass "G3.3 hello 携带 hostId（规范小写 UUIDv4）"
else fail "G3.3 hostId 缺失或非规范 UUIDv4（got: ${HOST_ID1}）"; fi
H9_LINE=$(sock_send_read "$ION_HOST_SOCKET" '{"id":"h9","method":"hello"}' 4 2 | head -1)
if [ "$(evjson "$H9_LINE" '.data.hostId')" = "$HOST_ID1" ]; then
    pass "G3.4 跨连接 hostId 稳定（同一 host 进程，pin 校验前提）"
else fail "G3.4 hostId 漂移（got: $(evjson "$H9_LINE" '.data.hostId') vs ${HOST_ID1}）"; fi

echo
echo "=== G4 审批同源绑定：未订阅连接 ui_respond 拒绝；同连接 ui subscribe 后放行 ==="
DENY_OUT=$(sock_send_read "$ION_HOST_SOCKET" '{"id":"u1","method":"ui_respond","params":{"request_id":"nonexistent","response":"allow"}}' 4 2)
D1=$(printf '%s\n' "$DENY_OUT" | head -1)
if [ "$(evjson "$D1" '.success')" = "false" ] && printf '%s' "$(evjson "$D1" '.error')" | grep -q "ui_respond rejected"; then
    pass "G4.1 未 subscribe(ui) 的连接 ui_respond 被拒（同源错误信息）"
else fail "G4.1 ui_respond 拒绝（got: ${D1}）"; fi
# 同连接 subscribe(ui:true) 后再 ui_respond：过同源闸（unknown request → "not found" 而非 rejected）
G4_LOG="$TEST_DIR/g4.log"
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
    pass "G4.2 ui subscribe ack"
else fail "G4.2 ui subscribe ack（got: ${ACK_UI}）"; fi
if [ "$(evjson "$RESP_UI" '.success')" = "false" ] && printf '%s' "$(evjson "$RESP_UI" '.error')" | grep -q "not found"; then
    pass "G4.3 同连接 ui_respond 过同源闸（unknown id → not found，非 rejected）"
else fail "G4.3 同连接 ui_respond 应放行到业务层（got: ${RESP_UI}）"; fi

echo
echo "=== G5 旧客户端兼容：不发 hello 的 ion rpc / ion subscribe 行为不变 ==="
OLD_RPC=$(rpc list_sessions)
if printf '%s' "$OLD_RPC" | jq -e '.success == true' > /dev/null 2>&1; then
    pass "G5.1 ion rpc（不发 hello）照常工作"
else fail "G5.1 ion rpc（got: ${OLD_RPC}）"; fi
OLD_SUB=$(sock_send_read "$ION_HOST_SOCKET" "{\"id\":\"old\",\"method\":\"subscribe\",\"session\":\"$SID\"}" 6 2)
O1=$(printf '%s\n' "$OLD_SUB" | head -1); O2=$(printf '%s\n' "$OLD_SUB" | sed -n '2p')
if [ "$(evjson "$O1" '.type')" = "subscribed" ] && [ "$(evjson "$O2" '.event.customType')" = "snapshot" ]; then
    pass "G5.2 旧客户端 subscribe 照常（ack 帧形状兼容，新增 epoch/snapshot 字段为增量）"
else fail "G5.2 旧客户端 subscribe（got: $O1 / ${O2}）"; fi
CLI_SUB="$TEST_DIR/g5_cli.log"
timeout 5 "$ION_BIN" subscribe --session "$SID" > "$CLI_SUB" 2>&1
if grep -q '"subscribed"' "$CLI_SUB" && grep -q '"snapshot"' "$CLI_SUB"; then
    pass "G5.3 ion subscribe CLI 照常工作并增量打印 epoch/snapshot 帧"
else fail "G5.3 ion subscribe CLI（$(head -c 200 "$CLI_SUB")）"; fi

echo
echo "=== G6 hostId 重启换新：host 重启后 hostId 变化（逻辑实例身份不落盘）+ CLI pin 拒绝旧值 ==="
kill "$HOST_PID" 2>/dev/null
wait "$HOST_PID" 2>/dev/null
"$ION_BIN" serve >> "$TEST_DIR/host.log" 2>&1 &
HOST_PID=$!
HOST_READY2=0
for _ in $(seq 1 20); do
    sleep 1
    if rpc list_sessions 2>/dev/null | grep -q "sessions"; then HOST_READY2=1; break; fi
done
if [ "$HOST_READY2" -eq 1 ]; then
    pass "G6.1 host 重启成功（新 PID=${HOST_PID}）"
else fail "G6.1 host 重启失败"; tail -5 "$TEST_DIR/host.log" | sed 's/^/     /'; fi
H10_LINE=$(sock_send_read "$ION_HOST_SOCKET" '{"id":"h10","method":"hello"}' 4 2 | head -1)
HOST_ID2=$(evjson "$H10_LINE" '.data.hostId')
if [ -n "$HOST_ID2" ] && [ "$HOST_ID2" != "$HOST_ID1" ]; then
    pass "G6.2 重启后 hostId 变化（${HOST_ID1:0:8}… → ${HOST_ID2:0:8}…）"
else fail "G6.2 重启后 hostId（got: ${HOST_ID2}, old: ${HOST_ID1}）"; fi
# CLI pin：ION_EXPECT_HOST_ID 指向旧 hostId → 客户端报错断开（exit 非 0）
PIN_LOG="$TEST_DIR/g6_pin.log"
ION_EXPECT_HOST_ID="$HOST_ID1" timeout 30 "$ION_BIN" rpc --method list_sessions > "$PIN_LOG" 2>&1
PIN_RC=$?
if [ "$PIN_RC" -ne 0 ] && grep -q "hostId pin" "$PIN_LOG"; then
    pass "G6.3 CLI pin 拒绝旧 hostId（报错断开，exit=${PIN_RC}）"
else fail "G6.3 CLI pin 行为异常（exit=$PIN_RC: $(head -c 200 "$PIN_LOG")）"; fi
# pin 匹配新 hostId → 正常工作
PIN2_RC=0
ION_EXPECT_HOST_ID="$HOST_ID2" rpc list_sessions > /dev/null 2>&1 || PIN2_RC=$?
if [ "$PIN2_RC" -eq 0 ]; then
    pass "G6.4 CLI pin 匹配新 hostId → 正常工作"
else fail "G6.4 CLI pin 匹配新 hostId 失败（exit=${PIN2_RC}）"; fi

echo
echo "=========================================="
echo "subscribe_protocol_ci: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
