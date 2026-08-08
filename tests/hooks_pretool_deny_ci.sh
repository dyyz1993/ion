#!/usr/bin/env bash
# PreToolUse deny CLI verification.
#
# Proves the externally observable session/export contract with FauxProvider:
# User -> Assistant(tool call) -> Hook audit -> ToolResult(error) -> Assistant.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0
FAIL=0

pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-hooks-pretool-deny-XXXXXX)"
TEST_HOME="$TEST_ROOT/home"
TEST_PROJECT="$TEST_ROOT/project"
SESSION_ID="sess_hooks_pretool_deny_ci"
HTML="$TEST_ROOT/pretool-deny.html"
MARKER="$TEST_ROOT/tool-ran.txt"
mkdir -p "$TEST_HOME" "$TEST_PROJECT/.ion"

cleanup() {
    if [ "${KEEP_TEST_ROOT:-0}" = "1" ]; then
        printf '  debug artifacts: %s\n' "$TEST_ROOT"
    else
        rm -rf "$TEST_ROOT"
    fi
}
trap cleanup EXIT

cat > "$TEST_PROJECT/.ion/deny.sh" <<'HOOK'
#!/usr/bin/env bash
printf '%s\n' '{"decision":"block","reason":"CLI Hook 拒绝执行 Bash"}'
exit 2
HOOK
chmod +x "$TEST_PROJECT/.ion/deny.sh"

cat > "$TEST_PROJECT/.ion/hooks.json" <<'JSON'
{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {"type": "command", "command": "bash .ion/deny.sh", "timeout": 5}
        ]
      }
    ]
  }
}
JSON

cat > "$TEST_ROOT/faux.jsonl" <<JSONL
{"tool_call":{"name":"bash","input":{"command":"touch $MARKER"}}}
{"text":"Bash 被 Hook 拒绝，因此没有执行。"}
JSONL

printf '%s\n' '════════════════════════════════════════════════════'
printf '%s\n' '  PreToolUse deny CLI'
printf '%s\n' '════════════════════════════════════════════════════'

if ! cargo build --bin ion >/dev/null 2>&1; then
    fail "ion 构建成功"
    exit 1
fi
pass "ion 构建成功"

(
    cd "$TEST_PROJECT" || exit 1
    HOME="$TEST_HOME" \
    ION_FAUX_SCRIPT="$TEST_ROOT/faux.jsonl" \
    ION_GRACEFUL_DRAIN_MS=0 \
    "$ION_BIN" \
        --no-context-files \
        --provider faux \
        --model faux-test \
        --session-id "$SESSION_ID" \
        --export "$HTML" \
        "请尝试执行 Bash；如果被拒绝，请说明没有执行。"
) >"$TEST_ROOT/run.log" 2>&1

if [ ! -e "$MARKER" ]; then
    pass "被拒绝的 Bash 没有执行"
else
    fail "被拒绝的 Bash 没有执行"
fi

SESSION_FILE="$(find "$TEST_HOME/.ion/agent/sessions" -name "$SESSION_ID.jsonl" -print -quit 2>/dev/null)"
if [ -n "$SESSION_FILE" ] && [ -s "$SESSION_FILE" ]; then
    pass "session.jsonl 已生成"
else
    fail "session.jsonl 已生成"
fi

if [ -n "$SESSION_FILE" ] && jq -s -e '
  [ .[]
    | select(.type == "message")
    | {entry: ., variant: (.message | to_entries[0])}
    | select(.variant.key == "User" or .variant.key == "Assistant" or .variant.key == "ToolResult")
  ] as $flow
  | ($flow | map(.variant.key)) == ["User", "Assistant", "ToolResult", "Assistant"]
  and ($flow[1].variant.value.content | any(has("ToolCall")))
  and ($flow[2].variant.value.is_error == true)
  and ($flow[2].variant.value.content[0].Text.text | contains("PreToolUse Hook"))
  and ($flow[2].variant.value.content[0].Text.text | contains("CLI Hook 拒绝执行 Bash"))
' "$SESSION_FILE" >/dev/null; then
    pass "消息顺序完整且 ToolResult 是拒绝错误"
else
    fail "消息顺序完整且 ToolResult 是拒绝错误"
fi

if [ -n "$SESSION_FILE" ] && jq -s -e '
  [ .[]
    | select(.type == "message")
    | {entry: ., variant: (.message | to_entries[0])}
    | select(.variant.key == "User" or .variant.key == "Assistant" or .variant.key == "ToolResult")
  ] as $flow
  | ([.[] | select(.customType == "hook_event")][0]) as $hook
  | ($flow[1].variant.value.content | map(select(has("ToolCall")) | .ToolCall.id)[0]) as $call_id
  | ($hook.details.source == "hook")
  and ($hook.details.hookEvent == "PreToolUse")
  and ($hook.details.decision == "block")
  and ($hook.details.toolCallId == $call_id)
  and ($flow[2].variant.value.tool_call_id == $call_id)
  and ($hook.parentId == $flow[1].entry.id)
  and ($flow[2].entry.parentId == $hook.id)
' "$SESSION_FILE" >/dev/null; then
    pass "Hook 审计记录与 ToolResult 使用同一 toolCallId 且位于当前分支"
else
    fail "Hook 审计记录与 ToolResult 使用同一 toolCallId 且位于当前分支"
fi

INDEX_FILE="$TEST_HOME/.ion/agent/sessions.index.json"
if jq -e --arg sid "$SESSION_ID" '
  .sessions[$sid].message_count == 4
  and .sessions[$sid].user_prompt_count == 1
  and .sessions[$sid].llm_request_count == 2
  and .sessions[$sid].turn_count == 2
' "$INDEX_FILE" >/dev/null; then
    pass "SessionIndex 使用完整会话的准确计数"
else
    fail "SessionIndex 使用完整会话的准确计数"
fi

if [ -s "$HTML" ] && grep -q 'id="session-data"' "$HTML"; then
    pass "单文件 HTML 导出成功"
else
    fail "单文件 HTML 导出成功"
fi

printf '\nResult: %s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
