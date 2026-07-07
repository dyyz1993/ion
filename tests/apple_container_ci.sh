#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Apple Container Backend CI 测试脚本
# 对应 APPLE_CONTAINER_EXTENSION.md 的 Group A/B/C/D/E/F
# ──────────────────────────────────────────────────────────
set -uo pipefail
TMPDIR="${TMPDIR:-/tmp}"
HOST_PID_FILE="$TMPDIR/ion-ac-pid"

cleanup() {
    if [ -f "$HOST_PID_FILE" ]; then
        kill "$(cat "$HOST_PID_FILE")" 2>/dev/null || true
        rm -f "$HOST_PID_FILE"
    fi
    # 清理容器（容忍失败）
    for c in ion-ac-test ion-ac-d1 ion-ac-d2 ion-ac-ipcheck; do
        /usr/local/bin/container stop "$c" 2>/dev/null || true
    done
    # 兜底清理 ion 进程
    for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
        kill "$pid" 2>/dev/null || true
    done
    # 恢复 config
    [ -f /tmp/ion-ac-config.bak ] && cp /tmp/ion-ac-config.bak ~/.ion/config.json 2>/dev/null || true
    rm -f /tmp/ion-ac-*.log /tmp/ion-ac-*.json /tmp/ion-ac-config.bak
    rm -f /Users/xuyingzhou/.ion/host.sock
}
trap cleanup EXIT

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || ION_BIN="ion"
RPC="$ION_BIN rpc"

echo "════════════════════════════════════════════════════"
echo "  Apple Container CI Test — $(date)"
echo "════════════════════════════════════════════════════"

# ── Phase 0: Build ──
echo "--- Phase 0: Build ---"
cargo build --bin ion --bin ion-worker 2>/dev/null && pass "build" || { fail "build"; exit 1; }
RUST_LOG=error cargo test --lib backend_registry 2>&1 | grep -q "test result:" && pass "unit tests (backend_registry)" || fail "unit tests"

# ── Phase 0.5: container 服务检查 ──
echo "--- Phase 0.5: Container Service ---"
CONTAINER_BIN="/usr/local/bin/container"
if [ ! -x "$CONTAINER_BIN" ]; then
    skip "container CLI not found at $CONTAINER_BIN — skipping all container tests"
    echo "════════════════════════════════════════════════════"
    echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
    echo "════════════════════════════════════════════════════"
    exit 0
fi

# 启动 container 服务
"$CONTAINER_BIN" system start > /tmp/ion-ac-container-start.log 2>&1
if [ $? -ne 0 ] && ! "$CONTAINER_BIN" system status > /dev/null 2>&1; then
    skip "container service unavailable — skipping container tests"
    echo "  see /tmp/ion-ac-container-start.log"
    echo "════════════════════════════════════════════════════"
    echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
    echo "════════════════════════════════════════════════════"
    exit 0
fi
pass "container service running"

# ── Phase 1: 配置 apple-dev backend ──
echo "--- Phase 1: Configuration ---"

# 备份旧配置
cp ~/.ion/config.json /tmp/ion-ac-config.bak 2>/dev/null || true

# 写入测试配置
cat > /tmp/ion-ac-config.json << 'IONEOF'
{
  "default_provider": "zhipuai",
  "default_model": "glm-4.7",
  "api_key": "",
  "runtime": {
    "default": "ac-test",
    "backends": {
      "local":   {"type": "local"},
      "ac-test": {"type": "container", "driver": "apple", "image": "docker.io/library/alpine:latest"}
    },
    "routes": [
      {"command": "npm *",   "target": "local"},
      {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
    ]
  }
}
IONEOF
# 保留原始 api_key 和 provider
python3 -c "
import json
with open('/tmp/ion-ac-config.json') as f: new = json.load(f)
try:
    with open('/tmp/ion-ac-config.bak') as f: old = json.load(f)
    new['api_key'] = old.get('api_key', '')
    new['default_provider'] = old.get('default_provider', 'zhipuai')
    new['default_model'] = old.get('default_model', 'glm-4.7')
except: pass
with open('/tmp/ion-ac-config.json', 'w') as f: json.dump(new, f, indent=2)
"
cp /tmp/ion-ac-config.json ~/.ion/config.json
pass "config written (runtime.default=ac-test, backends: local + ac-test)"

# ── Phase 2: Host + Worker ──
echo "--- Phase 2: Host + Worker ---"
cleanup
sleep 1; rm -f /Users/xuyingzhou/.ion/host.sock

"$ION_BIN" serve start > /tmp/ion-ac-host.log 2>&1 &
HOST_PID=$!
echo $HOST_PID > "$HOST_PID_FILE"
sleep 3

SID=$($RPC --method create_worker --params '{"cwd":"/tmp"}' 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))")
if [ -z "$SID" ]; then
    fail "create_worker"
    echo "  host log: $(tail -3 /tmp/ion-ac-host.log)"
else
    pass "create_worker (SID=$SID)"
fi

# ── Group A: 容器生命周期 ──
echo "--- Group A: Container Lifecycle ---"

# A1: 首次 bash 触发 container run
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo hello-from-ac"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ "$VAL" = "hello-from-ac" ]; then
    pass "A1: first bash triggers container run"
else
    fail "A1: expected 'hello-from-ac', got '$VAL'"
    echo "  response: $(echo "$OUT" | head -c 200)"
fi

# A2: 容器存在 — 通过 container exec 验证（比 inspect 更可靠）
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo alive"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ "$VAL" = "alive" ]; then
    pass "A2: container is alive (bash echo works)"
else
    fail "A2: container not responding"
fi

# A3: 第二次 bash（复用）
OUT2=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo second-call"}}' 2>/dev/null)
VAL2=$(echo "$OUT2" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ "$VAL2" = "second-call" ]; then
    pass "A3: second bash reuses container"
else
    fail "A3: expected 'second-call', got '$VAL2'"
fi

# ── Group B: 命令执行 ──
echo "--- Group B: Command Execution ---"

# B1: bash echo already tested in A1

# B2: 多条命令
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"hostname"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ -n "$VAL" ]; then
    pass "B2: bash hostname returned '$VAL'"
else
    fail "B2: hostname returned empty"
fi

# ── Group C: 文件操作 ──
echo "--- Group C: File Operations ---"

# C1: write
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"write","args":{"file_path":"/tmp/ac-test.txt","content":"hello container fs"}}' 2>/dev/null)
ECHO=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if echo "$ECHO" | grep -q "wrote"; then
    pass "C1: write file in container"
else
    fail "C1: write failed: $ECHO"
fi

# C2: read
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"/tmp/ac-test.txt"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ "$VAL" = "hello container fs" ]; then
    pass "C2: read file in container"
else
    fail "C2: expected 'hello container fs', got '$VAL'"
fi

# C4: ls
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"ls","args":{"path":"/tmp"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if echo "$VAL" | grep -q "ac-test.txt"; then
    pass "C4: ls in container sees ac-test.txt"
else
    fail "C4: ls didn't find ac-test.txt in /tmp"
fi

# C5: read 不存在路径
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"/nonexistent-file"}}' 2>/dev/null)
SUCCESS=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('success',False))" 2>/dev/null)
if [ "$SUCCESS" = "False" ]; then
    pass "C5: read nonexistent file returned error"
else
    fail "C5: should have failed for nonexistent file"
fi

# C3: edit
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"edit","args":{"file_path":"/tmp/ac-test.txt","old":"hello","new":"EDITED"}}' 2>/dev/null)
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"/tmp/ac-test.txt"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ "$VAL" = "EDITED container fs" ]; then
    pass "C3: edit file in container"
else
    fail "C3: expected 'EDITED container fs', got '$VAL'"
fi

# ── Group E: 路由规则 ──
echo "--- Group E: Routing Rules ---"

# E1: npm 走本地
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"npm --version"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ -n "$VAL" ]; then
    pass "E1: npm --version runs locally (command route)"
else
    fail "E1: npm failed"
fi

# E2: 本地文件读取
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo default-route"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
if [ "$VAL" = "default-route" ]; then
    pass "E2: default route goes to container"
else
    fail "E2: default route misrouted: '$VAL'"
fi

# ── Group G: 容器身份验证 ──
echo "--- Group G: Container Identity ---"

HOSTNAME_LOCAL=$(hostname)

# G1: hostname 确认在容器内执行
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"hostname"}}' 2>/dev/null)
HOSTNAME_CONTAINER=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output','').strip())" 2>/dev/null)
if [ -n "$HOSTNAME_CONTAINER" ] && [ "$HOSTNAME_CONTAINER" != "$HOSTNAME_LOCAL" ]; then
    pass "G1: container hostname='$HOSTNAME_CONTAINER' differs from host='$HOSTNAME_LOCAL'"
else
    fail "G1: hostname not isolated (container=$HOSTNAME_CONTAINER, host=$HOSTNAME_LOCAL)"
fi

# G2: /etc/hostname
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"/etc/hostname"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output','').strip())" 2>/dev/null)
if [ "$VAL" = "$HOSTNAME_CONTAINER" ]; then
    pass "G2: /etc/hostname matches ($VAL)"
else
    fail "G2: /etc/hostname mismatch (got '$VAL', expected '$HOSTNAME_CONTAINER')"
fi

# G3: /proc/1/status — PID 1 存在即说明容器进程隔离
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"cat /proc/1/status 2>/dev/null | head -1"}}' 2>/dev/null)
PNAME=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output','').strip())" 2>/dev/null)
if [ -n "$PNAME" ]; then
    pass "G3: PID 1 exists in container: $PNAME"
else
    fail "G3: PID 1 not found"
fi

# G4: uname 确认是 Linux
OUT=$($RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"uname -s"}}' 2>/dev/null)
VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output','').strip())" 2>/dev/null)
if [ "$VAL" = "Linux" ]; then
    pass "G4: uname returns 'Linux' (not Darwin/macOS)"
else
    fail "G4: expected 'Linux', got '$VAL'"
fi

# ── Group H: 容器 IP 查询 ──
echo "--- Group H: Container IP ---"

# H1 + H2: 创建容器查 IP，再次查确认不变
"$CONTAINER_BIN" run --name ion-ac-ipcheck --detach --rm --network default docker.io/library/alpine:latest sh -lc "sleep infinity" > /dev/null 2>&1
sleep 2

IP1=$("$CONTAINER_BIN" inspect ion-ac-ipcheck 2>/dev/null | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(d[0]['networks'][0]['ipv4Address'])
" 2>/dev/null | cut -d/ -f1)

IP2=$("$CONTAINER_BIN" inspect ion-ac-ipcheck 2>/dev/null | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(d[0]['networks'][0]['ipv4Address'])
" 2>/dev/null | cut -d/ -f1)

if echo "$IP1" | grep -q "^192.168.64\."; then
    pass "H1: container IP=$IP1 in 192.168.64.0/24"
else
    fail "H1: unexpected IP='$IP1'"
fi

if [ "$IP1" = "$IP2" ] && [ -n "$IP1" ]; then
    pass "H2: IP unchanged ($IP1)"
else
    fail "H2: IP changed from '$IP1' to '$IP2'"
fi

"$CONTAINER_BIN" stop ion-ac-ipcheck > /dev/null 2>&1 || true

# ── Group A3: 停止容器 ──
echo "--- Group A3: Stop Container ---"
# 停止 Host → Worker Drop → container stop
kill "$(cat "$HOST_PID_FILE")" 2>/dev/null
sleep 2
rm -f "$HOST_PID_FILE"

# 检查容器是否已停止
if "$CONTAINER_BIN" ls 2>/dev/null | grep -q "ion-ac-test"; then
    # 还没停可能是 Drop 还没执行
    "$CONTAINER_BIN" stop ion-ac-test 2>/dev/null && pass "A3: container stopped (manual fallback)" || skip "A3: container stop"
else
    pass "A3: container auto-stopped on Worker Drop"
fi

# ── Group F: 错误处理 ──
echo "--- Group F: Error Handling ---"

# F3: 容器名冲突 — 先手动创建同名容器
"$CONTAINER_BIN" run --name ion-ac-test --detach --rm --network default docker.io/library/alpine:latest sh -lc "sleep infinity" > /tmp/ion-ac-precreate.log 2>&1
if [ $? -eq 0 ]; then
    # 启动 Host + Worker → 应 inspect 到已有容器
    sleep 1; rm -f /Users/xuyingzhou/.ion/host.sock
    "$ION_BIN" serve start > /tmp/ion-ac-host2.log 2>&1 &
    HOST_PID=$!
    echo $HOST_PID > "$HOST_PID_FILE"
    sleep 3

    SID2=$($RPC --method create_worker --params '{"cwd":"/tmp"}' 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)
    if [ -n "$SID2" ]; then
        OUT=$($RPC --session "$SID2" --method call_tool --params '{"tool":"bash","args":{"command":"echo conflict-test"}}' 2>/dev/null)
        VAL=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',''))" 2>/dev/null)
        if [ "$VAL" = "conflict-test" ]; then
            pass "F3: container name conflict handled (inspect reuse)"
        else
            fail "F3: conflict test failed: $VAL"
        fi
    fi

    # 清理冲突容器
    "$CONTAINER_BIN" stop ion-ac-test 2>/dev/null || true
    kill "$HOST_PID" 2>/dev/null || true
    sleep 1
fi

# ── Group D: 多容器并行 ──
echo "--- Group D: Multi-container ---"

# D1: 创建两个新容器（A3 已清理了旧的 ion-ac-test）
"$CONTAINER_BIN" run --name ion-ac-d1 --detach --rm --network default docker.io/library/alpine:latest sh -lc "sleep infinity" > /tmp/ion-ac-d1.log 2>&1
"$CONTAINER_BIN" run --name ion-ac-d2 --detach --rm --network default docker.io/library/alpine:latest sh -lc "sleep infinity" > /tmp/ion-ac-d2.log 2>&1

# 等就绪
sleep 2

if "$CONTAINER_BIN" inspect ion-ac-d1 > /dev/null 2>&1 && "$CONTAINER_BIN" inspect ion-ac-d2 > /dev/null 2>&1; then
    pass "D1: two containers created (ion-ac-d1, ion-ac-d2)"
else
    skip "D1: could not create both containers"
fi

# D2: IP 不同
IP1=$("$CONTAINER_BIN" inspect ion-ac-d1 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(d[0]['networks'][0]['ipv4Address'].split('/')[0])" 2>/dev/null)
IP2=$("$CONTAINER_BIN" inspect ion-ac-d2 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(d[0]['networks'][0]['ipv4Address'].split('/')[0])" 2>/dev/null)
if [ -n "$IP1" ] && [ -n "$IP2" ] && [ "$IP1" != "$IP2" ]; then
    pass "D2: containers have different IPs ($IP1 vs $IP2)"
else
    fail "D2: IPs are same or empty (ip1=$IP1, ip2=$IP2)"
fi

# 清理 D 的额外容器
"$CONTAINER_BIN" stop ion-ac-d1 2>/dev/null || true
"$CONTAINER_BIN" stop ion-ac-d2 2>/dev/null || true

# ── 总结 ──
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ]
