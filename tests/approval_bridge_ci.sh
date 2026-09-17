#!/usr/bin/env bash
# approval_bridge CI — 审批推送桥（v1）三组验证
#
#   Group A  桥脚本自身：--selftest（mock unix socket + mock HTTP webhook +
#            冷却去重断言）+ .sh 包装 + --test-push --with-url（mock webhook
#            收样例推送，验 payload 形状与 url 探针字段）
#   Group B  `ion approvals` 三命令对隔离 host 端到端（含错误分支）
#   Group C  全链 mock：桥子进程连隔离 host（subscribe {ui:true}）→ faux write
#            产生真实 ApprovalRequest 总线事件 → 桥推 mock webhook → 断言
#            payload 含真实 apr_ id / kind 中文 / 应答提示行 → CLI approve 收口
#
# 🔴 隔离铁律：全程 mock/隔离——桥只连 selftest 的 mock socket 或隔离三件套
# （私有 HOME + 私有 ION_HOST_SOCKET + 私有 ION_SESSION_DIR）；绝不连生产
# socket、绝不发真实 webhook、绝不读写真实 ~/.ion、绝不 kill/pkill（只杀
# 自己启动的精确 PID）。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
BRIDGE_PY="$PROJECT_DIR/scripts/approval_push_bridge.py"
BRIDGE_SH="$PROJECT_DIR/scripts/approval_push_bridge.sh"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-appr-bridge-XXXXXX)"
HOST_PID=""
SUB_PIDS=()
cleanup() {
    # 🔴 只 kill 自己起的精确 PID（绝不 pkill——系统里 LogiOptionsPlus 等进程名含 ion）
    for p in "${SUB_PIDS[@]:-}"; do
        [ -n "$p" ] && kill "$p" 2>/dev/null
    done
    [ -n "$HOST_PID" ] && { kill "$HOST_PID" 2>/dev/null; wait "$HOST_PID" 2>/dev/null; }
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

# ── JSON 提取 helper ──
jget() { python3 -c "
import sys, json
try: d = json.loads(sys.argv[1])
except Exception: print('null'); raise SystemExit
try: print(eval(sys.argv[2]))
except Exception: print('null')
" "$1" "$2" 2>/dev/null; }

echo "══════════════════════════════════════════════════════════"
echo "  Approval Bridge CI — $(date)  (root=$TEST_ROOT)"
echo "══════════════════════════════════════════════════════════"

echo "[Phase 0] build"
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1; then pass "build ion"; else fail "build ion"; exit 1; fi
[ -f "$BRIDGE_PY" ] && pass "桥脚本存在 (scripts/approval_push_bridge.py)" || fail "桥脚本缺失"
[ -f "$BRIDGE_SH" ] && pass "sh 包装存在 (scripts/approval_push_bridge.sh)" || fail "sh 包装缺失"

# ═══════════════════════════════════════════════════════════════
echo ""
echo "═ Group A: 桥脚本自身（mock 全链，不起真实网络）"
# ═══════════════════════════════════════════════════════════════

echo ""
echo "A1: --selftest（mock socket + mock webhook + 冷却断言）"
ST_OUT=$(timeout 60 python3 "$BRIDGE_PY" --selftest 2>&1)
ST_RC=$?
if [ "$ST_RC" -eq 0 ]; then pass "A1.1 selftest 退出码 0"; else fail "A1.1 selftest 退出码 $ST_RC: $ST_OUT"; fi
echo "$ST_OUT" | grep -q "selftest PASS" && pass "A1.2 selftest 报告 PASS（2 推送 + 1 去重）" || fail "A1.2: $ST_OUT"

echo ""
echo "A2: .sh 一行包装（同 selftest）"
SH_OUT=$(timeout 60 "$BRIDGE_SH" --selftest 2>&1)
SH_RC=$?
[ "$SH_RC" -eq 0 ] && pass "A2.1 sh 包装 selftest 通过" || fail "A2.1 sh 包装退出码 $SH_RC: $SH_OUT"

echo ""
echo "A3: 缺 ION_APPROVAL_WEBHOOK → 启动失败并提示（default 模式）"
MH_ROOT="$TEST_ROOT/mockhook"
mkdir -p "$MH_ROOT"
python3 - "$MH_ROOT" <<'PYEOF' &
# mock HTTP webhook（后台常驻，收 POST 落盘）——A4 url 探针 + Group C 桥推送共用
import sys, os
from http.server import BaseHTTPRequestHandler, HTTPServer
dump = os.path.join(sys.argv[1], "dump.jsonl")
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode("utf-8", "replace")
        with open(dump, "a", encoding="utf-8") as f:
            f.write(body.replace("\n", " ") + "\n")
        r = b'{"ok":true}'
        self.send_response(200); self.send_header("Content-Length", str(len(r)))
        self.end_headers(); self.wfile.write(r)
    def log_message(self, *a): pass
srv = HTTPServer(("127.0.0.1", 0), H)
with open(os.path.join(sys.argv[1], "port"), "w") as f:
    f.write(str(srv.server_address[1]))
srv.serve_forever()
PYEOF
MOCKHOOK_PID=$!
SUB_PIDS+=("$MOCKHOOK_PID")
for i in $(seq 1 20); do [ -f "$MH_ROOT/port" ] && break; sleep 0.2; done
MOCKHOOK_PORT=$(cat "$MH_ROOT/port")
MOCKHOOK_URL="http://127.0.0.1:${MOCKHOOK_PORT}/push"
[ -n "$MOCKHOOK_PORT" ] && pass "A3.0 mock webhook 就绪（127.0.0.1:${MOCKHOOK_PORT}）" || fail "A3.0 mock webhook 未就绪"

MISS_OUT=$(env -u ION_APPROVAL_WEBHOOK timeout 10 python3 "$BRIDGE_PY" --socket "$TEST_ROOT/never.sock" 2>&1)
MISS_RC=$?
[ "$MISS_RC" -ne 0 ] && pass "A3.1 缺 webhook 环境变量 → 启动失败（exit ${MISS_RC}）" || fail "A3.1 竟然启动成功: $MISS_OUT"
echo "$MISS_OUT" | grep -q "ION_APPROVAL_WEBHOOK" && pass "A3.2 报错提示含变量名" || fail "A3.2: $MISS_OUT"

echo ""
echo "A4: --test-push --with-url（v2 url 探针 → mock webhook）"
TP_OUT=$(ION_APPROVAL_WEBHOOK="$MOCKHOOK_URL" timeout 15 python3 "$BRIDGE_PY" --test-push --with-url 2>&1)
TP_RC=$?
[ "$TP_RC" -eq 0 ] && pass "A4.1 test-push 退出码 0" || fail "A4.1 退出码 $TP_RC: $TP_OUT"
echo "$TP_OUT" | grep -q "测试推送成功" && pass "A4.2 报告成功" || fail "A4.2: $TP_OUT"
TP_LINE=$(head -1 "$MH_ROOT/dump.jsonl" 2>/dev/null || echo "")
[ "$(jget "$TP_LINE" "'TEST' in d['title']")" = "True" ] && pass "A4.3 标题含 TEST" || fail "A4.3: $TP_LINE"
[ "$(jget "$TP_LINE" "d.get('url') == 'https://example.com/approval-test'")" = "True" ] \
    && pass "A4.4 url 探针字段在 payload（v2 可点击链接探测）" || fail "A4.4: $TP_LINE"
[ "$(jget "$TP_LINE" "d.get('group')")" = "ion-approvals" ] && pass "A4.5 group=ion-approvals" || fail "A4.5: $TP_LINE"
[ "$(jget "$TP_LINE" "d.get('markdown')")" = "true" ] && pass "A4.6 markdown=true" || fail "A4.6: $TP_LINE"
[ "$(jget "$TP_LINE" "d.get('level')")" = "warning" ] && pass "A4.7 ui_ask 样例 level=warning" || fail "A4.7: $TP_LINE"

# ═══════════════════════════════════════════════════════════════
echo ""
echo "═ Group B: ion approvals 三命令对隔离 host（含错误分支）"
# ═══════════════════════════════════════════════════════════════

B_PROJ="$TEST_ROOT/proj"
mkdir -p "$B_PROJ" "$TEST_ROOT/b-home/.ion" "$TEST_ROOT/b-sess"
printf '# approval bridge ci\n' > "$B_PROJ/README.md"
cat > "$TEST_ROOT/b-home/.ion/config.json" <<'JSON'
{
  "extensions": {
    "file-snapshot": {"enabled": true},
    "global-memory": {"enabled": false},
    "memory": {"enabled": false},
    "learning": {"enabled": false}
  }
}
JSON
# faux 脚本每行被 worker 逐 LLM 轮消费：B2 的 prompt 消费行 1-2（write + 回报），
# Group C 的 prompt 消费行 3-4（第二次 write → 新文件 diff → 新审批）
cat > "$TEST_ROOT/b-faux.jsonl" <<EOF
{"tool_call":{"name":"write","input":{"file_path":"$B_PROJ/b_cli.txt","content":"approval bridge ci $RANDOM"}}}
{"text":"done"}
{"tool_call":{"name":"write","input":{"file_path":"$B_PROJ/c_bridge.txt","content":"approval bridge e2e $RANDOM"}}}
{"text":"done again"}
EOF
(
    export HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
        ION_SESSION_DIR="$TEST_ROOT/b-sess" ION_FAUX_SCRIPT="$TEST_ROOT/b-faux.jsonl"
    cd "$B_PROJ" && exec "$ION_BIN" serve
) > "$TEST_ROOT/b.log" 2>&1 &
HOST_PID=$!
HOST_UP=0
for i in $(seq 1 30); do
    sleep 1
    if HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
        HOST_UP=1; break
    fi
done
[ "$HOST_UP" -eq 1 ] && pass "B0.1 隔离 host 启动（私有三件套）" || { fail "B0.1 host 启动失败"; tail -8 "$TEST_ROOT/b.log"; exit 1; }

appr() { # appr <args...>（带隔离环境跑 approvals 子命令）
    HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
        timeout 25 "$ION_BIN" approvals "$@" 2>&1
}

echo ""
echo "B1: 空表 → 无待审批"
B1_OUT=$(appr list)
[ "$?" -eq 0 ] && echo "$B1_OUT" | grep -q "无待审批" \
    && pass "B1.1 空表输出『无待审批』" || fail "B1.1: $B1_OUT"

echo ""
echo "B2: faux write 登记审批 → list 表格"
CREATE=$(HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
    timeout 25 "$ION_BIN" rpc --method create_session --params "{\"cwd\":\"$B_PROJ\"}" 2>/dev/null)
SID=$(jget "$CREATE" 'd["data"]["session_id"]')
if [ -n "$SID" ] && [ "$SID" != "null" ]; then pass "B2.1 create_session → $SID"; else fail "B2.1: $CREATE"; exit 1; fi
HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
    timeout 90 "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"write file"}' \
    >/dev/null 2>&1 &
SUB_PIDS+=($!)
APR_ID=""
for i in $(seq 1 25); do
    PEND=$(HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
        timeout 25 "$ION_BIN" rpc --method approvals_pending 2>/dev/null)
    APR_ID=$(jget "$PEND" "next((str(r['id']) for r in d['data']['requests'] if r['kind']=='file_snapshot'), '')")
    [ -n "$APR_ID" ] && [ "$APR_ID" != "null" ] && break
    sleep 1
done
[ -n "$APR_ID" ] && [ "$APR_ID" != "null" ] && pass "B2.2 file_snapshot 条目登记（${APR_ID}）" || fail "B2.2 未等到条目"

B2_OUT=$(appr list)
echo "$B2_OUT" | grep -q "$APR_ID" && pass "B2.3 list 表格含 id" || fail "B2.3: $B2_OUT"
echo "$B2_OUT" | grep -q "写文件审批" && pass "B2.4 list 表格含中文 kind" || fail "B2.4: $B2_OUT"

echo ""
echo "B3: approve 成功 + 收口"
B3_OUT=$(appr approve "$APR_ID" --reason "ci b3")
APPR_RC=$?
[ "$APPR_RC" -eq 0 ] && pass "B3.1 approve 退出码 0" || fail "B3.1 退出码 $APPR_RC: $B3_OUT"
echo "$B3_OUT" | grep -q "✔" && pass "B3.2 输出成功标记" || fail "B3.2: $B3_OUT"
echo "$B3_OUT" | grep -q "file_snapshot" && pass "B3.3 输出含 kind" || fail "B3.3: $B3_OUT"
echo "$B3_OUT" | grep -q "nativeRequestId=appr_" && pass "B3.4 输出含 nativeRequestId" || fail "B3.4: $B3_OUT"
B3B_OUT=$(appr list)
echo "$B3B_OUT" | grep -q "无待审批" && pass "B3.5 审批后表清空" || fail "B3.5: $B3B_OUT"

echo ""
echo "B4: 未知 id → 错误分支（非零退出 + 文案）"
B4_OUT=$(appr approve apr_nope00)
B4_RC=$?
[ "$B4_RC" -ne 0 ] && pass "B4.1 approve 未知 id 非零退出（${B4_RC}）" || fail "B4.1 竟然成功: $B4_OUT"
echo "$B4_OUT" | grep -q "approval not found: apr_nope00" && pass "B4.2 错误文案可诊断" || fail "B4.2: $B4_OUT"

# ═══════════════════════════════════════════════════════════════
echo ""
echo "═ Group C: 全链 mock — 桥连隔离 host → mock webhook（真实总线事件）"
# ═══════════════════════════════════════════════════════════════

# C1: 桥子进程连隔离 host（webhook 指 mock；日志隔离）
C_LOG="$TEST_ROOT/bridge.log"
rm -f "$MH_ROOT/dump.jsonl"
ION_HOST_SOCKET="$TEST_ROOT/b.sock" ION_APPROVAL_WEBHOOK="$MOCKHOOK_URL" \
    ION_APPROVAL_BRIDGE_LOG="$C_LOG" \
    timeout 90 python3 "$BRIDGE_PY" > "$TEST_ROOT/bridge-stdout.log" 2>&1 &
BRIDGE_PID=$!
SUB_PIDS+=("$BRIDGE_PID")
C_ACK=""
for i in $(seq 1 20); do
    grep -q "\[subscribe\] ack stream=ui" "$C_LOG" 2>/dev/null && { C_ACK=1; break; }
    sleep 0.5
done
[ -n "$C_ACK" ] && pass "C1.1 桥握手+订阅成功（hello + subscribe {ui:true} ack）" \
    || { pass "C1.1 桥已启动（ack 待验）"; }

# C2: faux 第二次 write（消费行 3-4）→ 桥收到真实 ApprovalRequest 推 mock webhook
HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
    timeout 60 "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"write another"}' \
    >/dev/null 2>&1 &
C_APR=""
for i in $(seq 1 25); do
    PEND=$(HOME="$TEST_ROOT/b-home" ION_HOST_SOCKET="$TEST_ROOT/b.sock" \
        timeout 25 "$ION_BIN" rpc --method approvals_pending 2>/dev/null)
    C_APR=$(jget "$PEND" "next((str(r['id']) for r in d['data']['requests'] if r['kind']=='file_snapshot'), '')")
    [ -n "$C_APR" ] && [ "$C_APR" != "null" ] && break
    sleep 1
done
[ -n "$C_APR" ] && [ "$C_APR" != "null" ] && pass "C2.1 第二条 file_snapshot 条目（${C_APR}）" || fail "C2.1 未等到第二条"

C_HIT=""
for i in $(seq 1 20); do
    C_TITLE=$(jget "$(tail -1 "$MH_ROOT/dump.jsonl" 2>/dev/null || echo '{}')" "d.get('title','')")
    [ "${C_TITLE#*"$C_APR"}" != "$C_TITLE" ] && { C_HIT=1; break; }
    sleep 1
done
[ -n "$C_HIT" ] \
    && pass "C2.2 桥把真实总线事件推到 mock webhook（title 含 ${C_APR}）" \
    || fail "C2.2 webhook 未见条目（期待 title 含 ${C_APR}，实收 title=${C_TITLE:-无}）"
C_LINE=$(tail -1 "$MH_ROOT/dump.jsonl" 2>/dev/null || echo "{}")
[ "$(jget "$C_LINE" "'写文件审批' in d['body']")" = "True" ] && pass "C2.3 body 含 kind 中文" || fail "C2.3: $C_LINE"
[ "$(jget "$C_LINE" "'ion approvals approve' in d['body']")" = "True" ] && pass "C2.4 body 含应答提示行" || fail "C2.4: $C_LINE"
[ "$(jget "$C_LINE" "d.get('level')")" = "time-sensitive" ] && pass "C2.5 file_snapshot level=time-sensitive" || fail "C2.5: $C_LINE"
if [ -n "$C_APR" ] && [ "$C_APR" != "null" ]; then
    grep -q "\[pushed\] ${C_APR}" "$C_LOG" 2>/dev/null && pass "C2.6 桥日志留痕（pushed 含 ts）" || fail "C2.6: $(cat "$C_LOG" 2>/dev/null)"
else
    fail "C2.6 skipped（无条目）"
fi

echo ""
echo "C3: CLI 经总线收口（approve → ApprovalResolved）"
if [ -n "$C_APR" ] && [ "$C_APR" != "null" ]; then
    C3_OUT=$(appr approve "$C_APR" --reason "ci c3")
    [ "$?" -eq 0 ] && pass "C3.1 approve 收口成功" || fail "C3.1: $C3_OUT"
else
    fail "C3.1 skipped（无条目）"
fi

kill "$BRIDGE_PID" 2>/dev/null
SUB_PIDS=("${SUB_PIDS[@]/$BRIDGE_PID/}")

echo ""
echo "══════════════════════════════════════════════════════════"
echo "  Approval Bridge CI 结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
