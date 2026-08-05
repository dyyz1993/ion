#!/usr/bin/env bash
# workflow_cross_session.sh — X-2: 跨 session 学经验（memory 持久化）
#
# 流程：
#   session A: save("我喜欢 Rust", tags=[language, rust])
#   session B: 启动后 search "language preference"
#   验证：B 能找到 A 写的（全局库真持久化）
#
# 用法：bash scripts/workflow_cross_session.sh

set -o pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="${ION_BIN:-$(which ion)}"
ION_MODEL="${ION_MODEL:-glm-5.2}"
ION_PROVIDER="${ION_PROVIDER:-zai}"
CHROME="${CHROME:-/Applications/Chromium.app/Contents/MacOS/Chromium}"
REPORT_DIR="/tmp/ext_workflow_x2"
mkdir -p "$REPORT_DIR"

green() { printf "\033[32m%s\033[0m\n" "$*"; }
red()   { printf "\033[31m%s\033[0m\n" "$*"; }
blue()  { printf "\033[34m%s\033[0m\n" "$*"; }

green "════════════════════════════════════════════════════════════"
green "  X-2: 跨 session 学经验（memory 持久化）"
green "════════════════════════════════════════════════════════════"

# ── Session A: save ──
blue "▶ Step 1: Session A 保存经验"
WORK_A="/tmp/x2_session_a"
rm -rf "$WORK_A"; mkdir -p "$WORK_A"
PROMPT_A="$REPORT_DIR/prompt_a.txt"
python3 -c "
with open('$PROMPT_A','w',encoding='utf-8') as f:
    f.write('请记住：用户喜欢的编程语言是 Rust。用 memory_save 工具保存，tags 加 language 和 rust。')
"
START=$(date +%s)
(
    cd "$WORK_A"
    timeout 120 "$ION_BIN" --agent developer --model "$ION_MODEL" --provider "$ION_PROVIDER" \
        "@$PROMPT_A" 2>&1 | tail -3
)
RC=$?
ELAPSED_A=$(( $(date +%s) - START ))
green "  ✓ Session A 完成 (${ELAPSED_A}s, rc=$RC)"

# 找 session A 的 ID + ToolResult
SESS_A_DIR=$(ls -dt ~/.ion/agent/sessions/* 2>/dev/null | head -1)
SESS_A_JSONL=$(ls "$SESS_A_DIR"/sess_*.jsonl 2>/dev/null | head -1)
SESS_A_ID=$(basename "$SESS_A_JSONL" .jsonl)
green "  Session A: $SESS_A_ID"

# 提取 save 返回的 gmem ID
GMEM_ID=$(python3 << EOF
import json
with open("$SESS_A_JSONL") as f:
    for line in f:
        line = line.strip()
        if not line: continue
        e = json.loads(line)
        if e.get('type') != 'message': continue
        m = e.get('message', {})
        if 'ToolResult' in m and m['ToolResult'].get('tool_name') == 'memory_save':
            for c in m['ToolResult'].get('content', []):
                if 'Text' in c:
                    import re
                    mm = re.search(r'"id":"(gmem_[a-f0-9-]{36})"', c['Text'].get('text',''))
                    if mm:
                        print(mm.group(1))
                        exit()
EOF
)
if [ -z "$GMEM_ID" ]; then
    red "  ✗ Session A 没保存 memory（没拿到 gmem_id）"
    exit 1
fi
green "  ✓ Session A 写入 memory: $GMEM_ID"

# ── Session B: search ──
blue "▶ Step 2: Session B（新 cwd）检索经验"
WORK_B="/tmp/x2_session_b"
rm -rf "$WORK_B"; mkdir -p "$WORK_B"
PROMPT_B="$REPORT_DIR/prompt_b.txt"
python3 -c "
with open('$PROMPT_B','w',encoding='utf-8') as f:
    f.write('搜索记忆里关于 language 或 rust 的内容。用 memory_search 工具。')
"
START=$(date +%s)
(
    cd "$WORK_B"
    timeout 120 "$ION_BIN" --agent developer --model "$ION_MODEL" --provider "$ION_PROVIDER" \
        "@$PROMPT_B" 2>&1 | tail -3
)
ELAPSED_B=$(( $(date +%s) - START ))
green "  ✓ Session B 完成 (${ELAPSED_B}s)"

# 找 session B 的 jsonl
SESS_B_DIR=$(ls -dt ~/.ion/agent/sessions/* 2>/dev/null | head -1)
SESS_B_JSONL=$(ls "$SESS_B_DIR"/sess_*.jsonl 2>/dev/null | head -1)

# 验证 B 找到了 A 的 memory
PERSIST=$(python3 << EOF
import json
found = False
with open("$SESS_B_JSONL") as f:
    for line in f:
        line = line.strip()
        if not line: continue
        e = json.loads(line)
        if e.get('type') != 'message': continue
        m = e.get('message', {})
        if 'ToolResult' in m and m['ToolResult'].get('tool_name') == 'memory_search':
            for c in m['ToolResult'].get('content', []):
                if 'Text' in c and '$GMEM_ID' in c['Text'].get('text',''):
                    found = True
                    break
print('PASS' if found else 'FAIL')
EOF
)

if [ "$PERSIST" = "PASS" ]; then
    green "  ✅ X-2 PASS: Session B 在新 cwd 找到了 Session A 写的 memory"
    green "     这证明全局库真的持久化（不只是 session-local）"
    EXIT=0
else
    red "  ❌ X-2 FAIL: Session B 没找到 Session A 的 memory ($GMEM_ID)"
    red "     全局库持久化有问题（02-M5 失败的真实根因）"
    EXIT=1
fi

# 导出两个 session 的 HTML
for sid_dir in "$SESS_A_DIR" "$SESS_B_DIR"; do
    sid=$(basename $(ls "$sid_dir"/sess_*.jsonl 2>/dev/null | head -1) .jsonl)
    (cd "$sid_dir" && "$ION_BIN" --export "$sid" 2>/dev/null)
    mv "$sid_dir/$sid" "$REPORT_DIR/$(basename $sid_dir)_${sid}.html" 2>/dev/null
done

green "  HTML: $REPORT_DIR"
echo "$GMEM_ID" > "$REPORT_DIR/gmem_id.txt"
exit $EXIT
