#!/bin/bash
# sandbox_stateless_ci.sh — 沙盒池「无状态五步试炼」命令行验证
# 设计文档: docs/design/SANDBOX_POOL.md §4.1
#
# 五步: ① env 注入单端点 + 起隔离 host ② create_worker ③ ssh 精确 kill -9 远端 worker
#       ④ Mac 侧会话回流断言（ToolResult + Assistant 回答） ⑤ get_session_messages 直读
#
# 三种端点（环境变量）:
#   WSL 直连 : RW_HOST=root@192.168.0.38 RW_PORT=2222 RW_USER=root RW_KEY=~/.ssh/id_ed25519
#   Win sshd : RW_HOST=win38 RW_PORT=22 RW_USER=sshuser RW_KEY=~/.ssh/id_ed25519 RW_BIN=/usr/local/bin/ion
#   wrapper  : 上述 + RW_WRAPPER='wsl -d ion -u root'（cmd.exe → wsl 穿透）
#
# 端点不可达 → 整组 SKIP（nc 探测）。
set -u
ION="${ION_BIN:-target/debug/ion}"
HOST="${RW_HOST:-root@192.168.0.38}"
PORT="${RW_PORT:-2222}"
USER_="${RW_USER:-root}"
KEY="${RW_KEY:-$HOME/.ssh/id_ed25519}"
RBIN="${RW_BIN:-/usr/local/bin/ion}"
NAME="${RW_NAME:-ci-sbx}"
WRAPPER="${RW_WRAPPER:-}"
SOCK="/tmp/ion-sbx-ci-$$.sock"
LOG="/tmp/ion-sbx-ci-$$.log"
PASS=0; FAIL=0; SKIP=0
HOST_PID=""

cleanup() {
  # 红线：只 kill 本脚本起的 host PID，严禁宽泛 pkill ion
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  rm -f "$SOCK" "$SOCK.pid" "$LOG"
}
trap cleanup EXIT

ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
skip() { SKIP=$((SKIP+1)); echo "  ⏭️ SKIP: $1"; }

# ── 可达性探测（学 remote_worker_ci.sh：不可达整组 SKIP）──
if ! nc -z -G 3 "${HOST#*@}" "$PORT" 2>/dev/null; then
  echo "远端 $HOST:$PORT 不可达，跳过全部用例"
  skip "endpoint unreachable (${HOST#*@}:$PORT)"
  echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
  exit 0
fi

# ── ① env 注入单端点 ION_REMOTE_WORKERS（cwd=/tmp，llm_bridge=true）──
RW_JSON=$(HOST="$HOST" PORT="$PORT" USER_="$USER_" KEY="$KEY" RBIN="$RBIN" NAME="$NAME" \
  WRAPPER="$WRAPPER" python3 -c "
import json, os
m = {os.environ['NAME']: {'user': os.environ['USER_'],
     'hostname': os.environ['HOST'].split('@')[-1],
     'port': int(os.environ['PORT']), 'key': os.environ['KEY'],
     'worker_bin': os.environ['RBIN'], 'cwd': '/tmp', 'llm_bridge': True}}
w = os.environ.get('WRAPPER')
if w: m[os.environ['NAME']]['wrapper'] = w
print(json.dumps(m))")
export ION_REMOTE_WORKERS="$RW_JSON"

# 起隔离 host（私有 socket 按 $$ 命名）
ION_HOST_SOCKET="$SOCK" "$ION" serve >"$LOG" 2>&1 &
HOST_PID=$!
for i in $(seq 1 20); do [ -S "$SOCK" ] && break; sleep 0.5; done
[ -S "$SOCK" ] || { echo "host 起不来"; cat "$LOG"; exit 1; }
ok "① 隔离 host 起动（socket=$SOCK, host=$NAME, llm_bridge=true, cwd=/tmp）"

rpc() { ION_HOST_SOCKET="$SOCK" "$ION" rpc "$@"; }

# ── ② create_worker：远程 bash 写时间戳文件并原样回报数字 ──
TS=$(date +%s%N)
PROMPT="运行 bash: echo $TS > /tmp/ion-sbx-ci-$TS.txt 并确认写入成功，然后把数字 $TS 原样回复给我"
W=$(rpc --method create_worker --params "$(python3 -c "
import json
print(json.dumps({'host': '$NAME', 'agent': 'build', 'initial_prompt': '''$PROMPT''', 'wait': False}))")")
WID=$(echo "$W" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['workerId'])" 2>/dev/null)
SID=$(echo "$W" | python3 -c "import json,sys; print(json.load(sys.stdin)['data'].get('sessionId',''))" 2>/dev/null)
if [ -n "$WID" ]; then
  ok "② create_worker(host=$NAME) → worker=$WID session=$SID"
else
  bad "② create_worker 失败: $(echo "$W" | head -3)"
  echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
  exit 1
fi

# 等 worker 在远端真正跑起来（Bash 工具执行完毕��后皆可，关键是进程存在）
sleep 5

# ── ③ ssh 远端精确 kill -9 该 worker（pattern 含 sessionId，防 pkill 自匹配）──
SSH_CMD="ssh -i $KEY -p $PORT -o StrictHostKeyChecking=no -o ConnectTimeout=5 $USER@${HOST#*@}"
# 若指定 wrapper，先穿 wrapper 再执行远端命令
if [ -n "$WRAPPER" ]; then
  KILL_OUT=$($SSH_CMD $WRAPPER "pkill -9 -f 'ion.*$SID'" 2>&1)
else
  KILL_OUT=$($SSH_CMD "pkill -9 -f 'ion.*$SID'" 2>&1)
fi
if [ $? -eq 0 ]; then
  ok "③ 远端精确 kill -9 worker（pattern 含 sessionId=$SID，无 pkill 自匹配）"
else
  bad "③ 远端 kill 失败: $(echo "$KILL_OUT" | head -2)"
fi

# ── ④ Mac 侧会话回流断言：<sid>.jsonl 存在且含 ToolResult 与 Assistant 回答 ──
F=""
for i in $(seq 1 15); do
  F=$(find "$HOME/.ion/agent/sessions" -name "$SID.jsonl" 2>/dev/null | head -1)
  [ -n "$F" ] && break
  sleep 1
done
if [ -n "$F" ]; then
  ok "④a 会话回流 Mac 落盘 $F（$(wc -l < "$F" | tr -d ' ') 行）"
  # 轮等回流内容补齐（kill -9 后流式回流可能滞后）
  HAS_TR=1; HAS_AS=1
  for i in $(seq 1 15); do
    HAS_TR=$(grep -c '"type":"tool_result"\|"type":"ToolResult"\|toolResult' "$F" 2>/dev/null || true)
    HAS_AS=$(python3 -c "
import json,sys
n=0
for line in open('$F'):
    try: e=json.loads(line)
    except: continue
    if e.get('type')=='assistant' or e.get('role')=='assistant': n+=1
print(n)" 2>/dev/null || echo 0)
    { [ "${HAS_TR:-0}" -ge 1 ] && [ "${HAS_AS:-0}" -ge 1 ]; } && break
    sleep 1
  done
  [ "${HAS_TR:-0}" -ge 1 ] && ok "④b JSONL 含 ToolResult 条目" || bad "④b 未见 ToolResult"
  [ "${HAS_AS:-0}" -ge 1 ] && ok "④c JSONL 含 Assistant 回答" || bad "④c 未见 Assistant 回答"
else
  bad "④ 会话未回流 Mac（sid=$SID）"
fi

# ── ⑤ get_session_messages 直读 ──
if [ -n "$F" ]; then
  MSGS=$(ION_HOST_SOCKET="$SOCK" "$ION" rpc --session "$F" --method get_session_messages --params '{"limit":5}' 2>&1)
  N=$(echo "$MSGS" | python3 -c "import json,sys; print(len(json.load(sys.stdin)['data'].get('messages',[])))" 2>/dev/null)
  if [ -n "$N" ] && [ "$N" -ge 1 ] 2>/dev/null; then
    ok "⑤ get_session_messages 直读成功（limit=5, 取回 $N 条）"
  else
    bad "⑤ 直读失败: $(echo "$MSGS" | head -3)"
  fi
else
  skip "⑤ 依赖 ④ 的落盘文件，跳过直读"
fi

# 收尾：清理残留 worker（host 退出时由 cleanup 兜底）
[ -n "$WID" ] && rpc --method kill --params "{\"workerId\":\"$WID\"}" >/dev/null 2>&1

echo ""
echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
[ "$FAIL" -eq 0 ]
