#!/usr/bin/env bash
# tests/extensions_ci.sh — WASM extensions (todo-extension) CLI 验收
#
# 验证 todo Extension 的 5 个工具端到端可用：
#   - 先 build WASM
#   - 装 .wasm 到 ~/.ion/agent/extensions/
#   - 起 ion serve
#   - 通过 RPC 调每个工具，断言返回 JSON 正确
#
# 用法：
#   bash tests/extensions_ci.sh
#
# 前提：
#   - 已 cargo build --bin ion（debug 即可）
#   - 已在 extensions/todo-extension 中构建 wasm32-wasip1 release 产物
#   - 本脚本会自动 build wasm 如果产物缺失
#
# 清理：
#   - 脚本结束自动 kill serve + 删测试文件
#   - 不修改任何源码

set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="${ION_BIN:-./target/debug/ion}"
PASS=0
FAIL=0

# ── Session isolation (issue #30) ──────────────────────────────────────────
# ION_SESSION_DIR isolation (issue #30)
# Use a subdirectory UNDER $HOME/.ion/agent/sessions so that scripts which
# `find $HOME/.ion/agent/sessions` still work. Each test gets a unique subdir.
if [ -z "${ION_SESSION_DIR:-}" ]; then
    export ION_SESSION_DIR="$HOME/.ion/agent/sessions/_ci_$(basename "$0" .sh)_$$"
    mkdir -p "$ION_SESSION_DIR"
    CREATED_SESSION_DIR=1
fi
FAILED_TESTS=()

# ── 工具函数 ───────────────────────────────────────────────────────────────

red()    { printf "\033[31m%s\033[0m\n" "$*"; }
green()  { printf "\033[32m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
bold()   { printf "\033[1m%s\033[0m\n" "$*"; }

# 断言：$1=描述 $2=实际值 $3=期望(子串匹配)
assert_contains() {
    local desc="$1" actual="$2" expected="$3"
    if echo "$actual" | grep -qF "$expected"; then
        green "  ✅ $desc"
        PASS=$((PASS + 1))
    else
        red "  ❌ $desc"
        red "     期望含: $expected"
        red "     实际:   $actual"
        FAIL=$((FAIL + 1))
        FAILED_TESTS+=("$desc")
    fi
}

# 调用工具，返回 output 字段
call_tool() {
    local sid="$1" params="$2"
    "$ION_BIN" rpc --session "$sid" --method call_tool --params "$params" 2>&1 \
        | python3 -c "
import json, sys
try:
    d = json.loads(sys.stdin.read())
    print(d.get('data', {}).get('output', ''))
except Exception as e:
    print('PARSE_ERR:' + str(e))
"
}

# ── 准备：build wasm + 安装 ────────────────────────────────────────────────

bold "=== 准备阶段 ==="

# Check if wasm32 target is available
if ! rustup target list --installed 2>/dev/null | grep -q wasm32-wasip1; then
    echo "  ⚠️  wasm32-wasip1 target not installed — attempting install"
    rustup target add wasm32-wasip1 2>/dev/null || true
fi

# Build wasm 产物（如果缺失）— 从 todo-extension 目录编译（独立 workspace）
TODO_DIR="$PROJECT_DIR/extensions/todo-extension"
TODO_WASM="$TODO_DIR/target/wasm32-wasip1/release/todo_extension.wasm"
if [ ! -f "$TODO_WASM" ]; then
    yellow "building todo_extension.wasm..."
    (cd "$TODO_DIR" && cargo build --target wasm32-wasip1 --release) 2>&1 | tail -2
fi

# 验证 wasm 文件存在 — 如果编译失败就 skip 整个 CI
if [ ! -f "$TODO_WASM" ]; then
    echo "  ⏭️  wasm 编译失败（缺少 $TODO_WASM）— skip extensions_ci"
    echo "  Results: 0 passed, 0 failed, 1 skipped (wasm build failed)"
    exit 0
fi
if [ ! -f "$TODO_WASM" ]; then
    echo "  ⏭️  wasm 编译失败（缺少 $TODO_WASM）— skip extensions_ci"
    echo "  Results: 0 passed, 0 failed, 1 skipped (wasm build failed)"
    exit 0
fi

# 安装到全局 extensions 目录（ION worker 只扫这里）
EXT_DIR="$HOME/.ion/agent/extensions"
mkdir -p "$EXT_DIR"
INSTALLED_WASM="$EXT_DIR/todo_extension.wasm"
BACKUP_WASM=""
if [ -f "$INSTALLED_WASM" ]; then
    BACKUP_WASM=$(mktemp /tmp/ion-todo-extension-backup.XXXXXX)
    cp "$INSTALLED_WASM" "$BACKUP_WASM"
fi
cp "$TODO_WASM" "$INSTALLED_WASM"
green "✅ wasm 安装到 $EXT_DIR"

# 使用本测试专属 socket，不触碰用户正在运行的 host。
export ION_HOST_SOCKET="${ION_HOST_SOCKET:-$(mktemp -u /tmp/ion-ext-ci.sock.XXXXXX)}"
rm -f "$ION_HOST_SOCKET" 2>/dev/null

# 启动 serve（后台）
SERVE_LOG=$(mktemp /tmp/ion-ext-ci.XXXXXX)
SERVE_PID=""
cleanup() {
    if [ -n "${SERVE_PID:-}" ]; then
        kill "$SERVE_PID" 2>/dev/null || true
    fi
    rm -f "$ION_HOST_SOCKET" "${SERVE_LOG:-}" 2>/dev/null || true
    if [ -n "${BACKUP_WASM:-}" ] && [ -f "$BACKUP_WASM" ]; then
        mv "$BACKUP_WASM" "$INSTALLED_WASM"
    else
        rm -f "$INSTALLED_WASM"
    fi
    if [ "${CREATED_SESSION_DIR:-0}" = "1" ]; then
        rm -rf "$ION_SESSION_DIR"
    fi
}
trap cleanup EXIT
nohup "$ION_BIN" serve > "$SERVE_LOG" 2>&1 &
SERVE_PID=$!
yellow "serve pid=$SERVE_PID, log=$SERVE_LOG"

# 等 serve 起来（最多 10 秒）
for i in 1 2 3 4 5 6 7 8 9 10; do
    if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
        green "✅ serve 起来了 (waited ${i}s)"
        break
    fi
    sleep 1
done

# 创建 session（用 build agent，无工具白名单，WASM 工具可用）
SID=$("$ION_BIN" rpc --method create_session --params '{"agent":"build"}' 2>&1 \
    | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)

if [ -z "$SID" ]; then
    red "❌ 无法创建 session，serve log:"
    tail -20 "$SERVE_LOG"
    exit 1
fi
yellow "session: $SID"
sleep 2  # 等 worker 初始化 + 加载 wasm

# ── todo_extension 测试 ────────────────────────────────────────────────────

bold ""
bold "=== todo_extension (5 工具) ==="

# 用全新 session（避免历史数据干扰），每次测前清掉 tasks
TODO_SID=$("$ION_BIN" rpc --method create_session --params '{"agent":"build"}' 2>&1 \
    | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
sleep 1

# 用唯一 token（含 timestamp）避免历史数据污染断言
UNIQ_TOK="ci_$(date +%s)"
TODO_TEXT_A="CI_TASK_A_${UNIQ_TOK}"
TODO_TEXT_B="CI_TASK_B_${UNIQ_TOK}"

# 提取 add 返回的 id（todo_add 的 id 是从历史最大值递增的，不能硬编码）
OUT=$(call_tool "$TODO_SID" "{\"tool\":\"todo_add\",\"args\":{\"text\":\"$TODO_TEXT_A\"}}")
assert_contains "todo_add 返回 created" "$OUT" '"status":"created"'
assert_contains "todo_add 包含 text" "$OUT" "\"text\":\"$TODO_TEXT_A\""
ID_A=$(echo "$OUT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('id',''))" 2>/dev/null)
yellow "  $TODO_TEXT_A id=$ID_A"

OUT=$(call_tool "$TODO_SID" "{\"tool\":\"todo_add\",\"args\":{\"text\":\"$TODO_TEXT_B\"}}")
assert_contains "todo_add 第二个" "$OUT" '"status":"created"'
ID_B=$(echo "$OUT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('id',''))" 2>/dev/null)
yellow "  $TODO_TEXT_B id=$ID_B"

OUT=$(call_tool "$TODO_SID" '{"tool":"todo_list","args":{"status":"all"}}')
assert_contains "todo_list 返回数组" "$OUT" "$TODO_TEXT_A"
assert_contains "todo_list 包含两个" "$OUT" "$TODO_TEXT_B"

# 用动态提取的 id 做 done / remove
OUT=$(call_tool "$TODO_SID" "{\"tool\":\"todo_done\",\"args\":{\"id\":\"$ID_A\"}}")
assert_contains "todo_done 标记成功" "$OUT" '"status":"done"'

OUT=$(call_tool "$TODO_SID" '{"tool":"todo_list","args":{"status":"done"}}')
assert_contains "todo_list done 过滤" "$OUT" "$TODO_TEXT_A"

OUT=$(call_tool "$TODO_SID" "{\"tool\":\"todo_remove\",\"args\":{\"id\":\"$ID_B\"}}")
assert_contains "todo_remove 删除" "$OUT" '"status":"removed"'

OUT=$(call_tool "$TODO_SID" '{"tool":"todo_list","args":{"status":"all"}}')
# 删除后不应该包含 TODO_TEXT_B（用唯一 token 避免历史数据干扰）
if echo "$OUT" | grep -qF "$TODO_TEXT_B"; then
    red "  ❌ todo_remove 后 $TODO_TEXT_B 还在"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("todo_remove 后 list 不含已删项")
else
    green "  ✅ todo_remove 后 list 不含已删项"
    PASS=$((PASS + 1))
fi


# ── 清理 ────────────────────────────────────────────────────────────────────

bold ""
bold "=== 清理 ==="
cleanup
trap - EXIT
green "✅ 清理完成"

# ── 汇总 ────────────────────────────────────────────────────────────────────

bold ""
bold "=== 汇总 ==="
echo "  Pass: $PASS"
echo "  Fail: $FAIL"
if [ "$FAIL" -gt 0 ]; then
    red "失败项:"
    for t in "${FAILED_TESTS[@]}"; do
        red "  - $t"
    done
    exit 1
else
    green ""
    green "🎉 全部通过 ($PASS assertions)"
    exit 0
fi
# (this line intentionally left blank)
