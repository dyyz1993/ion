#!/bin/bash
# sandbox_monitor.sh — 沙盒 worker 实时活性监视器（30s 粒度，raw socket 版）
# 不依赖 ion 二进制（免疫 target 被并行会话清理——2026-09-13 第 4 次实录）
# 指标: worker 状态 / event_age_s（最后流式事件距今秒）/ commit / 页面 / zai → /tmp/sandbox-metrics.log
# 判定: Busy 且事件年龄>120s → STALL_SUSPECTED；>240s → STALL_CONFIRMED；Stale/Dead/GONE → WORKER_DOWN
LOG="${1:-/tmp/ion-phase1.log}"
SOCK="${2:-/tmp/ion-phase1.sock}"
CYCLES="${3:-99999}"
METRICS="/tmp/sandbox-metrics.log"

for cycle in $(seq 1 "$CYCLES"); do
  READ=$(SOCK_="$SOCK" HOSTF=win38-a python3 -c "
import socket, json, os
def rpc():
    s = socket.socket(socket.AF_UNIX); s.settimeout(8)
    s.connect(os.environ['SOCK_'])
    s.sendall(b'{\"id\":\"m1\",\"method\":\"list_workers\",\"params\":{}}\n')
    buf = b''
    while b'\n' not in buf:
        d = s.recv(65536)
        if not d: break
        buf += d
    s.close()
    return json.loads(buf.decode())['data']['workers']
try:
    ws = rpc()
    tgt = [w for w in ws if w.get('host') == os.environ['HOSTF']]
    live = [w for w in tgt if w.get('status') not in ('Stale','Dead','Gone')]
    pick = (live or tgt)[-1] if tgt else None
    print((pick.get('workerId','')+' '+pick.get('status','')+' '+pick.get('sessionId','')[:36]) if pick else 'NONE')
except Exception:
    try:
        ws = rpc()
        tgt = [w for w in ws if w.get('host') == os.environ['HOSTF']]
        live = [w for w in tgt if w.get('status') not in ('Stale','Dead','Gone')]
        pick = (live or tgt)[-1] if tgt else None
        print((pick.get('workerId','')+' '+pick.get('status','')+' '+pick.get('sessionId','')[:36]) if pick else 'NONE')
    except Exception:
        print('HOST_DOWN')
")
  WID=$(echo "$READ" | awk '{print $1}'); ST=$(echo "$READ" | awk '{print $2}'); SID=$(echo "$READ" | awk '{print $3}')

  EV_AGE=-1
  if [ -n "$SID" ] && [ -f "$LOG" ]; then
    EV_AGE=$(grep "$SID" "$LOG" 2>/dev/null | grep -E '"delta"|toolCall|tool_execution_(start|end)' | tail -1 | python3 -c "
import json,sys,time
try:
    d=json.loads(sys.stdin.read()); ts=d.get('timestamp') or d.get('event',{}).get('timestamp',0)
    ts=int(ts or 0)
    if ts <= 0: print(-1); raise SystemExit   # 无时间戳的行（tracing 前缀等）→ 未知
    if ts < 10**12: ts *= 1000  # 秒级时间戳归一为毫秒
    print(max(0,int(time.time()*1000-ts)//1000))
except SystemExit: raise
except Exception: print(-1)" 2>/dev/null)
  fi

  GIT=$(ssh -o ConnectTimeout=4 -p 2222 root@192.168.0.38 'cd /root/work/ion-web-a && git log -1 --format="%ct %h" web/dev' 2>/dev/null)
  C_AGE=$(( ( $(date +%s) - ${GIT%% *} ) / 60 )); C_HASH=${GIT##* }
  PAGE=$(curl -s -o /dev/null -w "%{http_code}" --connect-timeout 3 http://192.168.0.38:5180 2>/dev/null)
  ZURL=$(python3 -c "import json;c=json.load(open('$HOME/.ion/config.json'));print(c['providers']['zai']['base_url'])" 2>/dev/null)
  ZAI=$(curl -s -o /dev/null -w "%{http_code}" --connect-timeout 5 "$ZURL" 2>/dev/null)

  VERDICT="OK"
  [ "$ST" = "Busy" ] && [ "$EV_AGE" -ge 0 ] && [ "$EV_AGE" -gt 120 ] && VERDICT="STALL_SUSPECTED"
  [ "$ST" = "Busy" ] && [ "$EV_AGE" -ge 0 ] && [ "$EV_AGE" -gt 240 ] && VERDICT="STALL_CONFIRMED"
  case "$ST" in GONE|NONE|Dead|Stale) VERDICT="WORKER_DOWN";; esac
  [ "$ST" = "HOST_DOWN" ] && VERDICT="HOST_DOWN"
  [ "$PAGE" != "200" ] && VERDICT="PAGE_DOWN"
  [ "$ZAI" != "200" ] && [ "$ZAI" != "404" ] && [ "$ZAI" != "401" ] && VERDICT="ZAI_DOWN"

  python3 -c "
import json,time
print(json.dumps({'ts':int(time.time()),'worker':'$WID','status':'$ST','event_age_s':$EV_AGE,'commit':'$C_HASH','commit_age_min':$C_AGE,'page':$PAGE,'zai':$ZAI,'verdict':'$VERDICT'},ensure_ascii=False))" >> "$METRICS"

  case "$VERDICT" in
    OK) [ $((cycle % 10)) -eq 1 ] && echo "✅ $(date +%H:%M:%S) $ST 事件年龄=${EV_AGE}s head=$C_HASH 页面=$PAGE";;
    *)  echo "⚠️ $(date +%H:%M:%S) $VERDICT worker=$WID status=$ST 事件年龄=${EV_AGE}s 页面=$PAGE zai=$ZAI";;
  esac
  sleep 30
done
