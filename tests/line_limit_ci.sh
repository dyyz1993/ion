#!/usr/bin/env bash
# line_limit CI — JSONL 行长上限（16 MiB，P0.1 对标 pi protocol framing）命令行验证
#
# 验证：
#   G1 host socket：>16MiB 行 → line_too_large 错误帧 + 连接断开（读端不被打爆）
#   G2 边界：16MiB-1 行 → 不触发行长上限（走正常 invalid JSON 路径）
#   G3 回归：小行 RPC 在同一 host 上照常工作
#   G4 worker stdin（直 spawn 无 host）：>16MiB 行 → 错误帧 + worker 退出
#
# 隔离铁律：私有 HOME + 私有 ION_HOST_SOCKET + 私有 ION_SESSION_DIR，绝不碰真实 ~/.ion；
# 只 kill 自己启动的精确 PID（HOST_PID / WORKER_PID），绝不 pkill。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"

PASS=0
FAIL=0
pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

# ── 隔离三件套 ──
TEST_DIR="$(mktemp -d /tmp/ion-line-limit-XXXXXX)"
export HOME="$TEST_DIR/home"
mkdir -p "$HOME" "$TEST_DIR/proj"
export ION_HOST_SOCKET="$TEST_DIR/host.sock"
export ION_SESSION_DIR="$TEST_DIR/sessions"
export ION_FAUX_REPLY="${ION_FAUX_REPLY:-line limit ci ready}"

HOST_PID=""
WORKER_PID=""
cleanup() {
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && wait "$HOST_PID" 2>/dev/null
    [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null
    [ -n "$WORKER_PID" ] && wait "$WORKER_PID" 2>/dev/null
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

# ── socket 客户端：发一行（payload 由 python 自建 'x'*n，避免 argv/env 超 ARG_MAX）
#    → 读到 EOF，逐行输出帧 ──
# 用法: sock_send_line_till_eof <socket> <行字节数> → stdout 每行一个 JSON 帧
sock_send_line_till_eof() {
    SOCK_="$1" NBYTES_="$2" python3 -c '
import socket, os
s = socket.socket(socket.AF_UNIX); s.settimeout(30)
s.connect(os.environ["SOCK_"])
payload = b"x" * int(os.environ["NBYTES_"])
chunk = 256 * 1024
try:
    for i in range(0, len(payload), chunk):
        s.sendall(payload[i:i+chunk])
    s.sendall(b"\n")
except (BrokenPipeError, ConnectionResetError, OSError):
    pass  # host 拒收（超限断开）是预期路径之一
buf = b""
try:
    while True:
        s.settimeout(10)
        d = s.recv(65536)
        if not d: break
        buf += d
        while b"\n" in buf:
            raw, buf = buf.split(b"\n", 1)
            t = raw.decode(errors="replace").strip()
            if t: print(t)
except (socket.timeout, OSError):
    pass
s.close()
'
}

echo "=== 启动隔离 host（HOME=$HOME sock=${ION_HOST_SOCKET}）==="
"$ION_BIN" serve > "$TEST_DIR/host.log" 2>&1 &
HOST_PID=$!
HOST_READY=0
for _ in $(seq 1 20); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        HOST_READY=1; break
    fi
done
if [ "$HOST_READY" -eq 1 ]; then pass "host 启动（PID=${HOST_PID}，私有 HOME/socket）"; else
    fail "host 启动失败"; tail -5 "$TEST_DIR/host.log" | sed 's/^/     /'; exit 1; fi

echo
echo "=== G1 host socket：17MiB 行 → line_too_large 错误帧 + 断开 ==="
G1_OUT=$(sock_send_line_till_eof "$ION_HOST_SOCKET" $((17 * 1024 * 1024)))
G1_LINES=$(printf '%s\n' "$G1_OUT" | grep -c . 2>/dev/null || echo 0)
G1_TYPE=$(printf '%s' "$G1_OUT" | head -1 | jq -r '.type' 2>/dev/null)
G1_LIMIT=$(printf '%s' "$G1_OUT" | head -1 | jq -r '.limitBytes' 2>/dev/null)
if [ "$G1_TYPE" = "error" ]; then pass "G1.1 收到 error 帧共 ${G1_LINES} 条"; else
    fail "G1.1 首帧 type（got: $(printf '%s' "$G1_OUT" | head -c 200)）"; fi
if [ "$G1_LIMIT" = "16777216" ]; then pass "G1.2 limitBytes=16777216（16MiB）"; else
    fail "G1.2 limitBytes（got: ${G1_LIMIT}）"; fi
if printf '%s' "$G1_OUT" | head -1 | jq -r '.error' 2>/dev/null | grep -q "line too large"; then
    pass "G1.3 错误信息含 line too large"
else fail "G1.3 错误信息（got: $(printf '%s' "$G1_OUT" | head -1 | jq -r '.error' 2>/dev/null)）"; fi
if printf '%s' "$G1_OUT" | head -1 | jq -r '.actualBytes' 2>/dev/null | grep -qE '^[0-9]+$'; then
    pass "G1.4 actualBytes 报告超限实际字节"
else fail "G1.4 actualBytes 缺失"; fi
# 断开：错误帧之后 host 应关闭连接 → 客户端 recv EOF（脚本在 EOF 后自然结束）
if [ "$G1_LINES" = "1" ]; then pass "G1.5 错误帧后连接已断开（无后续帧）"; else
    fail "G1.5 断开语义（帧数: ${G1_LINES}）"; fi

echo
echo "=== G2 边界：16MiB-1 行 → 不触发行长上限（走 invalid JSON 正常路径）==="
G2_OUT=$(sock_send_line_till_eof "$ION_HOST_SOCKET" $((16 * 1024 * 1024 - 1)))
G2_ERR=$(printf '%s' "$G2_OUT" | head -1 | jq -r '.error // empty' 2>/dev/null)
if printf '%s' "$G2_ERR" | grep -q "invalid JSON"; then
    pass "G2.1 边界行走 invalid JSON（未触发行长上限）"
else fail "G2.1 边界行（got: ${G2_ERR:0:120}）"; fi
if printf '%s' "$G2_ERR" | grep -q "line too large"; then
    fail "G2.2 边界行被误判超限"
else pass "G2.2 边界行未误判超限（上限含语义：超过才拒）"; fi

echo
echo "=== G3 回归：小行 RPC 照常工作 ==="
G3_RESP=$("$ION_BIN" rpc --method list_sessions --params '{}' 2>/dev/null)
if printf '%s' "$G3_RESP" | jq -e '.success == true' > /dev/null 2>&1; then
    pass "G3.1 ion rpc 小请求正常（success=true）"
else fail "G3.1 ion rpc 回归（got: $(printf '%s' "$G3_RESP" | head -c 120)）"; fi

echo
echo "=== G4 worker stdin（直 spawn 无 host）：17MiB 行 → 错误帧 + 退出 ==="
# python 全程托管 worker 子进程：喂 17MiB 超限行 → 收帧 + 等退出（精确进程，无 pkill）
G4_FRAMES="$TEST_DIR/g4_frames.log"
G4_EXIT=$(python3 - "$ION_BIN" "$G4_FRAMES" << 'PYEOF'
import subprocess, sys, threading, time
bin_path, out_path = sys.argv[1], sys.argv[2]
p = subprocess.Popen([bin_path, "--mode", "rpc", "--session", "line-limit-ci"],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
def feed():
    try:
        for _ in range(17):
            p.stdin.write(b"x" * (1024 * 1024))
        p.stdin.write(b"\n")
        p.stdin.flush()
    except (BrokenPipeError, OSError, ValueError):
        pass  # worker 超限拒绝后退出 → EPIPE 是预期
threading.Thread(target=feed, daemon=True).start()
deadline = time.time() + 25
with open(out_path, "w") as f:
    while time.time() < deadline:
        line = p.stdout.readline()
        if not line: break
        f.write(line.decode(errors="replace"))
        f.flush()
try:
    code = p.wait(timeout=10)
    print(code)
except subprocess.TimeoutExpired:
    p.kill(); print("timeout")
PYEOF
)
G4_TYPE=$(grep -m1 '"type":"error"' "$G4_FRAMES" 2>/dev/null | jq -r '.type' 2>/dev/null)
G4_LIMIT=$(grep -m1 '"type":"error"' "$G4_FRAMES" 2>/dev/null | jq -r '.limitBytes' 2>/dev/null)
if [ "$G4_TYPE" = "error" ] && [ "$G4_LIMIT" = "16777216" ]; then
    pass "G4.1 worker stdin 超限 → line_too_large 错误帧（limitBytes=16777216）"
else fail "G4.1 worker 错误帧（got: $(head -c 200 "$G4_FRAMES" 2>/dev/null)）"; fi
G4_ERR_N=$(grep -c 'line too large' "$G4_FRAMES" 2>/dev/null || echo 0)
if [ "$G4_ERR_N" = "1" ]; then
    pass "G4.2 恰好 1 条 line_too_large 错误帧（无重复/循环；启动帧与错误帧并发乱序属正常）"
else fail "G4.2 line_too_large 帧数: ${G4_ERR_N}"; fi
case "$G4_EXIT" in
    ''|timeout) fail "G4.3 worker 未在期限内退出（exit=${G4_EXIT}）" ;;
    *) pass "G4.3 worker 已退出（exit code=${G4_EXIT}）" ;;
esac

echo
echo "======================================"
echo "line_limit CI：PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
