#!/usr/bin/env bash
# hooks_stdin_ci.sh — 验证 hook stdin JSON 字段完整性（对齐 Claude Code）
#
# 用 command handler echo stdin，捕获 JSON，断言所有字段存在 + 值正确。
# 覆盖所有事件 + 所有关键字段。
#
# Usage: bash tests/hooks_stdin_ci.sh
set -uo pipefail

ION_BIN="${ION_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/ion}"
HOST_SOCK="$HOME/.ion/host.sock"
PASS=0; FAIL=0

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  ✅ $label"
        PASS=$((PASS+1))
    else
        echo "  ❌ $label (expected '$needle')"
        FAIL=$((FAIL+1))
    fi
}

cleanup() {
    local pid=$(lsof -ti "$HOST_SOCK" 2>/dev/null || true)
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
}
trap cleanup EXIT

# 准备测试项目
TMP=$(mktemp -d /tmp/ion-hooks-stdin-XXXXXX)
mkdir -p "$TMP/proj/src"
echo "fn main() {}" > "$TMP/proj/src/main.rs"

# hook 配置：command handler 把 stdin 写到文件（让我们能检查 JSON）
HOOKS_JSON="$TMP/proj/.ion/hooks.json"
mkdir -p "$TMP/proj/.ion"

write_hooks() {
    local event="$1" outfile="$2"
    cat > "$HOOKS_JSON" << EOF
{
  "hooks": {
    "$event": [
      {"type": "command", "command": "cat > $outfile"}
    ]
  }
}
EOF
}

echo "=== hooks_stdin_ci: 验证 stdin JSON 字段（对齐 Claude Code）==="

cd "$TMP/proj"

# ── Group A: 通用字段（所有事件都有）──
echo ""
echo "--- Group A: 通用字段 ---"

# SessionStart
write_hooks "SessionStart" "$TMP/stdin_sessionstart.json"
OUT_FILE="$TMP/faux_ss.html"
ION_FAUX_REPLY='{"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"Stop"}' \
    "$ION_BIN" --no-context-files --provider faux --model faux-test \
    --export "$OUT_FILE" "test" >/dev/null 2>&1 </dev/null

if [ -f "$TMP/stdin_sessionstart.json" ]; then
    SS=$(cat "$TMP/stdin_sessionstart.json")
    assert_contains "A1: session_id 存在" "$SS" "session_id"
    assert_contains "A2: cwd 存在" "$SS" "cwd"
    assert_contains "A3: transcript_path 存在" "$SS" "transcript_path"
    assert_contains "A4: hook_event_name=SessionStart" "$SS" '"hook_event_name": "SessionStart"'
    assert_contains "A5: workspace_roots 存在" "$SS" "workspace_roots"
    assert_contains "A6: source 字段存在" "$SS" "source"
else
    echo "  ⚠️ SessionStart stdin 未捕获"
    FAIL=$((FAIL+6))
fi

# ── Group B: PreToolUse 字段 ──
echo ""
echo "--- Group B: PreToolUse 字段 ---"

write_hooks "PreToolUse" "$TMP/stdin_pretl.json"
ION_FAUX_REPLY='{"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"Stop"}' \
    "$ION_BIN" --no-context-files --provider faux --model faux-test \
    --export "$TMP/faux_pre.html" "test" >/dev/null 2>&1 </dev/null

if [ -f "$TMP/stdin_pretl.json" ]; then
    # 可能有多行（多个 PreToolUse），取第一行
    PRE=$(head -1 "$TMP/stdin_pretl.json")
    assert_contains "B1: tool_name 存在" "$PRE" "tool_name"
    assert_contains "B2: tool_input 存在" "$PRE" "tool_input"
    assert_contains "B3: tool_use_id 存在" "$PRE" "tool_use_id"
    assert_contains "B4: hook_event_name=PreToolUse" "$PRE" '"hook_event_name": "PreToolUse"'
else
    echo "  ⚠️ PreToolUse stdin 未捕获（faux 可能没调工具）"
    FAIL=$((FAIL+4))
fi

# ── Group C: Stop 字段 ──
echo ""
echo "--- Group C: Stop 字段 ---"

write_hooks "Stop" "$TMP/stdin_stop.json"
ION_FAUX_REPLY='{"role":"assistant","content":[{"type":"text","text":"task done"}],"stop_reason":"Stop"}' \
    "$ION_BIN" --no-context-files --provider faux --model faux-test \
    --export "$TMP/faux_stop.html" "test" >/dev/null 2>&1 </dev/null

if [ -f "$TMP/stdin_stop.json" ]; then
    STOP=$(cat "$TMP/stdin_stop.json")
    assert_contains "C1: hook_event_name=Stop" "$STOP" '"hook_event_name": "Stop"'
    assert_contains "C2: last_assistant_message 存在" "$STOP" "last_assistant_message"
    assert_contains "C3: stop_hook_active 存在" "$STOP" "stop_hook_active"
else
    echo "  ⚠️ Stop stdin 未捕获"
    FAIL=$((FAIL+3))
fi

# ── Group D: UserPromptSubmit 字段 ──
echo ""
echo "--- Group D: UserPromptSubmit 字段 ---"

write_hooks "UserPromptSubmit" "$TMP/stdin_ups.json"
ION_FAUX_REPLY='{"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"Stop"}' \
    "$ION_BIN" --no-context-files --provider faux --model faux-test \
    --export "$TMP/faux_ups.html" "test prompt here" >/dev/null 2>&1 </dev/null

if [ -f "$TMP/stdin_ups.json" ]; then
    UPS=$(cat "$TMP/stdin_ups.json")
    assert_contains "D1: hook_event_name=UserPromptSubmit" "$UPS" '"hook_event_name": "UserPromptSubmit"'
    assert_contains "D2: prompt 字段存在" "$UPS" "prompt"
    assert_contains "D3: prompt 含用户输入" "$UPS" "test prompt here"
else
    echo "  ⚠️ UserPromptSubmit stdin 未捕获"
    FAIL=$((FAIL+3))
fi

# ── Group E: JSON 合法性 ──
echo ""
echo "--- Group E: JSON 合法性 ---"

if [ -f "$TMP/stdin_sessionstart.json" ]; then
    SS=$(cat "$TMP/stdin_sessionstart.json")
    IS_JSON=$(echo "$SS" | python3 -c "import json,sys; json.loads(sys.stdin.read()); print('valid')" 2>/dev/null || echo "invalid")
    assert_contains "E1: SessionStart stdin 是合法 JSON" "$IS_JSON" "valid"
fi

if [ -f "$TMP/stdin_stop.json" ]; then
    STOP=$(cat "$TMP/stdin_stop.json")
    IS_JSON=$(echo "$STOP" | python3 -c "import json,sys; json.loads(sys.stdin.read()); print('valid')" 2>/dev/null || echo "invalid")
    assert_contains "E2: Stop stdin 是合法 JSON" "$IS_JSON" "valid"
fi

echo ""
echo "==============================================="
echo "hooks_stdin_ci: $PASS passed, $FAIL failed"
echo "==============================================="

rm -rf "$TMP"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
