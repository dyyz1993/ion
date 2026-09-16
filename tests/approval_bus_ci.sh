#!/usr/bin/env bash
# approval_bus CI — 统一审批总线 host 出口验证（APPROVAL_BUS.md）
#
# 验证（对标 docs/design/APPROVAL_BUS.md）：
#   G1 Pull 基础：approvals_pending 空表形状 {total, requests}
#   G2 三来源镜像（file_snapshot）：faux write → ApprovalRequest → 统一表出现 apr_ 条目
#   G3 条目形状：id 前缀 / kind / summary / payload.files / raisedAtMs
#   G4 Respond 矩阵：file_snapshot → 明确 M2 未接线错误（条目保留）
#   G5 旧 API 兼容：review_pending / review_approve 仍工作；旧通路完成后镜像条目收口
#   G6 错误分支：未知 id / 非法 decision / 缺 id
#   G7 Push：subscribe --ui 收到总线 ApprovalRequest（ext=host, data.approval.id=apr_*）
#            + ApprovalResolved + 旧 file-approval 原生事件共存
#   G8 snapshot 水合：subscribe(session) 快照帧 data.approvals 全量 + pendingApprovals 兼容保留
#   G9 旧 RPC 兼容：verb_pending / verb_review / ui_respond 同源绑定不变
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
TEST_DIR="$(mktemp -d /tmp/ion-appr-bus-XXXXXX)"
export HOME="$TEST_DIR/home"
mkdir -p "$HOME" "$TEST_DIR/proj"
export ION_HOST_SOCKET="$TEST_DIR/host.sock"
export ION_SESSION_DIR="$TEST_DIR/sessions"
printf '# approval bus ci\n' > "$TEST_DIR/proj/README.md"
# faux provider：host 起默认 session 不调真 LLM
export ION_FAUX_REPLY="${ION_FAUX_REPLY:-approval bus ci ready}"

cleanup() {
    [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "${HOST_PID:-}" ] && wait "$HOST_PID" 2>/dev/null
    [ -n "${SUB_PID:-}" ] && kill "$SUB_PID" 2>/dev/null
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

# ── JSON 提取 helper（输入 JSON + python 表达式 d=...；失败输出 null）──
jget() { python3 -c "
import sys, json
try: d = json.loads(sys.argv[1])
except Exception: print('null'); raise SystemExit
try: print(eval(sys.argv[2]))
except Exception: print('null')
" "$1" "$2" 2>/dev/null; }

# raw socket 一问一答（G8/G9 用）
sock_rpc() { # sock_rpc <send_line> <timeout_s>
    SOCK_="$ION_HOST_SOCKET" SEND_="$1" TMO_="${2:-5}" python3 -c '
import socket, json, os, time
s = socket.socket(socket.AF_UNIX); s.settimeout(8)
s.connect(os.environ["SOCK_"])
s.sendall((os.environ["SEND_"] + "\n").encode())
buf = b""; deadline = time.time() + float(os.environ["TMO_"]); out = []
while time.time() < deadline:
    s.settimeout(max(0.05, deadline - time.time()))
    try: d = s.recv(65536)
    except socket.timeout: break
    if not d: break
    buf += d
    while b"\n" in buf:
        raw, buf = buf.split(b"\n", 1)
        t = raw.decode(errors="replace").strip()
        if t: out.append(t)
    if out: break
s.close()
for l in out: print(l)
' 2>/dev/null
}

echo "══════════════════════════════════════════════════════════"
echo "  Approval Bus CI — $(date)  (HOME=$HOME sock=$ION_HOST_SOCKET)"
echo "══════════════════════════════════════════════════════════"

"$ION_BIN" --version >/dev/null 2>&1 || { echo "❌ ion binary missing at $ION_BIN"; exit 1; }
pass "ion binary present"

# file-snapshot 扩展开启（隔离 HOME 下写 config，安全）
mkdir -p "$HOME/.ion"
cat > "$HOME/.ion/config.json" <<'JSON'
{
  "extensions": {
    "file-snapshot": {"enabled": true},
    "global-memory": {"enabled": false}
  }
}
JSON

# ── faux 脚本：write 一个文件 → Stop（触发 file-approval on_gate_check）──
FAUX_SCRIPT="$TEST_DIR/faux.jsonl"
cat > "$FAUX_SCRIPT" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$TEST_DIR/proj/bus_ci.txt","content":"approval bus harness"}}}
{"text":"done"}
{"text":"done again"}
JSONL

# ── 启动隔离 host（faux 脚本经 host env 传给 worker 子进程）──
HOST_LOG="$TEST_DIR/host.log"
ION_FAUX_SCRIPT="$FAUX_SCRIPT" "$ION_BIN" serve >"$HOST_LOG" 2>&1 &
HOST_PID=$!
HOST_READY=0
for i in $(seq 1 15); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then HOST_READY=1; break; fi
done
if [ "$HOST_READY" = "1" ]; then pass "host 启动（PID=${HOST_PID}，隔离三件套）"; else
    fail "host 启动失败"; tail -5 "$HOST_LOG"; exit 1
fi

rpc() { "$ION_BIN" rpc "$@" 2>/dev/null; }

# ═══════════════════════════════════════════════════════════
echo ""
echo "G1: Pull 基础 — approvals_pending 空表"
# ═══════════════════════════════════════════════════════════
P0=$(rpc --method approvals_pending)
[ "$(jget "$P0" 'd["data"]["total"]')" = "0" ] && pass "G1.1 空表 total=0" || fail "G1.1 total != 0: $P0"
[ "$(jget "$P0" 'len(d["data"]["requests"])')" = "0" ] && pass "G1.2 requests=[]" || fail "G1.2 requests 非空: $P0"

# ═══════════════════════════════════════════════════════════
echo ""
echo "G7(前置): subscribe --ui 后台订阅（Push 断言用）"
# ═══════════════════════════════════════════════════════════
SUB_LOG="$TEST_DIR/sub_ui.log"
"$ION_BIN" subscribe --ui >"$SUB_LOG" 2>&1 &
SUB_PID=$!
sleep 1

# ═══════════════════════════════════════════════════════════
echo ""
echo "G2: file_snapshot 来源镜像 — faux write 触发统一表条目"
# ═══════════════════════════════════════════════════════════
CREATE=$(rpc --method create_session --params "{\"cwd\":\"$TEST_DIR/proj\"}")
SID=$(jget "$CREATE" 'd["data"]["session_id"]')
if [ -n "$SID" ] && [ "$SID" != "null" ]; then pass "G2.1 create_session（${SID}）"; else
    fail "G2.1 create_session 失败: $CREATE"; exit 1
fi

rpc --session "$SID" --method prompt --params '{"text":"write file"}' >/dev/null 2>&1 || true

ENTRY_ID=""
P1=""
for i in $(seq 1 10); do
    sleep 1
    P1=$(rpc --method approvals_pending)
    ENTRY_ID=$(jget "$P1" 'next((str(r["id"]) for r in d["data"]["requests"] if r["kind"]=="file_snapshot"), "")')
    [ -n "$ENTRY_ID" ] && [ "$ENTRY_ID" != "null" ] && break
done
if [ -n "$ENTRY_ID" ] && [ "$ENTRY_ID" != "null" ] && [ "$ENTRY_ID" != "" ]; then
    pass "G2.2 统一表出现 file_snapshot 条目（${ENTRY_ID}）"
else
    fail "G2.2 统一表无 file_snapshot 条目（10s 轮询后）"
fi

# ═══════════════════════════════════════════════════════════
echo ""
echo "G3: 条目形状"
# ═══════════════════════════════════════════════════════════
[ -n "$ENTRY_ID" ] && [ "$ENTRY_ID" != "" ] && [ "${ENTRY_ID:0:4}" = "apr_" ] \
    && pass "G3.1 id 统一前缀 apr_" || fail "G3.1 id 前缀异常: $ENTRY_ID"
P2=$(rpc --method approvals_pending)
KIND_OK=$(jget "$P2" 'next((r["kind"] for r in d["data"]["requests"] if r["id"]=="'"$ENTRY_ID"'"), "")')
[ "$KIND_OK" = "file_snapshot" ] && pass "G3.2 kind=file_snapshot" || fail "G3.2 kind=$KIND_OK"
SUM_OK=$(jget "$P2" 'next((bool(r["summary"]) for r in d["data"]["requests"] if r["id"]=="'"$ENTRY_ID"'"), False)')
[ "$SUM_OK" = "True" ] && pass "G3.3 summary 非空" || fail "G3.3 summary 空"
FILES_OK=$(jget "$P2" 'next((isinstance(r["payload"].get("files"), list) for r in d["data"]["requests"] if r["id"]=="'"$ENTRY_ID"'"), False)')
[ "$FILES_OK" = "True" ] && pass "G3.4 payload.files 列表" || fail "G3.4 payload.files 缺失"
RAISED_OK=$(jget "$P2" 'next((isinstance(r["raisedAtMs"], int) for r in d["data"]["requests"] if r["id"]=="'"$ENTRY_ID"'"), False)')
[ "$RAISED_OK" = "True" ] && pass "G3.5 raisedAtMs 整数" || fail "G3.5 raisedAtMs 异常"

# ═══════════════════════════════════════════════════════════
echo ""
echo "G4: Respond 矩阵 — file_snapshot 明确 M2 未接线"
# ═══════════════════════════════════════════════════════════
R1=$(rpc --method approval_respond --params "{\"id\":\"$ENTRY_ID\",\"decision\":\"approve\"}")
if echo "$R1" | grep -q "file_snapshot approval routing not wired"; then
    pass "G4.1 file_snapshot 返回明确 M2 未接线错误（含 review_approve 指引）"
else
    fail "G4.1 错误信息不符: $R1"
fi
P3=$(rpc --method approvals_pending)
LEFT=$(jget "$P3" 'sum(1 for r in d["data"]["requests"] if r["id"]=="'"$ENTRY_ID"'")')
[ "$LEFT" = "1" ] && pass "G4.2 未接线不应答 → 条目保留" || fail "G4.2 条目被误删"

# ═══════════════════════════════════════════════════════════
echo ""
echo "G5: 旧 API 兼容 — review_* 仍工作 + 镜像同步收口"
# ═══════════════════════════════════════════════════════════
PEND=$(rpc --session "$SID" --method review_pending --params '{}')
FIRST_PATH=$(jget "$PEND" 'd["data"]["pending"][0]["path"]')
if [ -z "$FIRST_PATH" ] || [ "$FIRST_PATH" = "null" ]; then
    FIRST_PATH=$(jget "$PEND" 'd["data"]["requests"][0]["path"]')
fi
if [ -n "$FIRST_PATH" ] && [ "$FIRST_PATH" != "null" ]; then
    pass "G5.1 review_pending 仍含待审文件（${FIRST_PATH}）"
    APPROVE_OUT=$(rpc --session "$SID" --method review_approve --params "{\"path\":\"$FIRST_PATH\"}")
    echo "$APPROVE_OUT" | grep -qi "approv" && pass "G5.2 review_approve（旧通路）生效" || fail "G5.2 approve 失败: $APPROVE_OUT"
    GONE=""
    for i in $(seq 1 8); do
        sleep 1
        P4=$(rpc --method approvals_pending)
        LEFT2=$(jget "$P4" 'sum(1 for r in d["data"]["requests"] if r["id"]=="'"$ENTRY_ID"'")')
        [ "$LEFT2" = "0" ] && { GONE=1; break; }
    done
    [ "$GONE" = "1" ] && pass "G5.3 旧通路完成后统一表镜像同步收口" || fail "G5.3 镜像未收口"
else
    fail "G5.1 review_pending 无待审: $PEND"
fi

# ═══════════════════════════════════════════════════════════
echo ""
echo "G6: 错误分支"
# ═══════════════════════════════════════════════════════════
R2=$(rpc --method approval_respond --params '{"id":"apr_nope","decision":"approve"}')
echo "$R2" | grep -q "approval not found" && pass "G6.1 未知 id → approval not found" || fail "G6.1: $R2"
R3=$(rpc --method approval_respond --params '{"id":"apr_x","decision":"yes"}')
echo "$R3" | grep -q "invalid params.decision" && pass "G6.2 非法 decision → 明确错误" || fail "G6.2: $R3"
R4=$(rpc --method approval_respond --params '{"decision":"approve"}')
echo "$R4" | grep -q "missing params.id" && pass "G6.3 缺 id → 明确错误" || fail "G6.3: $R4"

# ═══════════════════════════════════════════════════════════
echo ""
echo "G7: Push — subscribe --ui 收到总线事件"
# ═══════════════════════════════════════════════════════════
sleep 1
kill "$SUB_PID" 2>/dev/null || true
SUB_PID=""
# subscribe CLI 输出 pretty JSON（多行，键序 data→extension→...→ui_type）：
# 匹配到 "ui_type": "ApprovalRequest" 后向前取窗口找 ext=host + data.approval.id
BUS_EV=$(python3 - "$SUB_LOG" <<'PYEOF'
import sys, re
text = ""
try:
    with open(sys.argv[1]) as f: text = f.read()
except FileNotFoundError:
    pass
hits = []
for m in re.finditer(r'"ui_type":\s*"ApprovalRequest"', text):
    window = text[max(0, m.start() - 6000):m.start() + 50]
    if re.search(r'"extension":\s*"host"', window):
        ids = re.findall(r'"id":\s*"(apr_[0-9a-f]+)"', window)
        if ids: hits.append(ids[-1])
print("|".join(hits[:5]))
PYEOF
)
if [ -n "$BUS_EV" ]; then
    pass "G7.1 总线 ApprovalRequest（ext=host, data.approval.id=apr_*）推送可见: $BUS_EV"
else
    fail "G7.1 未收到总线 ApprovalRequest（见 ${SUB_LOG}）"
fi
grep -Eq '"ui_type": *"ApprovalRequest"' "$SUB_LOG" 2>/dev/null \
    && pass "G7.2 file-approval 原生 ApprovalRequest 与总线事件共存" \
    || fail "G7.2 sub log 无任何 ApprovalRequest"
grep -Eq '"ui_type": *"ApprovalResolved"' "$SUB_LOG" 2>/dev/null \
    && pass "G7.3 ApprovalResolved 推送可见（G5 旧通路触发）" \
    || fail "G7.3 未收到 ApprovalResolved"

# ═══════════════════════════════════════════════════════════
echo ""
echo "G8: snapshot 水合 — approvals 全量字段 + pendingApprovals 兼容"
# ═══════════════════════════════════════════════════════════
# 先再造一条 pending：call_tool 直调 write + prompt（faux 静态回复 → Stop → gate_check）
rpc --session "$SID" --method call_tool --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$TEST_DIR/proj/bus_ci2.txt\",\"content\":\"second\"}}" >/dev/null 2>&1 || true
rpc --session "$SID" --method prompt --params '{"text":"continue"}' >/dev/null 2>&1 || true
# 轮询等第二条 pending 进统一表（事件异步）
for i in $(seq 1 10); do
    sleep 1
    PN=$(rpc --method approvals_pending)
    PN_T=$(jget "$PN" 'd["data"]["total"]')
    [ "$PN_T" -ge 1 ] 2>/dev/null && break
done
[ "$PN_T" -ge 1 ] 2>/dev/null && pass "G8.0 第二次 write 产生新 pending（total=${PN_T}）" || fail "G8.0 第二次 write 未进统一表"

SNAP=$(SID_="$SID" SOCK_="$ION_HOST_SOCKET" python3 -c '
import socket, json, os, time
s = socket.socket(socket.AF_UNIX); s.settimeout(8)
s.connect(os.environ["SOCK_"])
send = {"id": "snap1", "method": "subscribe", "session": os.environ["SID_"]}
s.sendall((json.dumps(send) + "\n").encode())
buf = b""; deadline = time.time() + 6
while time.time() < deadline:
    s.settimeout(max(0.05, deadline - time.time()))
    try: d = s.recv(65536)
    except socket.timeout: break
    if not d: break
    buf += d
    while b"\n" in buf:
        raw, buf = buf.split(b"\n", 1)
        try: v = json.loads(raw.decode())
        except Exception: continue
        if v.get("type") == "instance_event" and v.get("snapshot"):
            s.close(); print(json.dumps(v)); raise SystemExit
s.close(); print("null")
' 2>/dev/null)
if [ -n "$SNAP" ] && [ "$SNAP" != "null" ]; then
    A_T=$(jget "$SNAP" 'd["event"]["data"]["approvals"]["total"]')
    PA_C=$(jget "$SNAP" 'd["event"]["data"]["pendingApprovals"]["count"]')
    if [ "$A_T" -ge 1 ] 2>/dev/null; then
        pass "G8.1 snapshot 含 approvals 全量且非空（total=${A_T}）"
    else
        fail "G8.1 snapshot 缺 approvals 字段"
    fi
    if [ -n "$PA_C" ] && [ "$PA_C" != "null" ]; then
        pass "G8.2 pendingApprovals 兼容字段保留（count=${PA_C}）"
    else
        fail "G8.2 pendingApprovals 丢失"
    fi
else
    fail "G8 未收到 snapshot 帧"
fi

# ═══════════════════════════════════════════════════════════
echo ""
echo "G9: 旧 RPC 兼容"
# ═══════════════════════════════════════════════════════════
V=$(rpc --method verb_pending --params '{}')
VP=$(jget "$V" 'type(d["data"].get("pending")).__name__')
[ "$VP" = "list" ] && pass "G9.1 verb_pending 形状不变（pending 列表）" || fail "G9.1: $V"
VR=$(rpc --method verb_review --params '{"requestId":"vapp_nonexist","approve":true}')
echo "$VR" | grep -q "verb approval not found" && pass "G9.2 verb_review 未知 id 行为不变" || fail "G9.2: $VR"
UIR=$(sock_rpc '{"id":"ur","method":"ui_respond","params":{"request_id":"x","response":"allow"}}')
echo "$UIR" | grep -q "requires a prior subscribe" && pass "G9.3 ui_respond 同源绑定不变（独立连接被拒）" || fail "G9.3: $UIR"

echo ""
echo "══════════════════════════════════════════════════════════"
echo "  Approval Bus CI 结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
