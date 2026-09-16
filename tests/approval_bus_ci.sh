#!/usr/bin/env bash
# approval_bus CI — 统一审批总线三合一验证（APPROVAL_BUS.md 终态）
#
# 覆盖（M1 总线核心 + M2 来源接入 + M3 泵/策略 三分支合并后的完整矩阵）：
#   Phase 1  M1 核心（host p1）
#     G1  Pull 空表形状（total/requests/pending 三字段）
#     G2  file_snapshot 来源登记（faux write → ApprovalRequest → apr_ 条目）
#     G3  条目形状（apr_ 前缀 / kind / summary / payload.files / raisedAtMs）
#     G4  统一 Respond 闭环（approval_respond approve → review_approve_all 转发
#         → worker pending 归零 → 条目消除）
#     G5  旧 API 兼容（review_pending/review_approve 旧通路 → 镜像同步收口）
#     G6  错误分支（unknown id / 非法 decision / 缺 decision / 缺 id——对齐 schema）
#     G7  Push（subscribe --ui 收总线 ApprovalRequest(ext=host, data.kind 平铺)
#         + file-approval 原生事件共存 + ApprovalResolved）
#     G8  snapshot 水合（approvals 全量 + pendingApprovals 兼容）
#     G9  旧 RPC 兼容（verb_pending / verb_review / ui_respond 同源绑定）
#   Phase 2  M2 worker Ask 托管（host p2，CommandGuard 中危 → Ask）
#     W1  空表基线
#     W2  Ask 登记统一表（kind=ui_ask + workerId 非空=双登记路径 sink 胜出 + apr_ id）
#     W3  approval_respond(approve) 路由回 worker（ask_respond）→ 工具真执行
#     W4  条目消除（AskResolved + respond 双保险）
#   Phase 3  M3 policy map + 审批泵（host p3，沙盒池 mock 注入）
#     P1x  policy map 形态（config 档案 untagged + sandbox_policy RPC GET/SET
#          两形态 + 严格拒绝）
#     P2x  泵实测（worker 级 map：file_snapshot=auto → SandboxAutoApproved +
#          ApprovalResolved(by=pump) + pending 归零 + 总线同步收口）
#     P3x  ask 对照轮（per-kind ask 不触发泵）+ 统一 Respond 收尾
#     P4x  approvals_pending kind 枚举与字段（M3 Group D 预留块激活）
#
# 隔离铁律：每 Phase 独立 host（私有 HOME + 私有 ION_HOST_SOCKET + 私有
# ION_SESSION_DIR），绝不读写真实 ~/.ion；LLM 走 FauxProvider；只 kill 自己启动
# 的精确 PID，绝不 pkill。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-appr-bus-XXXXXX)"
HOST_PID=""
SUB_PID=""
cleanup() {
    # 🔴 只 kill 自己起的 PID（绝不 pkill——系统里 LogiOptionsPlus 等进程名含 ion）
    [ -n "$SUB_PID" ] && kill "$SUB_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && wait "$HOST_PID" 2>/dev/null
    rm -rf "$TEST_ROOT"
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

# start_host <tag> <faux-jsonl> <proj-dir> [K=V ...]
#   额外 config 合并：$TEST_ROOT/<tag>-extra.json 存在时深度合并进 config.json
#   额外环境变量以 K=V 参数透传（值可含引号/空格）
start_host() {
    local tag="$1" faux="$2" proj="$3"; shift 3
    local home="$TEST_ROOT/$tag-home" sock="$TEST_ROOT/$tag.sock"
    local sess="$TEST_ROOT/$tag-sessions"
    mkdir -p "$home/.ion" "$proj" "$sess"
    printf '# approval bus ci\n' > "$proj/README.md"
    # config：开 file-snapshot（审批链依赖），关 memory 系（防单例 worker 消费 faux 队列）
    cat > "$home/.ion/config.json" <<'JSON'
{
  "extensions": {
    "file-snapshot": {"enabled": true},
    "global-memory": {"enabled": false},
    "memory": {"enabled": false},
    "learning": {"enabled": false}
  }
}
JSON
    if [ -f "$TEST_ROOT/$tag-extra.json" ]; then
        python3 - "$home/.ion/config.json" "$TEST_ROOT/$tag-extra.json" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1]))
extra = json.load(open(sys.argv[2]))
for k, v in extra.items():
    if isinstance(v, dict) and isinstance(cfg.get(k), dict):
        cfg[k].update(v)
    else:
        cfg[k] = v
json.dump(cfg, open(sys.argv[1], "w"), indent=2)
PY
    fi
    (
        export HOME="$home" ION_HOST_SOCKET="$sock" ION_SESSION_DIR="$sess" ION_FAUX_SCRIPT="$faux"
        local kv
        for kv in "$@"; do export "$kv"; done
        cd "$proj" && exec "$ION_BIN" serve
    ) > "$TEST_ROOT/$tag.log" 2>&1 &
    HOST_PID=$!
    local i
    for i in $(seq 1 30); do
        sleep 1
        if HOME="$home" ION_HOST_SOCKET="$sock" "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
            return 0
        fi
    done
    echo "host[$tag] 启动失败；日志尾部："
    tail -8 "$TEST_ROOT/$tag.log"
    return 1
}

stop_host() {
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && wait "$HOST_PID" 2>/dev/null
    HOST_PID=""
}

rpc() { # rpc <tag> <ion-rpc args...>（--method/--params 显式透传）
    local tag="$1"; shift
    HOME="$TEST_ROOT/$tag-home" ION_HOST_SOCKET="$TEST_ROOT/$tag.sock" \
        timeout 25 "$ION_BIN" rpc "$@" 2>/dev/null
}

wrpc() { # wrpc <tag> <session> <ion-rpc args...>
    local tag="$1" sid="$2"; shift 2
    HOME="$TEST_ROOT/$tag-home" ION_HOST_SOCKET="$TEST_ROOT/$tag.sock" \
        timeout 25 "$ION_BIN" rpc --session "$sid" "$@" 2>/dev/null
}

# 长跑 worker 命令（后台跑，不阻塞断言轮询）
bwrpc() { # bwrpc <tag> <session> <method> <params> <timeout-s>
    local tag="$1" sid="$2" method="$3" params="$4" tmo="$5"
    HOME="$TEST_ROOT/$tag-home" ION_HOST_SOCKET="$TEST_ROOT/$tag.sock" \
        timeout "$tmo" "$ION_BIN" rpc --session "$sid" --method "$method" --params "$params" \
        >/dev/null 2>&1 &
}

# 等统一表出现 kind 匹配条目，echo apr_ id（空 = 超时）
wait_pending_id() { # <tag> <kind> [tries]
    local tag="$1" kind="$2" tries="${3:-15}" i out id
    for i in $(seq 1 "$tries"); do
        out=$(rpc "$tag" --method approvals_pending)
        id=$(jget "$out" "next((str(r['id']) for r in d['data']['requests'] if r['kind']=='$kind'), '')")
        [ -n "$id" ] && [ "$id" != "null" ] && { echo "$id"; return 0; }
        sleep 1
    done
    return 1
}

wait_pending_gone() { # <tag> <apr-id>
    local tag="$1" aid="$2" i out n
    for i in $(seq 1 15); do
        out=$(rpc "$tag" --method approvals_pending)
        n=$(jget "$out" "sum(1 for r in d['data']['requests'] if r['id']=='$aid')")
        [ "$n" = "0" ] && return 0
        sleep 1
    done
    return 1
}

wait_session_marker() { # <tag> <sid> <marker> [tries]
    local tag="$1" sid="$2" marker="$3" tries="${4:-40}" i out
    for i in $(seq 1 "$tries"); do
        out=$(wrpc "$tag" "$sid" --method get_last_assistant_text 2>/dev/null || true)
        echo "$out" | grep -q "$marker" && { echo "$out"; return 0; }
        sleep 1
    done
    return 1
}

wait_idle() { # <tag> <worker-id>
    local tag="$1" wid="$2" i st
    for i in $(seq 1 60); do
        st=$(jget "$(rpc "$tag" --method list_workers)" \
            "next((w['status'] for w in d['data']['workers'] if w['workerId']=='$wid'), '')")
        [ "$st" = "Idle" ] && return 0
        sleep 0.5
    done
    return 1
}

echo "══════════════════════════════════════════════════════════"
echo "  Approval Bus CI（三合一）— $(date)  (root=$TEST_ROOT)"
echo "══════════════════════════════════════════════════════════"

echo "[Phase 0] build"
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1; then pass "build ion"; else fail "build ion"; exit 1; fi

# ═══════════════════════════════════════════════════════════════
echo ""
echo "═ Phase 1: M1 总线核心（host p1）"
# ═══════════════════════════════════════════════════════════════
P1_PROJ="$TEST_ROOT/p1-proj"
R1="$RANDOM"
cat > "$TEST_ROOT/p1-faux.jsonl" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$P1_PROJ/bus_ci.txt","content":"approval bus harness $R1"}}}
{"text":"done"}
{"text":"done again"}
{"text":"done third"}
JSONL
if ! start_host p1 "$TEST_ROOT/p1-faux.jsonl" "$P1_PROJ"; then fail "P1 host 启动"; exit 1; fi
pass "P1 host 启动（隔离三件套）"

echo ""
echo "G1: Pull 空表形状"
P0=$(rpc p1 --method approvals_pending)
[ "$(jget "$P0" 'd["data"]["total"]')" = "0" ] && pass "G1.1 total=0" || fail "G1.1: $P0"
[ "$(jget "$P0" 'len(d["data"]["requests"])')" = "0" ] && pass "G1.2 requests=[]" || fail "G1.2: $P0"
[ "$(jget "$P0" 'len(d["data"]["pending"])')" = "0" ] && pass "G1.3 pending=[]（schema 契约名）" || fail "G1.3: $P0"

echo ""
echo "G7(前置): subscribe --ui 后台订阅"
SUB_LOG="$TEST_ROOT/p1-sub-ui.log"
HOME="$TEST_ROOT/p1-home" ION_HOST_SOCKET="$TEST_ROOT/p1.sock" \
    "$ION_BIN" subscribe --ui > "$SUB_LOG" 2>&1 &
SUB_PID=$!
sleep 1

echo ""
echo "G2: file_snapshot 来源登记"
CREATE=$(rpc p1 --method create_session --params "{\"cwd\":\"$P1_PROJ\"}")
SID=$(jget "$CREATE" 'd["data"]["session_id"]')
if [ -n "$SID" ] && [ "$SID" != "null" ]; then pass "G2.1 create_session（${SID}）"; else
    fail "G2.1 create_session 失败: $CREATE"
    echo "---- host log tail ----"; tail -15 "$TEST_ROOT/p1.log" 2>/dev/null
    stop_host; exit 1
fi
bwrpc p1 "$SID" prompt '{"text":"write file"}' 90
ENTRY_ID=$(wait_pending_id p1 file_snapshot 20)
if [ -n "$ENTRY_ID" ]; then pass "G2.2 统一表出现 file_snapshot 条目（${ENTRY_ID}）"; else
    fail "G2.2 统一表无 file_snapshot 条目"
fi

echo ""
echo "G3: 条目形状"
P2=$(rpc p1 --method approvals_pending)
[ -n "$ENTRY_ID" ] && [ "${ENTRY_ID:0:4}" = "apr_" ] && pass "G3.1 id 统一前缀 apr_" || fail "G3.1 前缀异常: $ENTRY_ID"
[ "$(jget "$P2" "next((r['kind'] for r in d['data']['requests'] if r['id']=='$ENTRY_ID'), '')")" = "file_snapshot" ] \
    && pass "G3.2 kind=file_snapshot" || fail "G3.2 kind 异常"
[ "$(jget "$P2" "next((bool(r['summary']) for r in d['data']['requests'] if r['id']=='$ENTRY_ID'), False)")" = "True" ] \
    && pass "G3.3 summary 非空" || fail "G3.3 summary 空"
[ "$(jget "$P2" "next((isinstance(r['payload'].get('files'), list) for r in d['data']['requests'] if r['id']=='$ENTRY_ID'), False)")" = "True" ] \
    && pass "G3.4 payload.files 列表" || fail "G3.4 payload.files 缺失"
[ "$(jget "$P2" "next((isinstance(r['raisedAtMs'], int) for r in d['data']['requests'] if r['id']=='$ENTRY_ID'), False)")" = "True" ] \
    && pass "G3.5 raisedAtMs 整数" || fail "G3.5 raisedAtMs 异常"

echo ""
echo "G4: 统一 Respond 闭环 — approval_respond(approve) 路由 review_approve_all"
if [ -n "$ENTRY_ID" ]; then
    R1O=$(rpc p1 --method approval_respond --params "{\"id\":\"$ENTRY_ID\",\"decision\":\"approve\",\"reason\":\"ci g4\"}")
    if [ "$(jget "$R1O" 'd["success"]')" = "True" ]; then pass "G4.1 approval_respond success"; else fail "G4.1 失败: $R1O"; fi
    [ "$(jget "$R1O" 'd["data"]["kind"]')" = "file_snapshot" ] && pass "G4.2 data.kind=file_snapshot" || fail "G4.2: $R1O"
    [ "$(jget "$R1O" 'd["data"]["decision"]')" = "approve" ] && pass "G4.3 data.decision=approve" || fail "G4.3: $R1O"
    [ "$(jget "$R1O" 'd["data"]["remaining"]')" = "0" ] && pass "G4.4 data.remaining=0" || fail "G4.4: $R1O"
    RP=$(wrpc p1 "$SID" --method review_pending)
    [ "$(jget "$RP" 'd["data"]["summary"]["total"]')" = "0" ] && pass "G4.5 worker review_pending 归零（review_approve_all 真执行）" || fail "G4.5: $RP"
    if wait_pending_gone p1 "$ENTRY_ID"; then pass "G4.6 统一表条目消除"; else fail "G4.6 条目未消除"; fi
else
    fail "G4 skipped（无条目）"
fi

echo ""
echo "G5: 旧 API 兼容 — 第二次 write 走旧通路 review_approve"
wrpc p1 "$SID" --method call_tool --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$P1_PROJ/bus_ci2.txt\",\"content\":\"second $R1\"}}" >/dev/null 2>&1 || true
bwrpc p1 "$SID" prompt '{"text":"continue"}' 60
ENTRY2=$(wait_pending_id p1 file_snapshot 15)
if [ -n "$ENTRY2" ]; then
    pass "G5.1 第二次 write 产生新条目（${ENTRY2}）"
    PEND=$(wrpc p1 "$SID" --method review_pending)
    FIRST_PATH=$(jget "$PEND" 'd["data"]["pending"][0]["path"]')
    if [ -z "$FIRST_PATH" ] || [ "$FIRST_PATH" = "null" ]; then
        FIRST_PATH=$(jget "$PEND" 'd["data"]["requests"][0]["path"]')
    fi
    if [ -n "$FIRST_PATH" ] && [ "$FIRST_PATH" != "null" ]; then
        AO=$(wrpc p1 "$SID" --method review_approve --params "{\"path\":\"$FIRST_PATH\"}")
        echo "$AO" | grep -qi "approv" && pass "G5.2 review_approve（旧通路）生效" || fail "G5.2: $AO"
        if wait_pending_gone p1 "$ENTRY2"; then pass "G5.3 旧通路完成后镜像同步收口"; else fail "G5.3 镜像未收口"; fi
    else
        fail "G5.2 review_pending 无待审: $PEND"
    fi
else
    fail "G5.1 第二次 write 未进统一表"
fi

echo ""
echo "G6: 错误分支（对齐 schemas/rpc/subscribe/approval_respond.json）"
R2=$(rpc p1 --method approval_respond --params '{"id":"apr_nope","decision":"approve"}')
echo "$R2" | grep -q "approval not found: apr_nope" && pass "G6.1 未知 id → approval not found" || fail "G6.1: $R2"
R3=$(rpc p1 --method approval_respond --params '{"id":"apr_x","decision":"yes"}')
[ "$(jget "$R3" 'd["error"]')" = "invalid decision 'yes' (expected approve | reject)" ] \
    && pass "G6.2 非法 decision → invalid decision（fail-closed）" || fail "G6.2: $R3"
R4=$(rpc p1 --method approval_respond --params '{"id":"apr_x"}')
[ "$(jget "$R4" 'd["error"]')" = "missing params.decision" ] \
    && pass "G6.3 缺 decision → missing params.decision" || fail "G6.3: $R4"
R5=$(rpc p1 --method approval_respond --params '{"decision":"approve"}')
[ "$(jget "$R5" 'd["error"]')" = "missing params.id" ] \
    && pass "G6.4 缺 id → missing params.id" || fail "G6.4: $R5"

echo ""
echo "G7: Push — subscribe --ui 事件流"
sleep 1
kill "$SUB_PID" 2>/dev/null || true
SUB_PID=""
# subscribe CLI 输出 pretty JSON（data 字段在 ui_type 之前）；回看窗口定位 ext=host 总线事件
BUS_KIND=$(python3 - "$SUB_LOG" <<'PYEOF'
import sys, re
text = ""
try:
    with open(sys.argv[1]) as f: text = f.read()
except FileNotFoundError:
    pass
for m in re.finditer(r'"ui_type":\s*"ApprovalRequest"', text):
    window = text[max(0, m.start() - 6000):m.start() + 50]
    if re.search(r'"extension":\s*"host"', window):
        km = re.findall(r'"kind":\s*"([a-z_]+)"', window)
        ids = re.findall(r'"id":\s*"(apr_[0-9a-f]+)"', window)
        if km and ids:
            print(km[-1]); raise SystemExit
print("")
PYEOF
)
if [ -n "$BUS_KIND" ]; then
    pass "G7.1 总线 ApprovalRequest（ext=host, data.kind=${BUS_KIND} 平铺）推送可见"
else
    fail "G7.1 未收到总线 ApprovalRequest（ext=host）"
fi
grep -Eq '"ui_type": *"ApprovalRequest"' "$SUB_LOG" 2>/dev/null \
    && pass "G7.2 file-approval 原生 ApprovalRequest 与总线事件共存" \
    || fail "G7.2 sub log 无任何 ApprovalRequest"
grep -Eq '"ui_type": *"ApprovalResolved"' "$SUB_LOG" 2>/dev/null \
    && pass "G7.3 ApprovalResolved 推送可见（G4 统一应答触发）" \
    || fail "G7.3 未收到 ApprovalResolved"

echo ""
echo "G8: snapshot 水合 — approvals 全量 + pendingApprovals 兼容"
wrpc p1 "$SID" --method call_tool --params "{\"tool\":\"write\",\"args\":{\"file_path\":\"$P1_PROJ/bus_ci3.txt\",\"content\":\"third $R1\"}}" >/dev/null 2>&1 || true
bwrpc p1 "$SID" prompt '{"text":"continue"}' 60
ENTRY3=$(wait_pending_id p1 file_snapshot 15)
[ -n "$ENTRY3" ] && pass "G8.0 第三次 write 产生新 pending" || fail "G8.0 未进统一表"

SNAP=$(SID_="$SID" SOCK_="$TEST_ROOT/p1.sock" python3 -c '
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
    [ "$A_T" -ge 1 ] 2>/dev/null && pass "G8.1 snapshot 含 approvals 全量且非空（total=${A_T}）" || fail "G8.1 snapshot 缺 approvals"
    [ -n "$PA_C" ] && [ "$PA_C" != "null" ] && pass "G8.2 pendingApprovals 兼容字段保留" || fail "G8.2 pendingApprovals 丢失"
else
    fail "G8 未收到 snapshot 帧"
fi

echo ""
echo "G9: 旧 RPC 兼容"
V=$(rpc p1 --method verb_pending --params '{}')
[ "$(jget "$V" 'type(d["data"].get("pending")).__name__')" = "list" ] && pass "G9.1 verb_pending 形状不变" || fail "G9.1: $V"
VR=$(rpc p1 --method verb_review --params '{"requestId":"vapp_nonexist","approve":true}')
echo "$VR" | grep -q "verb approval not found" && pass "G9.2 verb_review 未知 id 行为不变" || fail "G9.2: $VR"
UIR=$(SEND_='{"id":"ur","method":"ui_respond","params":{"request_id":"x","response":"allow"}}' \
    SOCK_="$TEST_ROOT/p1.sock" python3 -c '
import socket, json, os, time
s = socket.socket(socket.AF_UNIX); s.settimeout(4)
s.connect(os.environ["SOCK_"])
s.sendall((os.environ["SEND_"] + "\n").encode())
buf = b""; deadline = time.time() + 3; out = []
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
' 2>/dev/null)
echo "$UIR" | grep -q "requires a prior subscribe" && pass "G9.3 ui_respond 同源绑定不变（独立连接被拒）" || fail "G9.3: $UIR"
stop_host

# ═══════════════════════════════════════════════════════════════
echo ""
echo "═ Phase 2: M2 worker Ask 托管（host p2，kind=ui_ask）"
# ═══════════════════════════════════════════════════════════════
P2_PROJ="$TEST_ROOT/p2-proj"
cat > "$TEST_ROOT/p2-faux.jsonl" <<'EOF'
{"tool_call":{"name":"bash","input":{"command":"echo M2ASKMARKER_OUT_42"}}}
{"text":"ASK_FLOW_FINISHED"}
EOF
# CommandGuard 风险模式：bash 命令含 M2ASKMARKER → 中危 → worker Ask
cat > "$TEST_ROOT/p2-extra.json" <<'JSON'
{
  "runtime": {
    "command_guard": {
      "mode": "blacklist",
      "whitelist": [],
      "risk_patterns": [
        {"pattern": "M2ASKMARKER", "level": "medium", "message": "ci ask trigger"}
      ]
    }
  }
}
JSON
if ! start_host p2 "$TEST_ROOT/p2-faux.jsonl" "$P2_PROJ"; then fail "P2 host 启动"; exit 1; fi
pass "P2 host 启动（CommandGuard 风险模式注入）"

echo ""
echo "W1: 空表基线"
PW=$(rpc p2 --method approvals_pending)
[ "$(jget "$PW" 'd["data"]["total"]')" = "0" ] && pass "W1.1 空表 total=0" || fail "W1.1: $PW"

SID2=$(jget "$(rpc p2 --method create_session --params '{}')" 'd["data"]["session_id"]')
if [ -z "$SID2" ] || [ "$SID2" = "null" ]; then fail "W0 create_session 失败"; stop_host; exit 1; fi
pass "W0 create_session → $SID2"

echo ""
echo "W2: Ask 登记统一表（双登记路径：sink 带 worker 信息胜出）"
bwrpc p2 "$SID2" prompt '{"text":"run marked cmd"}' 150
ASK_ID=""
for i in $(seq 1 30); do
    PO=$(rpc p2 --method approvals_pending)
    ASK_ID=$(jget "$PO" "next((str(r['id']) for r in d['data']['requests'] if r['kind']=='ui_ask'), '')")
    [ -n "$ASK_ID" ] && [ "$ASK_ID" != "null" ] && break
    sleep 1
done
if [ -n "$ASK_ID" ] && [ "$ASK_ID" != "null" ]; then
    pass "W2.1 Ask 登记统一表（kind=ui_ask, apr_id=${ASK_ID}）"
    PO=$(rpc p2 --method approvals_pending)
    WID=$(jget "$PO" "next((r.get('workerId') for r in d['data']['requests'] if r['id']=='$ASK_ID'), 'none')")
    [ -n "$WID" ] && [ "$WID" != "none" ] && [ "$WID" != "null" ] \
        && pass "W2.2 条目带 workerId（${WID}）——pump 登记路径胜出镜像" || fail "W2.2 workerId 缺失（镜像路径胜出为回归）"
    SESS=$(jget "$PO" "next((r.get('sessionId') for r in d['data']['requests'] if r['id']=='$ASK_ID'), 'none')")
    [ "$SESS" = "$SID2" ] && pass "W2.3 条目归会话（sessionId 对齐）" || fail "W2.3 sessionId=$SESS"
    NENT=$(jget "$PO" "sum(1 for r in d['data']['requests'] if r['kind']=='ui_ask')")
    [ "$NENT" = "1" ] && pass "W2.4 双登记路径去重（单条目）" || fail "W2.4 重复条目 x${NENT}"
else
    fail "W2.1 30s 内没等到 ui_ask 条目"
fi

echo ""
echo "W3: approval_respond(approve) 路由回 worker → 工具真执行"
if [ -n "$ASK_ID" ] && [ "$ASK_ID" != "null" ]; then
    RESP=$(rpc p2 --method approval_respond --params "{\"id\":\"$ASK_ID\",\"decision\":\"approve\"}")
    [ "$(jget "$RESP" 'd["success"]')" = "True" ] && pass "W3.1 approval_respond 投递成功" || fail "W3.1: $RESP"
    [ "$(jget "$RESP" 'd["data"]["kind"]')" = "ui_ask" ] && pass "W3.2 data.kind=ui_ask" || fail "W3.2: $RESP"
    OUT=$(wait_session_marker p2 "$SID2" "ASK_FLOW_FINISHED")
    [ -n "$OUT" ] && pass "W3.3 worker 放行后 run 完成（ASK_FLOW_FINISHED）" || fail "W3.3 会话未完成（工具疑似没放行）"
    MSGS=$(wrpc p2 "$SID2" --method get_session_messages --params '{"limit":30}')
    echo "$MSGS" | grep -q "M2ASKMARKER_OUT_42" \
        && pass "W3.4 bash 输出在会话消息中（工具执行证据）" \
        || fail "W3.4 未见工具输出证据"
else
    fail "W3 skipped（无 ui_ask 条目）"
fi

echo ""
echo "W4: 条目消除"
if [ -n "$ASK_ID" ] && [ "$ASK_ID" != "null" ]; then
    if wait_pending_gone p2 "$ASK_ID"; then pass "W4.1 AskResolved 后统一表条目消除"; else fail "W4.1 条目未消除"; fi
fi
stop_host

# ═══════════════════════════════════════════════════════════════
echo ""
echo "═ Phase 3: M3 policy map + 审批泵（host p3，沙盒池 mock 注入）"
# ═══════════════════════════════════════════════════════════════
P3_PROJ="$TEST_ROOT/p3-proj"
R3="$RANDOM"
cat > "$TEST_ROOT/p3-faux.jsonl" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$P3_PROJ/pump_c1.txt","content":"pump-c1-$R3"}}}
{"tool_call":{"name":"write","input":{"file_path":"$P3_PROJ/pump_c2.txt","content":"pump-c2-$R3"}}}
{"text":"pump round1 done"}
{"tool_call":{"name":"write","input":{"file_path":"$P3_PROJ/pump_c3.txt","content":"pump-c3-$R3"}}}
{"text":"pump round2 done"}
JSONL
# 沙盒池 mock（ION_REMOTE_WORKERS 注入；不 ssh——只在 config/档案层生效）：
#   ci-map  = map 形态档案（file_snapshot+remote_verb auto，ui_ask ask）
#   ci-auto = 旧字符串形态 auto_approve（兼容性对照）
SANDBOX_JSON='{"ci-map":{"hostname":"ci-map.invalid","user":"ci","approval_policy":{"file_snapshot":"auto","ui_ask":"ask","remote_verb":"auto"}},"ci-auto":{"hostname":"ci-auto.invalid","user":"ci","approval_policy":"auto_approve"}}'
if ! start_host p3 "$TEST_ROOT/p3-faux.jsonl" "$P3_PROJ" "ION_REMOTE_WORKERS=$SANDBOX_JSON"; then
    fail "P3 host 启动"; exit 1
fi
pass "P3 host 启动（沙盒池 mock 注入）"

echo ""
echo "P1x: policy map 形态（config 档案 + sandbox_policy RPC）"
GP=$(rpc p3 --method sandbox_policy --params '{"host":"ci-map"}')
[ "$(jget "$GP" 'd["data"]["profile"]["file_snapshot"]')" = "auto" ] && [ "$(jget "$GP" 'd["data"]["profile"]["ui_ask"]')" = "ask" ] \
    && pass "P1.1 map 档案 GET：profile 三来源解析视图（config untagged）" || fail "P1.1: $GP"
[ "$(jget "$GP" 'd["data"]["effective"]["file_snapshot"]')" = "auto" ] \
    && pass "P1.2 map 档案生效链 effective" || fail "P1.2: $GP"
GPA=$(rpc p3 --method sandbox_policy --params '{"host":"ci-auto"}')
[ "$(jget "$GPA" 'd["data"]["profile"]')" = "auto_approve" ] && [ "$(jget "$GPA" 'd["data"]["effective"]')" = "auto_approve" ] \
    && pass "P1.3 旧字符串档案兼容（零破坏）" || fail "P1.3: $GPA"
SPM=$(rpc p3 --method sandbox_policy --params '{"host":"ci-map","policy":{"file_snapshot":"ask","remote_verb":"auto"}}')
[ "$(jget "$SPM" 'd["success"]')" = "True" ] && [ "$(jget "$SPM" 'd["data"]["effective"]["remote_verb"]')" = "auto" ] \
    && pass "P1.4 SET host map（对象参数）→ 归一化生效视图" || fail "P1.4: $SPM"
BADK=$(rpc p3 --method sandbox_policy --params '{"host":"ci-map","policy":{"bogus_kind":"auto"}}')
[ "$(jget "$BADK" 'd["success"]')" = "False" ] && echo "$BADK" | grep -q "unknown policy kind" \
    && pass "P1.5 非法 kind key → 严格拒绝（fail-closed）" || fail "P1.5: $BADK"
BADS=$(rpc p3 --method sandbox_policy --params '{"host":"ci-map","policy":"default"}')
[ "$(jget "$BADS" 'd["success"]')" = "True" ] && pass "P1.6 恢复 default（清覆盖）" || fail "P1.6: $BADS"

echo ""
echo "P2x: 泵实测（worker 级 map 覆盖驱动，file_snapshot=auto）"
UI_LOG="$TEST_ROOT/p3-ui.log"
HOME="$TEST_ROOT/p3-home" ION_HOST_SOCKET="$TEST_ROOT/p3.sock" \
    "$ION_BIN" subscribe --ui > "$UI_LOG" 2>&1 &
SUB_PID=$!
sleep 1

CW=$(rpc p3 --method create_worker --params "{\"agent\":\"build\",\"project_path\":\"$P3_PROJ\"}")
CWID=$(jget "$CW" 'd["data"]["workerId"]')
CSID=$(jget "$CW" 'd["data"]["sessionId"]')
if [ -n "$CWID" ] && [ "$CWID" != "null" ]; then pass "P2.1 create_worker → $CWID / $CSID"; else
    fail "P2.1 create_worker: $CW"; stop_host; exit 1
fi
SPW=$(rpc p3 --method sandbox_policy --params "{\"worker\":\"$CWID\",\"policy\":{\"file_snapshot\":\"auto\",\"ui_ask\":\"ask\"}}")
[ "$(jget "$SPW" 'd["data"]["effective"]["file_snapshot"]')" = "auto" ] \
    && pass "P2.2 SET worker map（file_snapshot auto / ui_ask ask）" || fail "P2.2: $SPW"

bwrpc p3 "$CSID" prompt '{"text":"写文件并回报"}' 90
if wait_idle p3 "$CWID"; then pass "P2.3 faux 轮1 完成"; else fail "P2.3 faux 轮1 未完成"; fi

ui_wait() { # <ui_type> [tries]
    local t="$1" tries="${2:-40}" i n
    for i in $(seq 1 "$tries"); do
        n=$(grep -c "\"ui_type\": *\"$t\"" "$UI_LOG" 2>/dev/null || true)
        [ "${n:-0}" -gt 0 ] && return 0
        sleep 0.3
    done
    return 1
}
ui_wait SandboxAutoApproved && pass "P2.4 SandboxAutoApproved 可见（遗留事件保留）" || fail "P2.4 无 SandboxAutoApproved"
ui_wait ApprovalResolved && pass "P2.5 ApprovalResolved 统一事件可见" || fail "P2.5 无 ApprovalResolved"
FIRST_RESOLVED=$(python3 - "$UI_LOG" <<'PYEOF'
import sys, re, os
text = open(sys.argv[1]).read() if os.path.exists(sys.argv[1]) else ""
# subscribe --ui 输出 pretty JSON，data 字段在 ui_type 之前 → 回看窗口
for m in re.finditer(r'"ui_type":\s*"ApprovalResolved"', text):
    window = text[max(0, m.start() - 3000):m.start()]
    km = re.findall(r'"kind":\s*"([a-z_]+)"', window)
    bm = re.findall(r'"by":\s*"([a-z]+)"', window)
    if km and bm:
        print(f"{km[-1]}/{bm[-1]}"); raise SystemExit
print("/")
PYEOF
)
[ "$FIRST_RESOLVED" = "file_snapshot/pump" ] \
    && pass "P2.6 首个 ApprovalResolved（kind=file_snapshot, by=pump）" || fail "P2.6 形态: $FIRST_RESOLVED"
CRP=$(wrpc p3 "$CSID" --method review_pending)
[ "$(jget "$CRP" 'd["data"]["summary"]["total"]')" = "0" ] && pass "P2.7 review_pending 归零（map 内 auto 放行）" || fail "P2.7: $CRP"
# 总线同步收口：泵放行后统一表无 file_snapshot 残留
sleep 2
BUS_FS=$(jget "$(rpc p3 --method approvals_pending)" "sum(1 for r in d['data']['requests'] if r['kind']=='file_snapshot')")
[ "$BUS_FS" = "0" ] && pass "P2.8 总线条目同步收口（by=pump，无 user 归因残留）" || fail "P2.8 总线残留 x${BUS_FS}"

echo ""
echo "P3x: ask 对照轮（per-kind ask 不触发泵）+ 统一 Respond 收尾"
SPW2=$(rpc p3 --method sandbox_policy --params "{\"worker\":\"$CWID\",\"policy\":{\"file_snapshot\":\"ask\",\"remote_verb\":\"auto\"}}")
[ "$(jget "$SPW2" 'd["data"]["effective"]["file_snapshot"]')" = "ask" ] \
    && pass "P3.1 改 map（file_snapshot=ask）→ 生效视图翻转" || fail "P3.1: $SPW2"
N_BEFORE=$(grep -c '"ui_type": *"ApprovalResolved"' "$UI_LOG" 2>/dev/null || echo 0)
bwrpc p3 "$CSID" prompt '{"text":"再写一个文件"}' 90
if wait_idle p3 "$CWID"; then pass "P3.2 对照轮完成"; else fail "P3.2 对照轮未完成"; fi
sleep 2
N_AFTER=$(grep -c '"ui_type": *"ApprovalResolved"' "$UI_LOG" 2>/dev/null || echo 0)
[ "$N_AFTER" -eq "$N_BEFORE" ] && pass "P3.3 对照轮无新放行事件（ask 不触发泵）" || fail "P3.3 事件数 ${N_BEFORE}→${N_AFTER}"
CRP2=$(wrpc p3 "$CSID" --method review_pending)
[ "$(jget "$CRP2" 'd["data"]["summary"]["total"]')" -ge 1 ] 2>/dev/null \
    && pass "P3.4 pending 保留（ask 人工审批语义）" || fail "P3.4: $CRP2"
ENTRY4=$(wait_pending_id p3 file_snapshot 15)
if [ -n "$ENTRY4" ]; then
    RES=$(rpc p3 --method approval_respond --params "{\"id\":\"$ENTRY4\",\"decision\":\"approve\"}")
    [ "$(jget "$RES" 'd["success"]')" = "True" ] \
        && pass "P3.5 统一 Respond 放行 ask 保留的 pending（单一路由收尾）" || fail "P3.5: $RES"
    CRP3=$(wrpc p3 "$CSID" --method review_pending)
    [ "$(jget "$CRP3" 'd["data"]["summary"]["total"]')" = "0" ] && pass "P3.6 review_pending 归零" || fail "P3.6: $CRP3"
    if wait_pending_gone p3 "$ENTRY4"; then pass "P3.7 总线条目消除"; else fail "P3.7 条目未消除"; fi
else
    fail "P3.5 对照轮 pending 未进统一表"
fi
kill "$SUB_PID" 2>/dev/null; SUB_PID=""

echo ""
echo "P4x: approvals_pending kind 枚举与字段（M3 Group D 预留块激活）"
PEN=$(rpc p3 --method approvals_pending)
KINDS_OK=$(jget "$PEN" "all(r['kind'] in ('ui_ask','file_snapshot','remote_verb') for r in d['data']['requests'])")
[ "$KINDS_OK" = "True" ] && pass "P4.1 pending[].kind 枚举合法（对齐 schema）" || fail "P4.1: $PEN"
FIELDS_OK=$(jget "$PEN" "all(all(k in r for k in ('id','kind','sessionId','summary','payload','raisedAtMs')) for r in d['data']['pending'])")
[ "$FIELDS_OK" = "True" ] && pass "P4.2 ApprovalEntry 必填字段齐全（requests 与 pending 同形）" || fail "P4.2: $PEN"
stop_host

echo ""
echo "══════════════════════════════════════════════════════════"
echo "  Approval Bus CI（三合一）结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
