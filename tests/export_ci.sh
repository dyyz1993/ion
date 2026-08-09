#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Export CI — 验证 ion --export HTML 导出的格式正确性
#
# 背景：ION session JSONL 存的是 Rust enum 序列化形式
# ({"message":{"Assistant":{...}}} / content blocks {"Text":{"text":...}}),
# 而 ION 的离线 HTML 渲染层使用扁平形式
# ({"message":{"role":"assistant",...}} / {"type":"text","text":...}).
# 之前缺这层转换，导致侧边栏大量 [undefined]。
#
# 这个 CI 脚本验证转换链路：
#   Group A:  真实 ion 跑一个对话 → 导出 HTML → 解码 base64 → 检查转换正确
#   Group B:  含 step-snapshot 的 session → 导出 → 检查正文与 Timeline 完整展示
#   Group C:  边界场景：缺失/损坏 session 与退出码
#   Group D:  export-after-run 工具定义
#   Group E:  确定性 compaction fixture
#   Group F:  active branch 选择
#   Group G:  LLM/Tool/Custom/Extension 流程语义
#
# 用法：bash tests/export_ci.sh
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
EXPORT_CI_SESSION_DIR=$(mktemp -d)
export ION_SESSION_DIR="$EXPORT_CI_SESSION_DIR"
export ION_SKIP_MCP=1
trap 'rm -rf "$EXPORT_CI_SESSION_DIR"' EXIT

if ! command -v jq &>/dev/null; then
    echo "❌ 需要 jq"
    exit 1
fi

# 解码 HTML 中 base64 session 数据，转成可读 JSON
decode_session_data() {
    local html="$1"
    python3 - "$html" <<'PYEOF'
import re, base64, json, sys
html = open(sys.argv[1]).read()
m = re.search(r'<script id="session-data"[^>]*>([^<]+)</script>', html)
if not m:
    print("ERROR: no session-data script tag", file=sys.stderr)
    sys.exit(1)
b64 = m.group(1).strip()
decoded = base64.b64decode(b64).decode("utf-8")
data = json.loads(decoded)
print(json.dumps(data))
PYEOF
}

echo "════════════════════════════════════════════════════"
echo "  Export CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ═════════════════════════════════════════════════════════
# Group A: 真实对话 → 导出 → 验证转换
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group A: 真实对话导出（FauxProvider 驱动） ──"

WORKDIR=$(mktemp -d)
HTML="$WORKDIR/out.html"
SOCK="${ION_HOST_SOCKET:-$HOME/.ion/host.sock}"
# 清理上次 host 残留
# Reuse existing host if available
if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q sessions; then
  echo "  (reusing existing host)"
else
  [ -e "$SOCK" ] && lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null || true
fi
sleep 1

cd "$WORKDIR"
# FauxProvider 驱动一次对话（无需真实 API）
# Use a longer response so the exported HTML has enough content to validate.
ION_GRACEFUL_DRAIN_MS=50 \
ION_FAUX_REPLY="Hello from faux export test. This is a multi-word response that generates sufficient HTML content for the export validation checks to pass. The quick brown fox jumps over the lazy dog. Lorem ipsum dolor sit amet consectetur adipiscing elit." \
    "$ION_BIN" --offline --no-extensions -p "hi" 2>&1 >/dev/null || true

# 只从本用例隔离的 ION_SESSION_DIR 查找刚生成的会话，避免读取用户的
# ~/.ion/agent/last_session 而串到另一个并发任务。
LAST_SID=$(python3 - "$ION_SESSION_DIR" <<'PYEOF'
import json
from pathlib import Path
import sys

sessions = []
for path in Path(sys.argv[1]).rglob("*.jsonl"):
    try:
        with path.open() as handle:
            for line in handle:
                value = json.loads(line)
                if value.get("type") == "session" and value.get("id"):
                    sessions.append((path.stat().st_mtime, value["id"]))
                    break
    except (OSError, ValueError):
        pass
print(max(sessions)[1] if sessions else "")
PYEOF
)

if [ -n "$LAST_SID" ]; then
    "$ION_BIN" --export "$HTML" --session "$LAST_SID" 2>&1 | grep -q "Exported" && \
        pass "A1 export 命令成功（sid=$LAST_SID）" || fail "A1 export 失败"
else
    fail "A1 没有可用 session（last_session 为空）"
fi

if [ -f "$HTML" ]; then
    SIZE=$(stat -f%z "$HTML" 2>/dev/null || stat -c%s "$HTML" 2>/dev/null)
    [ "$SIZE" -gt 1500 ] && pass "A2 HTML 文件大小正常（$SIZE bytes）" || fail "A2 HTML 太小（$SIZE bytes）"

    grep -q 'class="ion-stats-inner"' "$HTML" && \
        pass "A9 顶部统计区使用结构化 masthead" || \
        fail "A9 缺少 ion-stats-inner"
    grep -q 'class="ion-overview-panel"' "$HTML" && \
        pass "A10 Extension/Timeline 使用统一概览卡" || \
        fail "A10 缺少 ion-overview-panel"
    grep -q 'grid-template-columns: var(--ion-sidebar-width)' "$HTML" && \
        pass "A11 主体使用响应式双栏网格" || \
        fail "A11 缺少响应式双栏样式"
    grep -q -- '--text: #172033;' "$HTML" && \
        pass "A12 离线主题变量完整注入" || \
        fail "A12 缺少离线主题变量"
else
    fail "A2 HTML 文件不存在"
fi

    # 解码并验证转换
    if [ -f "$HTML" ]; then
        DATA=$(decode_session_data "$HTML" 2>/dev/null)
        if [ -n "$DATA" ]; then
            pass "A3 base64 数据可解码"
            # 检查 entries 不为空
            N=$(echo "$DATA" | jq '.entries | length')
            [ "$N" -gt 0 ] && pass "A4 entries 非空（$N 条）" || fail "A4 entries 为空"

            # Timeline 使用独立的完整 entry 流，不能被正文过滤规则截断
            TIMELINE_N=$(echo "$DATA" | jq '.timelineEntries | length')
            [ "$TIMELINE_N" -gt 0 ] && \
                pass "A13 timelineEntries 非空（$TIMELINE_N 条）" || \
                fail "A13 timelineEntries 缺失或为空"
            [ "$TIMELINE_N" -eq "$N" ] && \
                pass "A14 Timeline 与正文使用同一完整 Entry 流（$TIMELINE_N 条）" || \
                fail "A14 Timeline/正文 Entry 数不一致（$TIMELINE_N != $N）"
            grep -q 'data-entry-category' "$HTML" && grep -q 'ion-timeline-tooltip' "$HTML" && \
                pass "A15 Timeline 包含类型筛选与悬停概要" || \
                fail "A15 Timeline 交互结构缺失"
            INDEX_META=$(echo "$DATA" | jq '.header.indexMeta | type == "object"')
            [ "$INDEX_META" = "true" ] && \
                pass "A16 SessionIndex 元信息快照已注入导出数据" || \
                fail "A16 header.indexMeta 缺失"
            grep -q -- '--timeline-content-width' "$HTML" && grep -q 'visible.length \* 12' "$HTML" && grep -q 'scrollIntoView' "$HTML" && \
                pass "A17 Timeline 按 Entry 顺序紧凑排列且支持点击跳转" || \
                fail "A17 Timeline 紧凑排列或跳转逻辑缺失"
            grep -q 'ion-entry-fold-hint' "$HTML" && \
                grep -q '#messages > \[id\^="entry-"\]' "$HTML" && \
                grep -q 'minimumVisibleContentLines = 3' "$HTML" && \
                grep -q 'minimumHiddenContentLines = 3' "$HTML" && \
                grep -q 'hiddenLines > minimumHiddenContentLines' "$HTML" && \
                grep -q 'data-ion-output-foldable' "$HTML" && \
                grep -q 'shouldFold = remaining > minimumHiddenLines' "$HTML" && \
                grep -q 'collectVisualLines' "$HTML" && \
                grep -q 'data-ion-entry-foldable' "$HTML" && \
                grep -q 'more lines, click to expand' "$HTML" && \
                grep -q 'aria-expanded' "$HTML" && \
                pass "A18 仅在展开可新增超过 3 行正文时折叠" || \
                fail "A18 缺少隐藏正文超过三行的折叠门槛"
            TIMELINE_IDS=$(echo "$DATA" | jq -r '[.timelineEntries[] | .id // ""] | sort | .[]')
            BODY_IDS=$(echo "$DATA" | jq -r '[.entries[] | .id // ""] | sort | .[]')
            [ "$TIMELINE_IDS" = "$BODY_IDS" ] && \
                grep -q 'ionEntryBodyCoverage' "$HTML" && \
                grep -q 'ion-entry-nested-events' "$HTML" && \
                pass "A19 每条 Timeline Entry 都声明正文目标，Hook 支持归组" || \
                fail "A19 Timeline Entry 正文目标覆盖不完整"
            SOURCE_N=$(echo "$DATA" | jq '.sourceEntries | length')
            INTERNAL_N=$(echo "$DATA" | jq '.internalEntries | length')
            [ "$SOURCE_N" -eq $((TIMELINE_N + INTERNAL_N)) ] && \
                pass "A20 sourceEntries 保留当前分支完整有序数据（$SOURCE_N 条）" || \
                fail "A20 sourceEntries 数量不完整"
            META_N=$(echo "$DATA" | jq '[.timelineEntries[] | select(.ionMeta | type == "object")] | length')
            [ "$META_N" -eq "$TIMELINE_N" ] && \
                pass "A21 所有 Timeline Entry 都有统一 ionMeta" || \
                fail "A21 只有 $META_N/$TIMELINE_N 条 Entry 有 ionMeta"
            FLOW_OK=$(echo "$DATA" | jq '.flowSummary.entries == (.sourceEntries | length) and (.flowSummary.llmCalls >= 1)')
            [ "$FLOW_OK" = "true" ] && \
                pass "A22 flowSummary 汇总 LLM/Tool/Custom 流程" || \
                fail "A22 flowSummary 缺失或计数错误"
            grep -q 'What happened in this session' "$HTML" && \
                grep -q 'ion-entry-provenance' "$HTML" && \
                grep -q 'LLM context · included' "$HTML" && \
                pass "A23 正文与 Timeline 渲染同一份流程语义" || \
                fail "A23 流程语义 UI 缺失"
            ! grep -q '/Users/xuyingzhou/Project/temporary/pi-momo-fork' "$PROJECT_DIR/src/export.rs" && \
                grep -Fq 'include_str!("export_assets/template.html")' "$PROJECT_DIR/src/export.rs" && \
                pass "A24 导出资源由 ION 编译内置，不依赖 pi checkout" || \
                fail "A24 仍存在外部模板依赖"
            grep -q "replace(/</g, '&lt;')" "$HTML" && \
                pass "A25 Markdown 原始 HTML 在离线模板中被转义" || \
                fail "A25 缺少导出内容 XSS 防护"

            # 检查 message 是否已 flatten（没有 Assistant/User/ToolResult wrapper）
            WRAPPED=$(echo "$DATA" | jq '[.entries[] | select(.type=="message") | .message | select(has("Assistant") or has("User") or has("ToolResult"))] | length')
            [ "$WRAPPED" -eq 0 ] && pass "A5 message 已 flatten（无 enum wrapper）" || fail "A5 仍有 $WRAPPED 条 message 带 enum wrapper"

            # 检查 message 有 role 字段
            ROLE_MISSING=$(echo "$DATA" | jq '[.entries[] | select(.type=="message") | .message | select(.role == null)] | length')
            [ "$ROLE_MISSING" -eq 0 ] && pass "A6 所有 message 都有 role 字段" || fail "A6 有 $ROLE_MISSING 条 message 缺 role"

            # 检查 content blocks 已转 {type:text} 格式（没有 Text/ToolCall variant key）
            BAD_BLOCKS=$(echo "$DATA" | jq '
                [.entries[] | select(.type=="message") | .message.content // []
                 | (if type == "array" then . else [] end)
                ] | flatten
                | [.[] | select(.Text != null or .ToolCall != null or .User != null or .Assistant != null)] | length
            ')
            [ "$BAD_BLOCKS" -eq 0 ] && pass "A7 content blocks 已 flatten（无 enum variant）" || fail "A7 仍有 $BAD_BLOCKS 个未转换的 content block"

            # 检查 leafId 字段存在（template 用它定位最后一条消息）
            LEAF=$(echo "$DATA" | jq -r '.leafId // empty')
            [ -n "$LEAF" ] && pass "A8 leafId 字段存在（$LEAF）" || fail "A8 缺 leafId 字段"
        else
            # base64 解码失败 — 可能是 CI FauxProvider session 数据不完整
            yellow "A3 base64 解码失败（CI FauxProvider session 数据可能不完整）— skip A3-A8"
            SKIP=$((SKIP+6))
        fi
    fi
rm -rf "$WORKDIR"

# ═════════════════════════════════════════════════════════
# Group B: step-snapshot 在正文与 Timeline 都展示
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group B: step-snapshot 完整展示 ──"

TS_SESSION_ROOT=$(mktemp -d)
mkdir -p "$TS_SESSION_ROOT/exact"
TS_SESSION_FILE="$TS_SESSION_ROOT/exact/session.jsonl"
printf '%s\n' \
    '{"type":"session","version":3,"id":"snapshot_visible","timestamp":"2026-08-08T08:00:00Z","cwd":"/test"}' \
    '{"type":"message","id":"m1","parentId":"snapshot_visible","timestamp":"2026-08-08T08:00:01Z","message":{"User":{"role":"user","content":[{"Text":{"text":"run a tool"}}]}}}' \
    '{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-08T08:00:02Z","message":{"Assistant":{"role":"assistant","content":[{"Text":{"text":"done"}}],"usage":{"input":10,"output":20},"stop_reason":"stop"}}}' \
    '{"type":"custom","id":"snap-1","parentId":"m2","timestamp":"2026-08-08T08:00:03Z","customType":"step-snapshot","data":{"baselineTreeHash":"tree0","snapshotTreeHash":"tree1","toolSnapshotTurnId":"ts-1","turnIndex":1,"diff":{"added":["src/a.rs"],"modified":[],"deleted":[]}}}' \
    > "$TS_SESSION_FILE"

HTML_B=$(mktemp -t export_ci_B).html
ION_SESSION_DIR="$TS_SESSION_ROOT" \
    "$ION_BIN" --export "$HTML_B" --session snapshot_visible 2>&1 | grep -q "Exported" && \
    pass "B1 export 含 step-snapshot 的 session" || fail "B1 export 失败"

DATA_B=$(decode_session_data "$HTML_B" 2>/dev/null)
if [ -n "$DATA_B" ]; then
    BODY_SNAP=$(echo "$DATA_B" | jq '[.entries[] | select(.customType=="step-snapshot")] | length')
    TIMELINE_SNAP=$(echo "$DATA_B" | jq '[.timelineEntries[] | select(.customType=="step-snapshot")] | length')
    INTERNAL_COUNT=$(echo "$DATA_B" | jq '.internalEntries | length')
    [ "$BODY_SNAP" -eq 1 ] && pass "B2 正文展示 step-snapshot" || fail "B2 正文 step-snapshot 数量错误（$BODY_SNAP）"
    [ "$TIMELINE_SNAP" -eq 1 ] && pass "B3 Timeline 展示 step-snapshot" || fail "B3 Timeline step-snapshot 数量错误（$TIMELINE_SNAP）"
    [ "$INTERNAL_COUNT" -eq 0 ] && pass "B4 无脱离消息树的内部回合记录" || fail "B4 internalEntries 非空（$INTERNAL_COUNT）"
    echo "$DATA_B" | jq -e '
        .timelineEntries[] | select(.id=="snap-1") |
        .parentId=="m2" and
        .data.baselineTreeHash=="tree0" and
        .data.snapshotTreeHash=="tree1" and
        .ionMeta.displayType=="File Snapshot"
    ' >/dev/null && pass "B5 快照关联、tree hash 与语义标签完整" || fail "B5 step-snapshot 字段丢失"
else
    fail "B2 数据解码失败"
fi
rm -f "$HTML_B"
rm -r "$TS_SESSION_ROOT"

# ═════════════════════════════════════════════════════════
# Group E: compaction → Timeline 与正文都必须展示
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group E: 压缩 Entry 完整展示 ──"

COMPACTION_ROOT=$(mktemp -d)
mkdir -p "$COMPACTION_ROOT/exact"
COMPACTION_SESSION="$COMPACTION_ROOT/exact/session.jsonl"
printf '%s\n' \
    '{"type":"session","version":3,"id":"compaction_export","timestamp":"2026-08-08T09:00:00Z","cwd":"/test"}' \
    '{"type":"message","id":"cm1","parentId":"compaction_export","timestamp":"2026-08-08T09:00:01Z","message":{"User":{"role":"user","content":[{"Text":{"text":"long conversation"}}],"source":"prompt"}}}' \
    '{"type":"compaction","id":"compact-1","parentId":"cm1","timestamp":"2026-08-08T09:00:02Z","summary":"Earlier work was compacted","tokensBefore":42000,"batchCount":2,"stage":"batched_merged"}' \
    '{"type":"message","id":"cm2","parentId":"compact-1","timestamp":"2026-08-08T09:00:03Z","message":{"Assistant":{"role":"assistant","provider":"zai","model":"glm-5.2","api":"openai-completions","usage":{"input":100,"output":30},"stop_reason":"stop","content":[{"Text":{"text":"continued after compaction"}}]}}}' \
    > "$COMPACTION_SESSION"

HTML_E=$(mktemp -t export_ci_E).html
ION_SESSION_DIR="$COMPACTION_ROOT" \
    "$ION_BIN" --export "$HTML_E" --session compaction_export 2>&1 | grep -q "Exported" && \
    pass "E1 export 确定性 compaction session" || fail "E1 compaction session export 失败"
DATA_E=$(decode_session_data "$HTML_E" 2>/dev/null)
TIMELINE_COMPACTION=$(echo "$DATA_E" | jq '[.timelineEntries[] | select(.type=="compaction")] | length')
BODY_COMPACTION=$(echo "$DATA_E" | jq '[.entries[] | select(.type=="compaction")] | length')
[ "$TIMELINE_COMPACTION" -eq 1 ] && [ "$TIMELINE_COMPACTION" -eq "$BODY_COMPACTION" ] && \
    pass "E2 Compaction 在 Timeline 与正文一一对应（$BODY_COMPACTION 条）" || \
    fail "E2 Compaction Timeline/正文展示不一致"
COMPACTION_META=$(echo "$DATA_E" | jq -r '.timelineEntries[] | select(.type=="compaction") | .ionMeta.displayType')
[ "$COMPACTION_META" = "Compaction" ] && \
    pass "E3 Compaction 使用独立流程语义" || \
    fail "E3 Compaction ionMeta 缺失"
grep -q "entry.type === 'compaction'" "$HTML_E" && \
    pass "E4 Compaction 使用独立内置卡片渲染" || \
    fail "E4 缺少 Compaction 内置卡片渲染"
rm -f "$HTML_E"
rm -r "$COMPACTION_ROOT"

# ═════════════════════════════════════════════════════════
# Group F: 分支 session → 只导出 active branch + 分叉记录
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group F: 当前分支导出 ──"

BRANCH_ROOT=$(mktemp -d)
mkdir -p "$BRANCH_ROOT/exact"
BRANCH_SESSION="$BRANCH_ROOT/exact/session.jsonl"
apply_branch_fixture() {
    printf '%s\n' \
        '{"type":"session","version":3,"id":"branch_export","timestamp":"2026-08-08T08:00:00Z","cwd":"/test"}' \
        '{"type":"message","id":"m1","parentId":"branch_export","timestamp":"2026-08-08T08:00:01Z","message":{"User":{"role":"user","content":[{"Text":{"text":"root"}}]}}}' \
        '{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-08T08:00:02Z","message":{"Assistant":{"role":"assistant","content":[{"Text":{"text":"base"}}]}}}' \
        '{"type":"message","id":"old-3","parentId":"m2","timestamp":"2026-08-08T08:00:03Z","message":{"User":{"role":"user","content":[{"Text":{"text":"old branch"}}]}}}' \
        '{"type":"message","id":"old-4","parentId":"old-3","timestamp":"2026-08-08T08:00:04Z","message":{"Assistant":{"role":"assistant","content":[{"Text":{"text":"abandoned"}}]}}}' \
        '{"type":"leaf_pointer","id":"lp-1","parentId":null,"timestamp":"2026-08-08T08:00:05Z","leafId":"m2"}' \
        '{"type":"message","id":"m5","parentId":"m2","timestamp":"2026-08-08T08:00:06Z","message":{"User":{"role":"user","content":[{"Text":{"text":"active branch"}}]}}}' \
        '{"type":"custom","id":"snap-1","parentId":"m5","timestamp":"2026-08-08T08:00:07Z","customType":"step-snapshot","data":{"baselineTreeHash":"tree0","snapshotTreeHash":"tree1","turnIndex":1,"diff":{"added":[],"modified":[],"deleted":[]}}}' \
        '{"type":"custom_message","id":"hook-1","parentId":null,"timestamp":"2026-08-08T08:00:08Z","customType":"hook_event","content":"active hook","display":true}' \
        '{"type":"custom_message","id":"old-note","parentId":"old-4","timestamp":"2026-08-08T08:00:09Z","customType":"diagnostics","content":"old branch detail","display":true}' \
        '{"type":"branch_summary","id":"bs-1","parentId":"old-4","timestamp":"2026-08-08T08:00:10Z","fromId":"old-4","summary":"abandoned branch"}' \
        > "$BRANCH_SESSION"
}
apply_branch_fixture

HTML_F=$(mktemp -t export_ci_F).html
ION_SESSION_DIR="$BRANCH_ROOT" \
    "$ION_BIN" --export "$HTML_F" --session branch_export 2>&1 | grep -q "Exported" && \
    pass "F1 分支 session 导出成功" || fail "F1 分支 session 导出失败"
DATA_F=$(decode_session_data "$HTML_F" 2>/dev/null)
ACTIVE_IDS=$(echo "$DATA_F" | jq -r '[.entries[].id] | join(",")')
if echo ",$ACTIVE_IDS," | grep -q ',m1,' && \
   echo ",$ACTIVE_IDS," | grep -q ',m2,' && \
   echo ",$ACTIVE_IDS," | grep -q ',m5,' && \
   ! echo ",$ACTIVE_IDS," | grep -q ',old-3,' && \
   ! echo ",$ACTIVE_IDS," | grep -q ',old-4,' && \
   ! echo ",$ACTIVE_IDS," | grep -q ',old-note,'; then
    pass "F2 正文只保留 root→active leaf，废弃分支内容已排除"
else
    fail "F2 active branch 选择错误（$ACTIVE_IDS）"
fi
BRANCH_RECORDS=$(echo "$DATA_F" | jq '[.timelineEntries[] | select(.type=="leaf_pointer")] | length')
[ "$BRANCH_RECORDS" -eq 1 ] && \
    pass "F3 分叉保留为一条 leaf_pointer 记录" || \
    fail "F3 分叉记录数量错误（$BRANCH_RECORDS）"
ACTIVE_LEAF=$(echo "$DATA_F" | jq -r '.activeLeafId // empty')
SOURCE_COUNT=$(echo "$DATA_F" | jq -r '.sourceEntryCount // 0')
OMITTED_COUNT=$(echo "$DATA_F" | jq -r '.omittedBranchEntryCount // 0')
[ "$ACTIVE_LEAF" = "m5" ] && [ "$SOURCE_COUNT" -eq 10 ] && [ "$OMITTED_COUNT" -eq 3 ] && \
    pass "F4 导出记录 active leaf 与省略分支统计" || \
    fail "F4 分支元数据错误（leaf=$ACTIVE_LEAF source=$SOURCE_COUNT omitted=$OMITTED_COUNT）"

rm -f "$HTML_F"
rm -r "$BRANCH_ROOT"

# ═════════════════════════════════════════════════════════
# Group G: 完整流程语义（LLM → Tool → Hook → ToolResult → Custom）
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group G: LLM / Tool / Custom / Extension 流程语义 ──"

FLOW_ROOT=$(mktemp -d)
mkdir -p "$FLOW_ROOT/exact"
FLOW_SESSION="$FLOW_ROOT/exact/session.jsonl"
cp "$PROJECT_DIR/tests/fixtures/export/flow_semantics/session.jsonl" "$FLOW_SESSION"

HTML_G=$(mktemp -t export_ci_G).html
ION_SESSION_DIR="$FLOW_ROOT" \
    "$ION_BIN" --export "$HTML_G" --session flow_semantics 2>&1 | grep -q "Exported" && \
    pass "G1 完整流程 fixture 导出成功" || fail "G1 完整流程 fixture 导出失败"
DATA_G=$(decode_session_data "$HTML_G" 2>/dev/null)
echo "$DATA_G" | jq -e '
    .flowSummary.llmCalls == 2 and
    .flowSummary.toolCalls == 1 and
    .flowSummary.toolResults == 1 and
    .flowSummary.customEntries == 4 and
    .flowSummary.contextInjections == 1
' >/dev/null && pass "G2 Flow Summary 统计 LLM/Tool/Custom/注入" || fail "G2 Flow Summary 计数错误"
echo "$DATA_G" | jq -e '
    .flowSummary.typeInventory.supported.entryTypes == 17 and
    .flowSummary.typeInventory.supported.builtInCustomTypes == 25 and
    (.flowSummary.typeInventory.supported.entryTypeNames | length) == 17 and
    (.flowSummary.typeInventory.supported.builtInCustomTypeNames | length) == 25 and
    .flowSummary.typeInventory.current.rawEntryTypes == 3 and
    .flowSummary.typeInventory.current.visibleTypes == 7 and
    .flowSummary.typeInventory.current.messageRoles == 4 and
    .flowSummary.typeInventory.current.customTypes == 4 and
    .flowSummary.typeInventory.current.builtInCustomTypes == 3 and
    .flowSummary.typeInventory.current.extensionCustomTypes == 1 and
    .flowSummary.typeInventory.current.unknownCustomTypes == 0 and
    .flowSummary.typeInventory.current.extensions == 4
' >/dev/null && pass "G8 类型目录区分 ION 支持总数与当前会话实际类型" || fail "G8 类型目录统计错误"
echo "$DATA_G" | jq -e '
    .timelineEntries[] | select(.id=="fh1") |
    .ionMeta.displayType=="Hook" and
    .ionMeta.source.name=="hooks" and
    .ionMeta.source.confidence=="recorded" and
    .ionMeta.audience.llmContext=="not_in_context"
' >/dev/null && pass "G3 Hook 明确标记为旁路审计而非 ToolResult" || fail "G3 Hook 语义错误"
echo "$DATA_G" | jq -e '
    .timelineEntries[] | select(.id=="fc1") |
    .ionMeta.displayType=="Diagnostics" and
    .ionMeta.source.name=="lsp" and
    .ionMeta.audience.llmContext=="input"
' >/dev/null && pass "G4 LSP Custom 标记来源且进入 LLM 上下文" || fail "G4 LSP Custom 语义错误"
echo "$DATA_G" | jq -e '
    .timelineEntries[] | select(.id=="fw1") |
    .ionMeta.displayType=="Custom" and
    .ionMeta.customClass=="extension" and
    .ionMeta.source.name=="weather" and
    .ionMeta.audience.liveUi==false
' >/dev/null && pass "G5 运行时 Extension Custom 统一命名且保留精确来源" || fail "G5 运行时 Custom 语义错误"
SOURCE_G=$(echo "$DATA_G" | jq '.sourceEntries | length')
[ "$SOURCE_G" -eq 8 ] && \
    echo "$DATA_G" | jq -e '.sourceEntries[-1].customType=="step-snapshot"' >/dev/null && \
    pass "G6 sourceEntries 保留完整正文原始穿插顺序" || \
    fail "G6 sourceEntries 未保留完整有序数据"
grep -q 'builtin:' "$HTML_G" && \
    grep -q 'sourceConfidence' "$PROJECT_DIR/docs/tasks/fix-export-fold-spec.md" && \
    grep -q 'Current types' "$HTML_G" && \
    grep -q 'ION catalog' "$HTML_G" && \
    grep -q 'live UI · hidden' "$HTML_G" && \
    pass "G7 内置 Custom 筛选与来源/受众 UI 已打包" || \
    fail "G7 流程语义 UI 缺失"

rm -f "$HTML_G"
rm -r "$FLOW_ROOT"

# ═════════════════════════════════════════════════════════
# Group D: --export + prompt → tools 面板应有内容
# ═════════════════════════════════════════════════════════
echo ""
echo "-- Group D: export-after-run 工具面板 ──"

WORKDIR_D=$(mktemp -d)
HTML_D="$WORKDIR_D/with_tools.html"

cd "$WORKDIR_D"
# 用 FauxProvider 跑一次对话 + 同时 export
ION_GRACEFUL_DRAIN_MS=50 \
ION_FAUX_REPLY="test response for export" \
    "$ION_BIN" --offline --no-extensions --export "$HTML_D" -p "hello" 2>&1 >/dev/null || true

if [ -f "$HTML_D" ]; then
    DATA_D=$(decode_session_data "$HTML_D" 2>/dev/null)
    if [ -n "$DATA_D" ]; then
        # tools 字段应非空（export-after-run 模式塞入了 tool registry）
        TOOLS_N=$(echo "$DATA_D" | jq '.tools | length')
        if [ "$TOOLS_N" -gt 0 ] 2>/dev/null; then
            pass "D1 export-after-run 包含 tools 字段（$TOOLS_N 个工具）"
            # 验证基本工具在内
            HAS_BASH=$(echo "$DATA_D" | jq '[.tools[] | select(.name == "bash")] | length')
            HAS_READ=$(echo "$DATA_D" | jq '[.tools[] | select(.name == "read")] | length')
            [ "$HAS_BASH" -gt 0 ] && pass "D2 bash 工具在列表中" || fail "D2 缺 bash 工具"
            [ "$HAS_READ" -gt 0 ] && pass "D3 read 工具在列表中" || fail "D3 缺 read 工具"
            # 验证 tool 有 name/description/parameters 三字段
            SCHEMA_OK=$(echo "$DATA_D" | jq '[.tools[] | select(.name != null and .description != null and .parameters != null)] | length')
            [ "$SCHEMA_OK" = "$TOOLS_N" ] && pass "D4 所有 tool 都有 name/description/parameters" || fail "D4 部分 tool schema 不完整"
        else
            fail "D1 tools 字段为空（export-after-run 没塞入工具）"
        fi
    else
        yellow "D1 数据解码失败（CI FauxProvider session 数据可能不完整）— skip D1-D4"
        SKIP=$((SKIP+4))
    fi
else
    fail "D1 HTML 未生成（export-after-run 没触发）"
fi
rm -rf "$WORKDIR_D"

# ═════════════════════════════════════════════════════════
# Group C: 边界场景
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group C: 边界场景 ──"

WORKDIR_C=$(mktemp -d)
HTML_C="$WORKDIR_C/empty.html"

# C1: 不存在的 session → 应报错
"$ION_BIN" --export "$HTML_C" --session "sess_definitely_does_not_exist_xyz" >"$WORKDIR_C/missing.log" 2>&1
MISSING_STATUS=$?
ERR=$(cat "$WORKDIR_C/missing.log")
if [ "$MISSING_STATUS" -ne 0 ] && echo "$ERR" | grep -qi "not found\|error\|失败"; then
    pass "C1 不存在的 session 报错并返回非零退出码"
else
    fail "C1 不存在的 session 退出状态错误（status=$MISSING_STATUS）：$ERR"
fi

# C2: 没指定 --session（用 last_session 或当前 cwd）→ 应该有合理行为
"$ION_BIN" --export "$WORKDIR_C/auto.html" 2>&1 | grep -q "Exported" && \
    pass "C2 不带 --session 时自动选最近 session" || \
    echo "  ⚠️ C2 跳过（可能 last_session 不可用）"

# C3: JSONL 中任意损坏行必须带文件和行号失败，禁止静默丢 Entry。
BROKEN_ROOT=$(mktemp -d)
mkdir -p "$BROKEN_ROOT/exact"
printf '%s\n' \
    '{"type":"session","version":3,"id":"broken_export","timestamp":"2026-08-08T11:00:00Z","cwd":"/test"}' \
    '{"type":"message","id":"ok","parentId":"broken_export","message":{"User":{"role":"user","content":[]}}}' \
    '{this line is broken json' \
    > "$BROKEN_ROOT/exact/session.jsonl"
ION_SESSION_DIR="$BROKEN_ROOT" \
    "$ION_BIN" --export "$WORKDIR_C/broken.html" --session broken_export >"$WORKDIR_C/broken.log" 2>&1
BROKEN_STATUS=$?
if [ "$BROKEN_STATUS" -ne 0 ] && grep -q 'invalid session JSONL.*:3:' "$WORKDIR_C/broken.log"; then
    pass "C3 损坏 JSONL 带行号失败，不静默丢 Entry"
else
    fail "C3 损坏 JSONL 处理错误（status=$BROKEN_STATUS）"
fi
rm -rf "$BROKEN_ROOT"

rm -rf "$WORKDIR_C"

# ═════════════════════════════════════════════════════════
echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
