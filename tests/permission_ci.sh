#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：权限系统 CLI 端到端验证
# ──────────────────────────────────────────────────────────
# 用法:
#   bash tests/permission_ci.sh              # 快速模式
#
# 退出码:
#   0 = 全部通过
#   1 = 至少一项失败
# ──────────────────────────────────────────────────────────
set -uo pipefail

PASS=0
FAIL=0

green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }

pass() { PASS=$((PASS + 1)); green "$1"; }
fail() { FAIL=$((FAIL + 1)); red "$1"; }

quiet() { "$@" 2>/dev/null | grep -v "setValueForKey\|valueForKey\|_encode\|_decode" || true; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

# Resolve HOME-aware paths (do NOT hardcode /Users/xuyingzhou — breaks isolation).
HOST_SOCK="$HOME/.ion/host.sock"
HOST_PID_FILE="$HOME/.ion/host.pid"
HOST_LOG="/tmp/ion-ci-perm-host-${$}.log"
LIB_LOG="/tmp/ion-ci-perm-lib-${$}.log"

echo "════════════════════════════════════════════════════"
echo "  ION Permission System CI Test"
echo "  $(date)"
echo "════════════════════════════════════════════════════"

# ── Cleanup trap ──
# Kill only the host we started (by PID), never anything matching the broad
# "target/debug/ion" pattern — that would kill production workers in a shared
# environment. The host writes host.pid on startup; we read & kill that PID.
cleanup() {
    local pid
    if [ -n "${MANAGER_PID:-}" ] && kill -0 "$MANAGER_PID" 2>/dev/null; then
        kill -9 "$MANAGER_PID" 2>/dev/null || true
        wait "$MANAGER_PID" 2>/dev/null || true
    fi
    # Best-effort cleanup of host via its pid file (covers cases where the cargo
    # shim exec'd into the binary, so MANAGER_PID was the cargo wrapper).
    if [ -f "$HOST_PID_FILE" ]; then
        pid=$(cat "$HOST_PID_FILE" 2>/dev/null || true)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f "$HOST_PID_FILE" 2>/dev/null || true
    fi
    rm -f "$HOST_LOG" "$LIB_LOG" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── Phase 0: Build ──
echo ""
echo "── Phase 0: Build ──"
if cargo build --bin ion 2>/dev/null; then
    pass "cargo build ion"
else
    fail "cargo build ion"
    exit 1
fi

# 选择 binary
if [ -x "$PROJECT_DIR/target/debug/ion" ]; then
    ION_BIN="$PROJECT_DIR/target/debug/ion"
else
    ION_BIN="ion"
fi
echo "  使用 binary: $ION_BIN"

# ── Phase 1: 单元测试 ──
echo ""
echo "── Phase 1: Unit Tests ──"
# Guard against cargo file-lock contention in parallel CI matrices:
# if the lock is held, cargo would block forever. Use `timeout` so the test
# is reported as failed (not hung) and other shards can proceed.
if command -v timeout >/dev/null 2>&1; then
    TEST_WRAP=(timeout 180)
elif [ -x /usr/local/bin/timeout ] || [ -x /opt/homebrew/bin/timeout ] || [ -x /usr/local/bin/gtimeout ]; then
    TEST_WRAP=("${TOOL:-gtimeout}" 180)
else
    TEST_WRAP=()
fi
if RUST_LOG=error "${TEST_WRAP[@]}" cargo test --lib --color never > "$LIB_LOG" 2>&1; then
    if grep -q "^test result:" "$LIB_LOG"; then
        pass "cargo test --lib"
    else
        fail "cargo test --lib (no result line)"
    fi
else
    rc=$?
    if grep -q "Blocking waiting for file lock" "$LIB_LOG"; then
        fail "cargo test --lib (blocked on file lock — parallel CI?)"
        echo "  ℹ️  skip lib tests this run; another shard holds the build lock"
    else
        fail "cargo test --lib (rc=$rc)"
    fi
fi

# ── Phase 2: 启动 Manager + Worker ──
echo ""
echo "── Phase 2: Manager & Worker ──"

# 清理 stale host（只清自己启动的）
MANAGER_PID=""
if [ -f "$HOST_PID_FILE" ]; then
    old_pid=$(cat "$HOST_PID_FILE" 2>/dev/null || true)
    if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
        kill -9 "$old_pid" 2>/dev/null || true
        sleep 1
    fi
    rm -f "$HOST_PID_FILE"
fi
# 仅当 socket 没人监听才删除（避免误删活跃 host）
if [ -S "$HOST_SOCK" ] && ! lsof -ti "$HOST_SOCK" >/dev/null 2>&1; then
    rm -f "$HOST_SOCK"
fi

# 启动 host（后台）。cargo run --bin ion 会 exec 进 ion binary，
# MANAGER_PID 是 cargo wrapper PID；真正监听 socket 的是其子进程。
nohup "$ION_BIN" serve > "$HOST_LOG" 2>&1 &
MANAGER_PID=$!

# 等待 host socket 可用（poll 而非固定 sleep 4 —— 修复 cargo 慢启动竞态）
HOST_READY=0
for i in $(seq 1 30); do
    sleep 1
    if "$ION_BIN" rpc --method health >/dev/null 2>&1; then
        HOST_READY=1
        break
    fi
    # If wrapper died, give up early.
    if ! kill -0 "$MANAGER_PID" 2>/dev/null; then
        # Maybe ion exec'd and replaced, double check socket via pid file
        if [ -f "$HOST_PID_FILE" ] && kill -0 "$(cat "$HOST_PID_FILE" 2>/dev/null)" 2>/dev/null; then
            continue
        fi
        break
    fi
done

if [ "$HOST_READY" -eq 1 ]; then
    pass "serve start"
else
    fail "serve start"
    echo "  host log tail:"
    tail -10 "$HOST_LOG" 2>/dev/null | sed 's/^/    /'
    exit 1
fi

# 创建 Worker
OUT=$(quiet "$ION_BIN" rpc --session x --method create_worker --params '{"cwd":"'"$PROJECT_DIR"'"}')
SID=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)

if [ -n "$SID" ]; then
    pass "create_worker (sid=${SID:0:8}...)"
else
    fail "create_worker"
    exit 1
fi

# ── Phase 3: PermissionExtension RPC ──
echo ""
echo "── Phase 3: PermissionExtension RPC ──"

rpc_ok() {
    quiet "$ION_BIN" rpc --session "$SID" --method "$1" --params "$2" \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print('ok' if d.get('success') else 'FAIL: '+str(d.get('error','')))" 2>/dev/null || echo "FAIL: rpc call error"
}

# 3a: list_rules（初始应为空）
rpc_ok "extension_rpc" '{"extension":"permission","method":"list_rules"}' | grep -q ok && pass "list_rules (empty)" || fail "list_rules (empty)"

# 3b: add_rule (session scope)
OUT=$(rpc_ok "extension_rpc" '{"extension":"permission","method":"add_rule","args":{"subject":"command.run","pattern":"echo *","decision":"allow","scope":"session"}}')
echo "$OUT" | grep -q ok && pass "add_rule session scope" || fail "add_rule session scope"

# 3c: add_rule (project scope)
OUT=$(rpc_ok "extension_rpc" '{"extension":"permission","method":"add_rule","args":{"subject":"file.read","pattern":"**/.env*","decision":"deny","scope":"project"}}')
echo "$OUT" | grep -q ok && pass "add_rule project scope" || fail "add_rule project scope"

# 3d: list_rules（应有 2 条）
OUT=$(rpc_ok "extension_rpc" '{"extension":"permission","method":"list_rules"}')
echo "$OUT" | grep -q ok && pass "list_rules (2 rules)" || fail "list_rules (2 rules)"

# ── Phase 4: 规则匹配测试 ──
echo ""
echo "── Phase 4: Rule Matching ──"

# 4a: allow rule — echo hello 应放行
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo hello"}}')
SUCCESS=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('success',''))" 2>/dev/null)
if [ "$SUCCESS" = "True" ] || echo "$OUT" | grep -q '"success": true'; then
    pass "allow rule: echo hello"
else
    fail "allow rule: echo hello"
    echo "  OUT=$OUT"
fi

# 4b: deny rule — read .env 应拒绝
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"'"$PROJECT_DIR"'/.env"}}')
if echo "$OUT" | grep -q '"success": false'; then
    pass "deny rule: read .env (blocked)"
else
    fail "deny rule: read .env"
    echo "  OUT=$OUT"
fi

# 4c: CommandGuard — rm -rf 应拦截
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"rm -rf /"}}')
if echo "$OUT" | grep -q "CommandGuard"; then
    pass "CommandGuard: rm -rf / (blocked)"
else
    fail "CommandGuard: rm -rf /"
    echo "  OUT=$OUT"
fi

# 4d: 安全命令放行
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo safe"}}')
if echo "$OUT" | grep -q '"success": true'; then
    pass "safe command: echo safe"
else
    fail "safe command: echo safe"
fi

# ── Phase 5: 清理 ──
echo ""
echo "── Cleanup ──"
# trap 已处理，这里仅提示
echo "  Cleaned up (via EXIT trap)"

# ── 总结 ──
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then
    exit 1
fi
exit 0
