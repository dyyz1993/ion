#!/usr/bin/env bash
# socket_security_ci.sh — host socket 三项安全加固验证（raw socket 版）
#
# 验证三件事：
#   1. peercred 同 uid 校验：同 uid 连接正常工作（回归）；
#      不同 uid 本机无法模拟（setuid），判定函数由 src/bin/ion.rs 单测覆盖
#      （cargo test --bin ion socket_peer_allowed unix_socket_same_uid）
#   2. get_session_messages 任意路径读取：sessions_dir 外路径 / 目录内 symlink
#      指向外部文件 → 拒绝（success:false + host 日志 security 告警）；
#      ION_SESSION_DIR 目录内路径 → 正常直读（ION_SESSION_DIR 覆盖场景）
#   3. verb_review approve 缺省：不再缺省放行（默认值 false 由 bin 单测覆盖；
#      本组验证 RPC 表面语义：缺 requestId 报错、显式 approve 透传）
#
# 场景：ion serve（场景 3）+ python3 raw AF_UNIX socket（不依赖 ion rpc 客户端）。
# 隔离三件套：私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR——绝不触碰真实 ~/.ion。
# 进程清理：只 kill 本脚本记录的精确 HOST_PID（铁律：禁止 pkill 宽泛匹配）。
#
# 用法：bash tests/socket_security_ci.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }

jf() { jq -r "$1" 2>/dev/null; }

# ── 隔离三件套 ──
WORK="$(mktemp -d /tmp/ion-sock-sec-ci-XXXXXX)"
export HOME="$WORK/home"
export ION_SESSION_DIR="$WORK/sessions"
export ION_HOST_SOCKET="$WORK/host.sock"
mkdir -p "$HOME" "$ION_SESSION_DIR"

HOST_PID=""
HOST_LOG="$WORK/host.log"

cleanup() {
    # 只杀本脚本启动的 host（精确 PID），绝不 pkill
    if [ -n "$HOST_PID" ]; then
        kill "$HOST_PID" 2>/dev/null
        wait "$HOST_PID" 2>/dev/null
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# ── raw AF_UNIX socket RPC 客户端（同 scripts/sandbox_monitor.sh 模式）──
rpc_raw() {
    SOCK_="$ION_HOST_SOCKET" JSON_="$1" python3 -c "
import socket, os, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(15)
s.connect(os.environ['SOCK_'])
s.sendall(os.environ['JSON_'].encode() + b'\n')
buf = b''
while b'\n' not in buf:
    d = s.recv(65536)
    if not d: break
    buf += d
s.close()
sys.stdout.write(buf.decode())
" 2>/dev/null
}

echo "── 启动隔离 host (HOME=$HOME) ──"
ION_FAUX_REPLY="socket sec ci" "$ION_BIN" serve > "$HOST_LOG" 2>&1 &
HOST_PID=$!

READY=0
for _ in $(seq 1 30); do
    sleep 1
    kill -0 "$HOST_PID" 2>/dev/null || break
    R=$(rpc_raw '{"id":"w0","method":"list_sessions","params":{}}' || true)
    if echo "$R" | grep -q '"success":true'; then READY=1; break; fi
done
[ "$READY" = "1" ]; check $? "A0 host 启动 + 同 uid raw socket 连通（peercred 回归）"
[ "$READY" = "1" ] || { echo "host log:"; tail -20 "$HOST_LOG"; exit 1; }

echo ""
echo "── Group A: peercred 同 uid 正常路径回归 ──"

# A1 完整 RPC 往返：list_sessions 返回 JSON 结构
R=$(rpc_raw '{"id":"a1","method":"list_sessions","params":{}}')
OK=$(echo "$R" | jf '.success')
[ "$OK" = "true" ]; check $? "A1 list_sessions 同 uid RPC 往返成功 (success=$OK)"

# A2 session id 形式（非路径）错误语义不受影响：不存在的 id → not found（非路径拒绝）
R=$(rpc_raw '{"id":"a2","method":"get_session_messages","params":{"session":"sess_no_such_ci"}}')
OK=$(echo "$R" | jf '.success')
ERR=$(echo "$R" | jf '.error')
echo "$ERR" | grep -q "not found on disk"; check $? "A2 id 形式查无此会话报 not found（id 直查路径不受影响）"

echo ""
echo "── Group B: get_session_messages 任意路径读取约束 ──"

# B1 sessions_dir 外的真实 JSONL → 拒绝
OUTSIDE="$WORK/outside.jsonl"
printf '{"cwd":"/nowhere","id":"evil","type":"session","version":3}\n' > "$OUTSIDE"
R=$(rpc_raw "{\"id\":\"b1\",\"method\":\"get_session_messages\",\"params\":{\"session\":\"$OUTSIDE\"}}")
OK=$(echo "$R" | jf '.success')
[ "$OK" = "false" ]; check $? "B1 sessions_dir 外路径被拒 (success=$OK)"
grep -q "outside sessions dir rejected" "$HOST_LOG"
check $? "B1b host 日志留下 security 拒绝记录"

# B2 目录内 symlink 指向外部文件（逃逸）→ 拒绝
SECRET="$WORK/secret.jsonl"
printf '{"cwd":"/nowhere","id":"secret","type":"session","version":3}\n' > "$SECRET"
LINK="$ION_SESSION_DIR/innocent.jsonl"
ln -s "$SECRET" "$LINK"
R=$(rpc_raw "{\"id\":\"b2\",\"method\":\"get_session_messages\",\"params\":{\"session\":\"$LINK\"}}")
OK=$(echo "$R" | jf '.success')
[ "$OK" = "false" ]; check $? "B2 symlink 逃逸被拒（canonicalize 解析后越界）(success=$OK)"

# B3 /etc 类系统文件路径（典型攻击样本）→ 拒绝
R=$(rpc_raw '{"id":"b3","method":"get_session_messages","params":{"session":"/etc/hosts"}}')
OK=$(echo "$R" | jf '.success')
[ "$OK" = "false" ]; check $? "B3 /etc/hosts 被拒 (success=$OK)"

# B4 ION_SESSION_DIR 目录内的合法会话文件（路径形式）→ 正常直读
# （验证 ION_SESSION_DIR 覆盖场景：约束跟着 env 走，不写死 ~/.ion）
FIXTURE="$ION_SESSION_DIR/ci_sec_sess.jsonl"
cat > "$FIXTURE" <<'EOF'
{"cwd":"/tmp/ion-sock-sec-ci","id":"ci_sec_sess","parentSession":null,"timestamp":"2026-09-15T00:00:00.000Z","type":"session","version":3}
{"id":"u1","message":{"User":{"content":[{"Text":{"text":"sec question"}}],"role":"user","source":"prompt","timestamp":1786249005773}},"parentId":"ci_sec_sess","timestamp":"2026-09-15T00:00:01.000Z","type":"message"}
{"id":"a1","message":{"Assistant":{"api":"openai-completions","content":[{"Text":{"text":"sec answer"}}],"role":"assistant","source":"api","timestamp":1786249006773}},"parentId":"u1","timestamp":"2026-09-15T00:00:02.000Z","type":"message"}
EOF
R=$(rpc_raw "{\"id\":\"b4\",\"method\":\"get_session_messages\",\"params\":{\"session\":\"$FIXTURE\"}}")
OK=$(echo "$R" | jf '.success')
N=$(echo "$R" | jf '.data.messages | length')
[ "$OK" = "true" ] && [ "$N" = "2" ]; check $? "B4 目录内会话文件路径形式正常直读 (success=$OK n=$N)"

echo ""
echo "── Group C: verb_review approve 缺省语义 ──"

# C1 缺 requestId → 明确报错（RPC 表面完整）
R=$(rpc_raw '{"id":"c1","method":"verb_review","params":{}}')
OK=$(echo "$R" | jf '.success')
ERR=$(echo "$R" | jf '.error')
echo "$ERR" | grep -q "missing params.requestId"
check $? "C1 缺 requestId 报 missing params.requestId (err=$ERR)"

# C2 approve 缺省（修复后默认 false）+ 未知 id → not found（不静默放行、不崩溃）
R=$(rpc_raw '{"id":"c2","method":"verb_review","params":{"requestId":"vapp_none"}}')
OK=$(echo "$R" | jf '.success')
ERR=$(echo "$R" | jf '.error')
echo "$ERR" | grep -q "verb approval not found"
check $? "C2 缺省 approve + 未知 id → not found（默认值 false 由 bin 单测覆盖）"

# C3 显式 approve:true 透传 + 未知 id → not found
R=$(rpc_raw '{"id":"c3","method":"verb_review","params":{"requestId":"vapp_none","approve":true}}')
ERR=$(echo "$R" | jf '.error')
echo "$ERR" | grep -q "verb approval not found"
check $? "C3 显式 approve:true 透传正常"

# C4 verb_pending RPC 表面可用
R=$(rpc_raw '{"id":"c4","method":"verb_pending","params":{}}')
OK=$(echo "$R" | jf '.success')
PENDING=$(echo "$R" | jf '.data.pending | length')
[ "$OK" = "true" ] && [ "$PENDING" = "0" ]; check $? "C4 verb_pending 可用且为空 (pending=$PENDING)"

echo ""
echo "══ 结果: $PASS passed / $FAIL failed ══"
[ "$FAIL" -eq 0 ]
