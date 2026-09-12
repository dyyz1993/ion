#!/bin/bash
# sandbox_stateless_ci.sh — 沙盒池「无状态五步试炼」命令行验证
# 设计文档: docs/design/SANDBOX_POOL.md §4.1
#
# 五步: ① env 注入单端点 + 起隔离 host ② create_worker 真任务 ③ ssh 精确 kill -9 远端 worker
#       ④ Mac 侧会话回流断言（ToolResult + Assistant 回答） ⑤ get_session_messages 直读
#
# 端点参数（环境变量）:
#   WSL 直连 : RW_HOST=192.168.0.38 RW_PORT=2222 RW_USER=root（KEY 留空用默认密钥）
#   Win 穿透 : RW_HOST=192.168.0.38 RW_PORT=22 RW_USER=sshuser RW_KEY=~/.ssh/id_ed25519 \
#              RW_WRAPPER='wsl -d ion -u root'
#   可选     : RW_BIN=/usr/local/bin/ion RW_NAME=ci-sbx ION_BIN=target/debug/ion
#
# 端点不可达 → 整组 SKIP（nc 探测）。红线：cleanup 只 kill 本脚本 host PID，严禁宽泛 pkill ion。
set -u
ION="${ION_BIN:-target/debug/ion}"
HOST="${RW_HOST:-192.168.0.38}"
PORT="${RW_PORT:-2222}"
USER_="${RW_USER:-root}"
KEY="${RW_KEY:-}"
RBIN="${RW_BIN:-/usr/local/bin/ion}"
NAME="${RW_NAME:-ci-sbx}"
WRAPPER="${RW_WRAPPER:-}"
SOCK="/tmp/ion-sbx-ci-$$.sock"
LOG="/tmp/ion-sbx-ci-$$.log"
PASS=0; FAIL=0; SKIP=0
HOST_PID=""

cleanup() {
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
  rm -f "$SOCK" "$SOCK.pid" "$LOG"
}
trap cleanup EXIT

ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
skip() { SKIP=$((SKIP+1)); echo "  ⏭️ SKIP: $1"; }

if ! nc -z -G 3 "$HOST" "$PORT" 2>/dev/null; then
  echo "端点 $HOST:$PORT 不可达，整组 SKIP"; skip "endpoint unreachable"; exit 0
fi

# ── ① env 注入单端点，起隔离 host（私有 socket；llm_bridge=true → 零 key 桥接 + 会话回流）──
SANDBOX_JSON=$(NAME_="$NAME" HOST_="$HOST" PORT_="$PORT" USER_="$USER_" KEY_="$KEY" \
  RBIN_="$RBIN" WRAPPER_="$WRAPPER" python3 -c "import json,os
sb = {'user': os.environ['USER_'], 'hostname': os.environ['HOST_'], 'worker_bin': os.environ['RBIN_'], 'cwd': '/tmp', 'llm_bridge': True}
if os.environ['PORT_']: sb['port'] = int(os.environ['PORT_'])
if os.environ['KEY_']: sb['key'] = os.path.expanduser(os.environ['KEY_'])
if os.environ['WRAPPER_']: sb['wrapper'] = os.environ['WRAPPER_']
print(json.dumps({os.environ['NAME_']: sb}))")
export ION_REMOTE_WORKERS="$SANDBOX_JSON"
ION_HOST_SOCKET="$SOCK" "$ION" serve >"$LOG" 2>&1 &
HOST_PID=$!
for i in $(seq 1 20); do [ -S "$SOCK" ] && break; sleep 0.5; done
[ -S "$SOCK" ] || { echo "host 起不来"; cat "$LOG"; exit 1; }
rpc() { ION_HOST_SOCKET="$SOCK" "$ION" rpc "$@"; }
ok "① 隔离 host 起动（socket=$SOCK, sandbox=$NAME, llm_bridge=true, cwd=/tmp）"

# ── ② create_worker：远程 bash 写时间戳文件并原样回报数字 ──
TS=$(date +%s)
PROMPT="运行 bash: echo $TS > /tmp/ion-sbx-ci-$TS.txt，然后把数字 $TS 原样回复给我"
PARAMS=$(NAME_="$NAME" PROMPT_="$PROMPT" python3 -c "import json,os; print(json.dumps({'host': os.environ['NAME_'], 'agent': 'build', 'initial_prompt': os.environ['PROMPT_'], 'wait': False}))")
W=$(rpc --method create_worker --params "$PARAMS")
WID=$(echo "$W" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['workerId'])" 2>/dev/null)
SID=$(echo "$W" | python3 -c "import json,sys; print(json.load(sys.stdin)['data'].get('sessionId',''))" 2>/dev/null)
if [ -n "$WID" ]; then
  ok "② create_worker(host=$NAME) → worker=$WID session=$SID"
else
  bad "② create_worker 失败: $(echo "$W" | head -3)"
  echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"; exit 1
fi

# 等远端首轮真正完成（Idle；真 LLM 往返最长 120s）——先跑完再杀，验证"完成后死亡数据仍全"
ST="Busy"
for i in $(seq 1 60); do
  ST=$(rpc --method list_workers 2>/dev/null | python3 -c "
import json,sys
try:
    ws=json.load(sys.stdin)['data']['workers']
    print(next((w.get('status') for w in ws if w.get('workerId')=='$WID'),'GONE'))
except Exception: print('ERR')" 2>/dev/null)
  [ "$ST" = "Idle" ] || [ "$ST" = "GONE" ] && break
  sleep 2
done
[ "$ST" = "Idle" ] && ok "②b 远端首轮完成（Idle，真 LLM 往返）" || bad "②b 状态: $ST（120s 未完成）"

# ── ③ ssh 远端精确 kill -9（io[n] 括号技巧防 pkill 自匹配）──
KEYOPT=""; [ -n "$KEY" ] && KEYOPT="-i $KEY"
if [ -n "$WRAPPER" ]; then
  KILL_OUT=$(ssh $KEYOPT -p "$PORT" -o StrictHostKeyChecking=no -o ConnectTimeout=5 "$USER_@$HOST" $WRAPPER "pkill -9 -f 'io[n].*$SID'" 2>&1)
else
  KILL_OUT=$(ssh $KEYOPT -p "$PORT" -o StrictHostKeyChecking=no -o ConnectTimeout=5 "$USER_@$HOST" "pkill -9 -f 'io[n].*$SID'" 2>&1)
fi
if [ $? -eq 0 ]; then
  ok "③ 远端精确 kill -9 worker（pattern 含 sessionId=${SID}）"
else
  bad "③ 远端 kill 失败: $(echo "$KILL_OUT" | head -2)"
fi

# ── ④ Mac 侧会话回流断言：JSONL 存在且含 ToolResult 与 Assistant 回答 ──
F=""
for i in $(seq 1 15); do
  F=$(find "$HOME/.ion/agent/sessions" -name "$SID.jsonl" 2>/dev/null | head -1)
  [ -n "$F" ] && break
  sleep 1
done
if [ -n "$F" ]; then
  ok "④a 会话回流 Mac 落盘（$(wc -l < "$F" | tr -d ' ') 行）"
  HAS_TR=0; HAS_AS=0
  for i in $(seq 1 15); do
    HAS_TR=$(grep -c 'ToolResult' "$F" 2>/dev/null)
    HAS_AS=$(grep -c '"Assistant"' "$F" 2>/dev/null)
    [ "${HAS_TR:-0}" -ge 1 ] && [ "${HAS_AS:-0}" -ge 1 ] && break
    sleep 1
  done
  [ "${HAS_TR:-0}" -ge 1 ] && ok "④b JSONL 含 ToolResult 条目" || bad "④b 未见 ToolResult"
  [ "${HAS_AS:-0}" -ge 1 ] && ok "④c JSONL 含 Assistant 回答" || bad "④c 未见 Assistant 回答"
else
  bad "④ 会话未回流 Mac（sid=${SID}）"
fi

# ── ⑤ get_session_messages 直读（worker 已死仍可读）──
if [ -n "$F" ]; then
  MSGS=$(rpc --session "$F" --method get_session_messages --params '{"limit":5}' 2>/dev/null)
  N=$(echo "$MSGS" | python3 -c "import json,sys; print(len(json.load(sys.stdin)['data'].get('messages',[])))" 2>/dev/null)
  if [ -n "$N" ] && [ "$N" -ge 1 ] 2>/dev/null; then
    ok "⑤ get_session_messages 直读成功（取回 $N 条）"
  else
    bad "⑤ 直读失败: $(echo "$MSGS" | head -3)"
  fi
fi

echo ""
echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
[ "$FAIL" -eq 0 ]
