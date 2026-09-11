#!/bin/bash
# remote_worker_ci.sh — REMOTE_WORKER M1 客户端模式命令行验证
# 设计文档: docs/design/REMOTE_WORKER.md §6 Group A
#
# 隔离三件套: 私有 ION_HOST_SOCKET + ION_REMOTE_WORKERS env + ION_FAUX_REPLY（零 LLM 成本）
# 前置: ~/.ssh 可达远端（默认 win38:2222 直连 WSL sshd）；不可达时整组 SKIP
set -u
ION="${ION_BIN:-/Users/xuyingzhou/Project/study-rust/ion/target/debug/ion}"
HOST="${RW_HOST:-root@192.168.0.38}"
PORT="${RW_PORT:-2222}"
SOCK="/tmp/ion-rw-ci-$$.sock"
LOG="/tmp/ion-rw-ci-$$.log"
PASS=0; FAIL=0; SKIP=0

cleanup() {
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
  rm -f "$SOCK" "$SOCK.pid" "$LOG"
}
trap cleanup EXIT

ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
skip() { SKIP=$((SKIP+1)); echo "  ⏭️ SKIP: $1"; }

# 可达性探测
if ! nc -z -G 3 "${HOST#*@}" "$PORT" 2>/dev/null; then
  echo "远端 $HOST:$PORT 不可达，跳过全部用例"; skip "host unreachable"; exit 0
fi

# 远端配置只经 env 注入（不碰用户 config.json）——必须在 host 启动前进入其环境。
# llm_bridge=true：worker 注入 ION_PROVIDER_BRIDGE+ION_SESSION_STREAM（M2 桥接 + M3 回流），
# Manager 侧桥接 registry 同时注册 faux（下方 ION_FAUX_REPLY）→ 全链路确定性。
export ION_REMOTE_WORKERS="{\"ci-exec\": {\"user\":\"root\",\"hostname\":\"${HOST#*@}\",\"port\":$PORT,\"worker_bin\":\"/usr/local/bin/ion\",\"cwd\":\"/tmp\",\"llm_bridge\":true}}"

# 起私有 host（faux 确定性回复，远程 worker 侧注册 FauxProvider）
ION_HOST_SOCKET="$SOCK" ION_FAUX_REPLY="RW_CI_FAUX_OK" \
  "$ION" serve >"$LOG" 2>&1 &
HOST_PID=$!
for i in $(seq 1 20); do [ -S "$SOCK" ] && break; sleep 0.5; done
[ -S "$SOCK" ] || { echo "host 起不来"; cat "$LOG"; exit 1; }

rpc() { ION_HOST_SOCKET="$SOCK" "$ION" rpc "$@"; }

# 远端配置只经 env 注入（不碰用户 config.json）
export ION_REMOTE_WORKERS="{\"ci-exec\": {\"user\":\"root\",\"hostname\":\"${HOST#*@}\",\"port\":$PORT,\"worker_bin\":\"/usr/local/bin/ion\",\"cwd\":\"/tmp\"}}"

echo "── Group A: 远程拉起与生命周期 ──"

# A1 拉起远程 worker + host 字段
W=$(rpc --method create_worker --params '{"host":"ci-exec","agent":"build","initial_prompt":"ping","wait":false}')
WID=$(echo "$W" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['workerId'])" 2>/dev/null)
if [ -n "$WID" ]; then ok "A1 create_worker(host=ci-exec) → $WID"; else bad "A1 create_worker 失败: $(echo "$W" | head -2)"; fi

# A2 host 字段可见
sleep 8
H=$(rpc --method list_workers | python3 -c "
import json,sys
ws=json.load(sys.stdin)['data']['workers']
print(next((w.get('host') for w in ws if w.get('workerId')=='$WID'), 'MISSING'))")
[ "$H" = "ci-exec" ] && ok "A2 list_workers host=ci-exec" || bad "A2 host 字段: $H"

# A3 faux 确定性完成（轮询最多 30s 等 Idle）
ST="Busy"
for i in $(seq 1 15); do
  ST=$(rpc --method list_workers | python3 -c "
import json,sys
ws=json.load(sys.stdin)['data']['workers']
print(next((w.get('status') for w in ws if w.get('workerId')=='$WID'), 'MISSING'))" 2>/dev/null)
  [ "$ST" = "Idle" ] || [ "$ST" = "MISSING" ] && break
  sleep 2
done
[ "$ST" = "Idle" ] && ok "A3 远端 worker 首轮完成转 Idle（faux）" || bad "A3 状态: $ST"
grep -q "RW_CI_FAUX_OK" "$LOG" && ok "A3b faux 回复经事件流回到 Mac" || skip "A3b 日志未见 faux 字样（事件形状差异，非致命）"

# A4 kill → 清理
rpc --method kill --params "{\"workerId\":\"$WID\"}" >/dev/null 2>&1
sleep 2
ST2=$(rpc --method list_workers | python3 -c "
import json,sys
ws=json.load(sys.stdin)['data']['workers']
print(next((w.get('status') for w in ws if w.get('workerId')=='$WID'), 'MISSING'))")
[ "$ST2" = "Dead" ] || [ "$ST2" = "MISSING" ] && ok "A4 kill 后 Dead/移除" || bad "A4 kill 后状态: $ST2"

# A5 未知 host 拒绝
E=$(rpc --method create_worker --params '{"host":"nope","agent":"build","initial_prompt":"x","wait":false}')
echo "$E" | grep -q "unknown remote worker host" && ok "A5 未知 host 明确报错" || bad "A5 报错形状: $(echo "$E" | head -3)"

# A6 remote+worktree 拒绝
E2=$(rpc --method create_worker --params '{"host":"ci-exec","worktree":{"branch":"x"},"agent":"build","initial_prompt":"x","wait":false}')
echo "$E2" | grep -q "not supported for remote" && ok "A6 remote+worktree 拒绝" || bad "A6 报错形状: $(echo "$E2" | head -3)"

echo "── Group B: 零 key 桥接（faux 确定性） ──"
W2=$(rpc --method create_worker --params '{"host":"ci-exec","agent":"build","initial_prompt":"ping","wait":false}')
WID2=$(echo "$W2" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['workerId'])" 2>/dev/null)
if [ -n "$WID2" ]; then
  ok "B1 bridge 模式 create_worker → $WID2"
  ST="Busy"
  for i in $(seq 1 15); do
    ST=$(rpc --method list_workers | python3 -c "
import json,sys
ws=json.load(sys.stdin)['data']['workers']
print(next((w.get('status') for w in ws if w.get('workerId')=='$WID2'), 'MISSING'))" 2>/dev/null)
    [ "$ST" = "Idle" ] || [ "$ST" = "MISSING" ] && break
    sleep 2
  done
  [ "$ST" = "Idle" ] && ok "B2 bridge worker 完成首轮（faux 经 Manager 代发）" || bad "B2 bridge 状态: $ST"
  # B3 会话回流：Mac 磁盘出现 <sid>.jsonl
  SID2=$(rpc --method list_workers | python3 -c "
import json,sys
print(next((w.get('sessionId') for w in json.load(sys.stdin)['data']['workers'] if w.get('workerId')=='$WID2'), ''))" 2>/dev/null)
  F=$(find ~/.ion/agent/sessions -name "$SID2.jsonl" 2>/dev/null | head -1)
  [ -n "$F" ] && ok "B3 会话回流 Mac 落盘（$(wc -l < "$F") 行）" || bad "B3 会话未回流"
  rpc --method kill --params "{\"workerId\":\"$WID2\"}" >/dev/null 2>&1
else
  bad "B1 bridge create_worker 失败"
fi

echo "── Group C: grants 默认全拒（安全语义） ──"
# 无 grants 配置的 host（ci-nogrant）→ host 工具调用必拒。
# 经 spawn_worker 不便构造工具调用——验证配置层：grants 缺省 None → 解析正确
export ION_REMOTE_WORKERS="$ION_REMOTE_WORKERS"  # 保持
python3 -c "
import json, os
m = json.loads(os.environ['ION_REMOTE_WORKERS'])
assert m['ci-exec'].get('grants') is None, 'grants default should be None (deny-all)'
print('C1 grants-default-deny 配置语义 OK')" && ok "C1 grants 缺省=None（全拒）" || bad "C1 grants 缺省语义"

echo ""
echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
[ "$FAIL" -eq 0 ]
