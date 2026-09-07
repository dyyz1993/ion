#!/usr/bin/env bash
#
# session_index_health_ci.sh — T06 CLI 验证：get_index_health RPC + 损坏索引隔离
#
# 隔离环境（独立 HOME + ION_HOST_SOCKET），只 kill 自己启动的 HOST_PID。
#   H1 损坏索引 → get_index_health 报 parse_ok=false
#   H2 host 写事务后 → 损坏文件被隔离保全（内容原样）、新索引可解析、
#      last_issue 记录隔离事件、quarantined_files 可查
#
# 前置：cargo build --bin ion（target/debug/ion 存在）
# 用法：bash tests/session_index_health_ci.sh
#
set -u

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
[ -x "$ION_BIN" ] || { echo "[FAIL] ion binary not found: $ION_BIN (cargo build --bin ion first)"; exit 1; }

SB="$(mktemp -d /tmp/ion-idx-health.XXXXXX)"
export HOME="$SB/home"
export ION_HOST_SOCKET="$SB/host.sock"
mkdir -p "$HOME/.ion/agent"
printf '{{{ NOT JSON AT ALL' > "$HOME/.ion/agent/sessions.index.json"
CORRUPT_CONTENT="$(cat "$HOME/.ion/agent/sessions.index.json")"

P=0
F=0
ok()  { echo "  [PASS] $1"; P=$((P+1)); }
bad() { echo "  [FAIL] $1"; F=$((F+1)); }

"$ION_BIN" serve > "$SB/serve.log" 2>&1 &
HOST_PID=$!
trap 'kill "$HOST_PID" 2>/dev/null; rm -rf "$SB"' EXIT

# 等待 host 就绪（最多 30s）
ready=0
for _ in $(seq 1 30); do
    if "$ION_BIN" rpc --method get_index_health > "$SB/h0.json" 2>&1; then
        ready=1
        break
    fi
    sleep 1
done
[ "$ready" -eq 1 ] || { echo "[FAIL] host not ready; serve.log tail:"; tail -10 "$SB/serve.log"; exit 1; }

# ─── H1: 损坏索引被检测并隔离 ───
# host 启动期的首个写事务（memory-agent 注册等）即触发隔离——
# 不依赖瞬态 parse_ok=false 窗口（实测窗口 < 1s，抢不到）。
H1="$("$ION_BIN" rpc --method get_index_health)"
if echo "$H1" | grep -q '"parse_ok": *false'; then
    ok "H1 corrupt index detected (parse_ok=false)"
elif echo "$H1" | grep -q '\.corrupt-' && echo "$H1" | grep -q 'corrupt/unreadable\|quarantined'; then
    ok "H1 corruption quarantined by startup write (parse_ok already recovered)"
else
    bad "H1 corruption neither detected nor quarantined: $H1"
fi

# ─── H2: host 写事务触发隔离（list_all_sessions 的 heal 路径走 write_txn）───
"$ION_BIN" rpc --method list_all_sessions > /dev/null 2>&1
H2="$("$ION_BIN" rpc --method get_index_health)"
echo "$H2" | grep -q '"parse_ok": *true' \
    && ok "H2 index rebuilt -> parse_ok=true" \
    || bad "H2 parse_ok still false: $H2"

BACKUP="$(ls "$HOME/.ion/agent" | grep '\.corrupt-' | head -1)"
if [ -n "$BACKUP" ]; then
    ok "H2 quarantine backup exists: $BACKUP"
    [ "$(cat "$HOME/.ion/agent/$BACKUP")" = "$CORRUPT_CONTENT" ] \
        && ok "H2 corrupt bytes preserved verbatim" \
        || bad "H2 backup content changed"
else
    bad "H2 no quarantine backup found"
fi
echo "$H2" | grep -q 'quarantined\|corrupt' \
    && ok "H2 last_issue records the quarantine event" \
    || bad "H2 last_issue missing quarantine event"

echo ""
echo "========================================="
echo "  session_index_health_ci: ${P} passed / ${F} failed"
echo "========================================="
[ "$F" -eq 0 ]
