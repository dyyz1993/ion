#!/usr/bin/env bash
# sandbox_live_drill.sh — 沙盒真机演练执行体（K5 真机验证待办 ②③；①走 sandbox_stateless_ci.sh）
#
# 套件:
#   policy-rpc  — sandbox_policy RPC 真机设置（SANDBOX_POOL.md §3.4 真机面）
#                 list_sandboxes 视图 / sandbox_probe 真 ssh 体检（可达+版本）/
#                 sandbox_policy GET→SET auto_approve→回读 override→非法值拒绝→list 同步
#                 隔离 HOME（临时目录，不碰真实 ~/.ion）、无需 LLM。预计 ~30s。
#   pump-e2e    — 真沙盒审批泵 e2e（SANDBOX_POOL.md §3.3 审批停摆机制的真机闭环）
#                 auto_approve 沙盒 worker 真实写文件 → ApprovalRequest → host 审批泵
#                 自动 review_approve_all → SandboxAutoApproved 事件 + review_pending 归零
#                 用真实 HOME 的 LLM 配置（llm_bridge=true 零 key 桥接，真 LLM 往返）。预计 2-4 分钟。
#
# 端点参数（与 sandbox_stateless_ci.sh 同一套环境变量，缺省=生产 win38）:
#   RW_HOST=192.168.0.38 RW_PORT=2222 RW_USER=root RW_KEY= RW_BIN=/usr/local/bin/ion
#   RW_NAME=ci-sbx RW_WRAPPER= ION_BIN=target/debug/ion
#
# ⚠️ 本脚本不含确认门。推荐经 tests/sandbox_live_readiness.sh --run <suite> 进入
#    （那里有 SANDBOX_LIVE_CONFIRM=YES + 终端交互输入 yes 双重确认）。直接运行本脚本 = 自担确认。
#
# 端点 TCP 不可达 → 整组 SKIP（nc 探测，绝不盲试 ssh）。
# 红线：cleanup 只 kill 本脚本记录的 HOST_PID / SUB_PID；严禁宽泛 pkill；不直接 ssh（ssh 只发生在
#       ion 自身的 sandbox_probe / 远程 spawn 内部）。
set -u
SUITE="${1:-}"
DIR_SELF="$(cd "$(dirname "$0")" && pwd)"
ION="${ION_BIN:-$DIR_SELF/../target/debug/ion}"
HOST="${RW_HOST:-192.168.0.38}"
PORT="${RW_PORT:-2222}"
USER_="${RW_USER:-root}"
KEY="${RW_KEY:-}"
RBIN="${RW_BIN:-/usr/local/bin/ion}"
NAME="${RW_NAME:-ci-sbx}"
WRAPPER="${RW_WRAPPER:-}"
PASS=0; FAIL=0; SKIP=0
HOST_PID=""
SUB_PID=""

ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
skip() { SKIP=$((SKIP+1)); echo "  ⏭️ SKIP: $1"; }
jq_ok() { # jq_ok <desc> <json> [jq 任意参数...] — 可透传 --arg；满足记 PASS，否则 FAIL
  local desc="$1" json="$2"; shift 2
  if echo "$json" | jq -e "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}

cleanup() {
  [ -n "${SUB_PID:-}" ] && kill "$SUB_PID" 2>/dev/null
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "${ROOT:-}" ] && rm -rf "$ROOT"
}
trap cleanup EXIT

case "$SUITE" in
  policy-rpc|pump-e2e) ;;
  *)
    echo "用法: $0 <policy-rpc|pump-e2e>"
    echo "  （① 五步试炼请直接跑 tests/sandbox_stateless_ci.sh）"
    exit 2
    ;;
esac

# ── 前置：TCP 探测（不可达整组 SKIP；不做任何 ssh）──
if ! nc -z -G 3 "$HOST" "$PORT" 2>/dev/null; then
  echo "端点 $USER_@$HOST:$PORT 不可达，整组 SKIP（授权窗口未开或端点未上电）"
  skip "endpoint unreachable"
  echo ""
  echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
  exit 0
fi
if [ ! -x "$ION" ]; then
  echo "❌ ion 二进制不存在/不可执行: ${ION}（先 cargo build --bin ion，或 ION_BIN=... 指向已构建产物）"
  exit 1
fi

# ── 组装沙盒注入 JSON（与 sandbox_stateless_ci.sh 同构）──
# policy-rpc: 带 notes（验 list_sandboxes 透传），不设出生策略（由 RPC SET 驱动）
# pump-e2e:   出生档案 auto_approve + llm_bridge=true（零 key 桥接 + 会话回流）
if [ "$SUITE" = "policy-rpc" ]; then
  SANDBOX_JSON=$(NAME_="$NAME" HOST_="$HOST" PORT_="$PORT" USER_="$USER_" KEY_="$KEY" \
    RBIN_="$RBIN" WRAPPER_="$WRAPPER" python3 -c "import json,os
sb = {'user': os.environ['USER_'], 'hostname': os.environ['HOST_'], 'worker_bin': os.environ['RBIN_'], 'cwd': '/tmp', 'notes': ['live drill: policy-rpc 演练注入']}
if os.environ['PORT_']: sb['port'] = int(os.environ['PORT_'])
if os.environ['KEY_']: sb['key'] = os.path.expanduser(os.environ['KEY_'])
if os.environ['WRAPPER_']: sb['wrapper'] = os.environ['WRAPPER_']
print(json.dumps({os.environ['NAME_']: sb}))")
else
  SANDBOX_JSON=$(NAME_="$NAME" HOST_="$HOST" PORT_="$PORT" USER_="$USER_" KEY_="$KEY" \
    RBIN_="$RBIN" WRAPPER_="$WRAPPER" python3 -c "import json,os
sb = {'user': os.environ['USER_'], 'hostname': os.environ['HOST_'], 'worker_bin': os.environ['RBIN_'], 'cwd': '/tmp', 'llm_bridge': True, 'approval_policy': 'auto_approve'}
if os.environ['PORT_']: sb['port'] = int(os.environ['PORT_'])
if os.environ['KEY_']: sb['key'] = os.path.expanduser(os.environ['KEY_'])
if os.environ['WRAPPER_']: sb['wrapper'] = os.environ['WRAPPER_']
print(json.dumps({os.environ['NAME_']: sb}))")
fi

start_host() { # start_host <home-dir|REAL> <sock> <log>
  local home="$1" sock="$2" log="$3"
  if [ "$home" = "REAL" ]; then
    ION_HOST_SOCKET="$sock" ION_REMOTE_WORKERS="$SANDBOX_JSON" \
      "$ION" serve >"$log" 2>&1 &
  else
    HOME="$home" ION_SESSION_DIR="$home/.ion/agent/sessions" \
    ION_HOST_SOCKET="$sock" ION_REMOTE_WORKERS="$SANDBOX_JSON" \
      "$ION" serve >"$log" 2>&1 &
  fi
  HOST_PID=$!
  local i
  for i in $(seq 1 20); do [ -S "$sock" ] && break; sleep 0.5; done
  [ -S "$sock" ]
}

rpc() { # rpc <sock> <home> <method> [params]
  local sock="$1" home="$2" method="$3" params="${4:-}"
  if [ -n "$params" ]; then
    HOME="$home" ION_HOST_SOCKET="$sock" "$ION" rpc --method "$method" --params "$params" 2>/dev/null
  else
    HOME="$home" ION_HOST_SOCKET="$sock" "$ION" rpc --method "$method" 2>/dev/null
  fi
}

# ════════════════════════════════════════════════════════
if [ "$SUITE" = "policy-rpc" ]; then
echo ""
echo "═ 套件 policy-rpc: sandbox_policy RPC 真机设置（沙盒=$NAME → $USER_@$HOST:${PORT}）"
# ════════════════════════════════════════════════════════

ROOT="$(mktemp -d /tmp/ion-live-policy-XXXXXX)"
mkdir -p "$ROOT/home/.ion/agent/sessions"
# 隔离 HOME：关 memory 系防单例 worker 噪声；本套件不需要 LLM、不需要 file-snapshot
printf '%s\n' '{"extensions":{"memory":{"enabled":false},"global-memory":{"enabled":false},"learning":{"enabled":false}}}' \
  > "$ROOT/home/.ion/config.json"
SOCK="$ROOT/h.sock"
if start_host "$ROOT/home" "$SOCK" "$ROOT/host.log"; then
  ok "① 隔离 host 起动（HOME=$ROOT/home, socket=$SOCK, 沙盒经 env 注入）"
else
  bad "① host 起不来"; tail -5 "$ROOT/host.log"; exit 1
fi

LS=$(rpc "$SOCK" "$ROOT/home" list_sandboxes)
jq_ok "A1 list_sandboxes 含 ${NAME}（env 注入入池）" "$LS" --arg n "$NAME" '.data.sandboxes[] | select(.name==$n)'
jq_ok "A2 dest 展示含 hostname（${HOST}）" "$LS" --arg n "$NAME" --arg h "$HOST" '.data.sandboxes[] | select(.name==$n) | (.dest | contains($h))'
jq_ok "A3 notes 注入可见（环境层档案）" "$LS" --arg n "$NAME" '.data.sandboxes[] | select(.name==$n) | (.notes|length) == 1'

PB=$(rpc "$SOCK" "$ROOT/home" sandbox_probe "{\"name\":\"$NAME\"}")
jq_ok "B1 sandbox_probe 成功（真 ssh 体检 $USER_@$HOST:${PORT}）" "$PB" '.success == true'
jq_ok "B2 health=Reachable" "$PB" '.data.status.health == "Reachable"'
V=$(echo "$PB" | jq -r '.data.status.version // empty' 2>/dev/null)
if [ -n "$V" ]; then
  ok "B3 远端 worker_bin 版本回读: ${V}（${RBIN}）"
else
  bad "B3 远端 --version 无输出（$RBIN 未安装或 wrapper 链路问题）"
fi
rpc "$SOCK" "$ROOT/home" sandbox_probe '{"name":"__nope__"}' | jq -e '.success == false' >/dev/null 2>&1
[ $? -eq 0 ] && ok "B4 未知沙盒 probe → 明确报错" || bad "B4 未知沙盒 probe → 明确报错"

GP=$(rpc "$SOCK" "$ROOT/home" sandbox_policy "{\"host\":\"$NAME\"}")
jq_ok "C1 GET 出生档案 default（无覆盖）" "$GP" '.success == true and .data.effective == "default" and .data.override == null'

SP=$(rpc "$SOCK" "$ROOT/home" sandbox_policy "{\"host\":\"$NAME\",\"policy\":\"auto_approve\"}")
jq_ok "D1 SET host → auto_approve（写后回读生效）" "$SP" '.success == true and .data.effective == "auto_approve"'
rpc "$SOCK" "$ROOT/home" sandbox_policy "{\"host\":\"$NAME\"}" | jq -e '.data.override == "auto_approve" and .data.effective == "auto_approve"' >/dev/null 2>&1
[ $? -eq 0 ] && ok "D2 GET 回读 override=auto_approve" || bad "D2 GET 回读 override=auto_approve"
LS2=$(rpc "$SOCK" "$ROOT/home" list_sandboxes)
jq_ok "D3 list_sandboxes 视图同步（覆盖 > 档案）" "$LS2" --arg n "$NAME" '.data.sandboxes[] | select(.name==$n) | .approvalPolicy == "auto_approve"'

rpc "$SOCK" "$ROOT/home" sandbox_policy "{\"host\":\"$NAME\",\"policy\":\"yolo\"}" | jq -e '.success == false' >/dev/null 2>&1
[ $? -eq 0 ] && ok "E1 非法 policy 严格拒绝（不静默回落）" || bad "E1 非法 policy 严格拒绝（不静默回落）"
rpc "$SOCK" "$ROOT/home" sandbox_policy '{"host":"__nope__","policy":"auto_approve"}' | jq -e '.success == false' >/dev/null 2>&1
[ $? -eq 0 ] && ok "E2 未知沙盒 SET 拒绝" || bad "E2 未知沙盒 SET 拒绝"

rpc "$SOCK" "$ROOT/home" sandbox_policy "{\"host\":\"$NAME\",\"policy\":\"default\"}" | jq -e '.data.effective == "default"' >/dev/null 2>&1
[ $? -eq 0 ] && ok "F1 SET 回 default（内存态覆盖可逆；host 重启即失）" || bad "F1 SET 回 default（内存态覆盖可逆；host 重启即失）"
fi

# ════════════════════════════════════════════════════════
if [ "$SUITE" = "pump-e2e" ]; then
echo ""
echo "═ 套件 pump-e2e: 真沙盒审批泵 e2e（沙盒=$NAME → $USER_@$HOST:${PORT}，真 LLM）"
# ════════════════════════════════════════════════════════

# 前置：写审批由 file-snapshot 扩展触发（默认 disabled）——真实 HOME 配置必须已开启
FS_ON=$(python3 -c "import json,os
try:
    cfg = json.load(open(os.path.expanduser('~/.ion/config.json')))
    ext = (cfg.get('extensions') or {}).get('file-snapshot') or {}
    print('yes' if ext.get('enabled') else 'no')
except Exception:
    print('no')" 2>/dev/null)
if [ "$FS_ON" != "yes" ]; then
  bad "前置失败: ~/.ion/config.json 未开 file-snapshot（写文件不会触发 ApprovalRequest，泵无从谈起）"
  echo "   开启方法: 在 ~/.ion/config.json 加 \"extensions\":{\"file-snapshot\":{\"enabled\":true}} 后重启 host 再演练"
  exit 1
fi
ok "前置: 真实 ~/.ion/config.json 已开 file-snapshot（写审批可触发）"

ROOT="$(mktemp -d /tmp/ion-live-pump-XXXXXX)"
SOCK="$ROOT/h.sock"
# 真实 HOME（LLM 配置/auth）+ 私有 socket + env 注入沙盒（出生档案 auto_approve）
if start_host REAL "$SOCK" "$ROOT/host.log"; then
  ok "① host 起动（真实 HOME=LLM 配置，socket=${SOCK}，沙盒 $NAME 出生档案 auto_approve + llm_bridge）"
else
  bad "① host 起不来"; tail -5 "$ROOT/host.log"; exit 1
fi

# 订阅 UI 事件流抓 SandboxAutoApproved
UI_LOG="$ROOT/ui.log"
HOME="$HOME" ION_HOST_SOCKET="$SOCK" "$ION" subscribe --ui >"$UI_LOG" 2>&1 &
SUB_PID=$!
sleep 1

TS=$(date +%s)
PROMPT="运行 bash: echo $TS > /tmp/ion-live-pump-$TS.txt，然后把数字 $TS 原样回复给我"
PARAMS=$(PNAME_="$NAME" PROMPT_="$PROMPT" python3 -c "import json,os; print(json.dumps({'host': os.environ['PNAME_'], 'agent': 'build', 'initial_prompt': os.environ['PROMPT_'], 'wait': False}))")
CW=$(rpc "$SOCK" "$HOME" create_worker "$PARAMS")
WID=$(echo "$CW" | jq -r '.data.workerId // empty' 2>/dev/null)
SID=$(echo "$CW" | jq -r '.data.sessionId // empty' 2>/dev/null)
if [ -n "$WID" ]; then
  ok "② create_worker(host=$NAME) → worker=$WID session=$SID"
else
  bad "② create_worker 失败: $(echo "$CW" | head -3)"
  exit 1
fi

# 等远端真实轮完成（真 LLM 往返，最长 240s）
ST="Busy"
for _ in $(seq 1 120); do
  ST=$(rpc "$SOCK" "$HOME" list_workers 2>/dev/null | jq -r --arg w "$WID" \
    '.data.workers[]? | select(.workerId==$w) | .status' 2>/dev/null)
  [ "$ST" = "Idle" ] || [ "$ST" = "GONE" ] && break
  sleep 2
done
[ "$ST" = "Idle" ] && ok "③ 远端真实轮完成（Idle；写文件已触发 ApprovalRequest）" \
                 || bad "③ 状态: ${ST}（240s 未完成，查 $ROOT/host.log 与审批队列）"

# 泵事件（最多 10s 异步到达；ui 流是 pretty-JSON 对象序列 → jq -s 聚合）
PUMP_HIT=1
for _ in $(seq 1 50); do
  if jq -s -e '[.[] | select(.ui_type=="SandboxAutoApproved")] | length > 0' "$UI_LOG" >/dev/null 2>&1; then
    PUMP_HIT=0; break
  fi
  sleep 0.2
done
[ $PUMP_HIT -eq 0 ] && ok "④ SandboxAutoApproved 事件可见（subscribe --ui 抓到泵动作）" \
                   || bad "④ 未见 SandboxAutoApproved（泵未 fire：查出生档案/覆盖链与 reader-loop）"
jq -s -e --arg w "$WID" '[.[] | select(.ui_type=="SandboxAutoApproved")][0].data.workerId' "$UI_LOG" 2>/dev/null | grep -q "$WID"
[ $? -eq 0 ] && ok "④b 事件带正确 workerId" || bad "④b 事件 workerId 不符"
jq -s -e '[.[] | select(.ui_type=="SandboxAutoApproved")][0].data.resolved.approved >= 1' "$UI_LOG" >/dev/null 2>&1
[ $? -eq 0 ] && ok "④c 事件带放行明细（resolved.approved>=1）" || bad "④c 事件缺放行明细"

RP=$(HOME="$HOME" ION_HOST_SOCKET="$SOCK" "$ION" rpc --session "$SID" --method review_pending 2>/dev/null)
echo "$RP" | jq -e '.data.summary.total == 0' >/dev/null 2>&1
[ $? -eq 0 ] && ok "⑤ review_pending 归零（无人值守闭环：审批不再停摆）" \
               || bad "⑤ review_pending 未归零: $(echo "$RP" | head -3)"
kill "$SUB_PID" 2>/dev/null; SUB_PID=""
fi

echo ""
echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
[ "$FAIL" -eq 0 ]
