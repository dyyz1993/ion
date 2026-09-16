#!/bin/bash
# G1 冒烟：bind 仲裁 + pid 文件 hostId 身份（隔离三件套：私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR）
# 纪律：精确 PID 管理（$!），绝不 pkill；不碰默认 host.sock（生产 host 在跑）
set -u
ION_BIN="$(dirname "$0")/../target/debug/ion"
SMOKE_DIR=$(mktemp -d /tmp/ion-smoke-k1.XXXXXX)
export HOME="$SMOKE_DIR/home"
mkdir -p "$HOME"
export ION_HOST_SOCKET="$SMOKE_DIR/host.sock"
export ION_SESSION_DIR="$SMOKE_DIR/sessions"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
check() { # check <desc> <pattern> <file>
  if grep -q "$2" "$3"; then ok "$1"; else bad "$1（期望含 '$2'）"; echo "    实际: $(cat "$3" | head -3)"; fi
}

echo "── T1 status：无 host → not running ──"
OUT=$(timeout 10 "$ION_BIN" serve status 2>&1)
echo "$OUT" | grep -q "Host not running" && ok "T1 未启动时报 not running" || { bad "T1"; echo "$OUT"; }

echo "── T2 干净启动 + status 带 hostId ──"
"$ION_BIN" serve >"$SMOKE_DIR/serve1.log" 2>&1 &
HOST_PID=$!
for i in $(seq 1 100); do [ -S "$ION_HOST_SOCKET" ] && break; sleep 0.1; done
if [ -S "$ION_HOST_SOCKET" ]; then ok "T2a socket 创建"; else bad "T2a socket 未创建"; cat "$SMOKE_DIR/serve1.log"; fi
sleep 0.5  # 等 pid 文件落位
OUT=$(timeout 10 "$ION_BIN" serve status 2>&1)
echo "$OUT" > "$SMOKE_DIR/status1.txt"
echo "$OUT" | grep -Eq "Host running \(pid [0-9]+, hostId [0-9a-f-]{8,}\)" && ok "T2b status 显示 running + hostId" || { bad "T2b"; echo "    实际: $OUT"; }
STATUS_PID=$(echo "$OUT" | grep -oE "pid [0-9]+" | head -1 | grep -oE "[0-9]+")
[ "$STATUS_PID" = "$HOST_PID" ] && ok "T2c status pid 与进程一致 (${HOST_PID})" || bad "T2c status pid=$STATUS_PID vs 进程=$HOST_PID"

echo "── T3 pid 文件 JSON 带 hostId、与 hello 同源 ──"
PID_FILE="$SMOKE_DIR/host.pid"
[ -f "$PID_FILE" ] && ok "T3a pid 文件存在" || bad "T3a pid 文件缺失"
grep -q "\"pid\":$HOST_PID" "$PID_FILE" && ok "T3b pid 字段正确" || bad "T3b: $(cat "$PID_FILE")"
grep -q "\"host_id\":\"" "$PID_FILE" && ok "T3c host_id 字段存在（pid 文件 JSON 用 serde 字段名，hello 协议回 camelCase hostId）" || bad "T3c: $(cat "$PID_FILE")"
PIDF_ID=$(grep -o '"host_id":"[^"]*"' "$PID_FILE" | head -1 | cut -d'"' -f4)
STATUS_ID=$(grep -o "hostId [0-9a-f-]*" "$SMOKE_DIR/status1.txt" | head -1 | cut -d' ' -f2)
[ -n "$PIDF_ID" ] && [ "$PIDF_ID" = "$STATUS_ID" ] && ok "T3d pid 文件 hostId == status 回报（hello 同源）" || bad "T3d pidf=$PIDF_ID status=$STATUS_ID"

echo "── T4 第二实例被拒（单例仲裁，且不动活 host 的 sock 文件）──"
SOCK_INO_BEFORE=$(stat -f '%i' "$ION_HOST_SOCKET")
timeout 15 "$ION_BIN" serve >"$SMOKE_DIR/serve2.log" 2>&1
grep -q "Host already running" "$SMOKE_DIR/serve2.log" && ok "T4a 二实例被拒" || bad "T4a: $(cat "$SMOKE_DIR/serve2.log" | head -3)"
SOCK_INO_AFTER=$(stat -f '%i' "$ION_HOST_SOCKET")
[ "$SOCK_INO_BEFORE" = "$SOCK_INO_AFTER" ] && ok "T4b 活 host 的 sock 文件 inode 未变（没被拆）" || bad "T4b sock 被动过"
kill -0 "$HOST_PID" 2>/dev/null && ok "T4c 原 host 仍存活" || bad "T4c 原 host 死了"

echo "── T5 死链重绑：停 host 后留死链文件 → 新实例清理重绑成功 ──"
# 优雅停第一个 host
timeout 10 "$ION_BIN" serve stop >"$SMOKE_DIR/stop1.log" 2>&1
wait "$HOST_PID" 2>/dev/null
grep -q "Host stopped" "$SMOKE_DIR/stop1.log" && ok "T5a 优雅停机（确认退出）" || bad "T5a: $(cat "$SMOKE_DIR/stop1.log")"
sleep 0.5
kill -0 "$HOST_PID" 2>/dev/null && bad "T5a2 进程未退" || ok "T5a2 进程已退"
# 造死链：普通文件冒充残留 sock（无人监听）
printf 'stale' > "$ION_HOST_SOCKET"
"$ION_BIN" serve >"$SMOKE_DIR/serve3.log" 2>&1 &
HOST_PID2=$!
for i in $(seq 1 100); do [ -S "$ION_HOST_SOCKET" ] && break; sleep 0.1; done
[ -S "$ION_HOST_SOCKET" ] && ok "T5b 死链清理后重绑成功（socket 就绪）" || { bad "T5b"; cat "$SMOKE_DIR/serve3.log" | head -5; }
grep -q "死链" "$SMOKE_DIR/serve3.log" && ok "T5c 死链告警可见" || bad "T5c 无死链告警: $(head -3 "$SMOKE_DIR/serve3.log")"
sleep 0.5
NEW_PIDF_ID=$(grep -o '"host_id":"[^"]*"' "$PID_FILE" 2>/dev/null | head -1 | cut -d'"' -f4)
[ -n "$NEW_PIDF_ID" ] && [ "$NEW_PIDF_ID" != "$PIDF_ID" ] && ok "T5d 重启后 hostId 已换（进程内存态随机）" || bad "T5d new=$NEW_PIDF_ID old=$PIDF_ID"

echo "── T6 收尾：停第二个 host，验证 pid/socket 清理 ──"
timeout 10 "$ION_BIN" serve stop >"$SMOKE_DIR/stop2.log" 2>&1
wait "$HOST_PID2" 2>/dev/null
sleep 0.3
[ ! -e "$ION_HOST_SOCKET" ] && ok "T6a socket 已清理" || bad "T6a socket 残留"
[ ! -e "$PID_FILE" ] && ok "T6b pid 文件已清理" || bad "T6b pid 文件残留"
timeout 10 "$ION_BIN" serve status 2>&1 | grep -q "Host not running" && ok "T6c 终态 not running" || bad "T6c"

rm -rf "$SMOKE_DIR"
echo ""
echo "═══ 冒烟结果: PASS=$PASS FAIL=$FAIL ═══"
[ "$FAIL" -eq 0 ]
