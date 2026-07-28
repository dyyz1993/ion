#!/usr/bin/env bash
# tests/extensions_ci.sh — WASM extensions (todo-extension + plan-extension) CLI 验收
#
# 验证两个 extension 的 10 个工具端到端可用：
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
#   - 已 cargo build --target wasm32-wasip1 --release -p todo-extension -p plan-extension
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
    trap 'rm -rf "$ION_SESSION_DIR"' EXIT
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

# Build wasm 产物（如果缺失）
for ext in todo_extension plan_extension; do
    WASM="target/wasm32-wasip1/release/${ext}.wasm"
    if [ ! -f "$WASM" ]; then
        yellow "building $ext.wasm..."
        cargo build --target wasm32-wasip1 --release -p "${ext%_extension}-extension" 2>&1 | tail -2
    fi
done

# 安装到全局 extensions 目录（ION worker 只扫这里）
EXT_DIR="$HOME/.ion/agent/extensions"
mkdir -p "$EXT_DIR"
cp target/wasm32-wasip1/release/todo_extension.wasm "$EXT_DIR/"
cp target/wasm32-wasip1/release/plan_extension.wasm "$EXT_DIR/"
green "✅ wasm 安装到 $EXT_DIR"

# 杀残留 serve + 清 host.sock
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
sleep 1
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid" 2>/dev/null

# 启动 serve（后台）
SERVE_LOG=$(mktemp /tmp/ion-ext-ci.XXXXXX)
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

# ── plan_extension 测试 ────────────────────────────────────────────────────

bold ""
bold "=== plan_extension (5 工具) ==="

PLAN_SID=$("$ION_BIN" rpc --method create_session --params '{"agent":"build"}' 2>&1 \
    | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['data']['session_id'])" 2>/dev/null)
sleep 1

# plan_path 必须在项目根目录（allowed_roots 内 + 不在 .ion/ 被 protect-ion-config 拦）
PLAN_FILE="$PROJECT_DIR/ci-test-plan.txt"
rm -f "$PLAN_FILE"

PLAN_STEP_A="CI_PLAN_A_${UNIQ_TOK}"
PLAN_STEP_B="CI_PLAN_B_${UNIQ_TOK}"

OUT=$(call_tool "$PLAN_SID" "{\"tool\":\"plan_enter\",\"args\":{\"plan_path\":\"$PLAN_FILE\"}}")
assert_contains "plan_enter 进入计划模式" "$OUT" '"status":"ok"'
assert_contains "plan_enter 返回 mode" "$OUT" '"mode":"plan"'

OUT=$(call_tool "$PLAN_SID" "{\"tool\":\"plan_add\",\"args\":{\"step\":\"$PLAN_STEP_A\"}}")
assert_contains "plan_add 第一步" "$OUT" '"status":"added"'

OUT=$(call_tool "$PLAN_SID" "{\"tool\":\"plan_add\",\"args\":{\"step\":\"$PLAN_STEP_B\"}}")
assert_contains "plan_add 第二步" "$OUT" '"status":"added"'

OUT=$(call_tool "$PLAN_SID" '{"tool":"plan_list","args":{}}')
assert_contains "plan_list 返回步骤" "$OUT" "$PLAN_STEP_A"
assert_contains "plan_list count=2" "$OUT" '"count":2'

OUT=$(call_tool "$PLAN_SID" '{"tool":"plan_done","args":{"index":0}}')
assert_contains "plan_done 标记" "$OUT" '"status":"done"'

OUT=$(call_tool "$PLAN_SID" '{"tool":"plan_list","args":{}}')
assert_contains "plan_list 标记后含 [x]" "$OUT" "[x] $PLAN_STEP_A"

OUT=$(call_tool "$PLAN_SID" '{"tool":"plan_exit","args":{}}')
assert_contains "plan_exit 退出" "$OUT" '"mode":"normal"'

# 文件落盘验证
if [ -f "$PLAN_FILE" ]; then
    if grep -qF "[x] $PLAN_STEP_A" "$PLAN_FILE" && grep -qF "$PLAN_STEP_B" "$PLAN_FILE"; then
        green "  ✅ 磁盘文件内容正确"
        PASS=$((PASS + 1))
    else
        red "  ❌ 磁盘文件内容错误: $(cat "$PLAN_FILE")"
        FAIL=$((FAIL + 1))
        FAILED_TESTS+=("磁盘文件内容")
    fi
else
    red "  ❌ 磁盘文件未创建: $PLAN_FILE"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("磁盘文件创建")
fi

# ── 清理 ────────────────────────────────────────────────────────────────────

bold ""
bold "=== 清理 ==="
kill "$SERVE_PID" 2>/dev/null
rm -f "$PLAN_FILE" "$SERVE_LOG"
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
