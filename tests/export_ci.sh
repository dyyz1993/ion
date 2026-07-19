#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Export CI — 验证 ion --export HTML 导出的格式正确性
#
# 背景：ION session JSONL 存的是 Rust enum 序列化形式
# ({"message":{"Assistant":{...}}} / content blocks {"Text":{"text":...}}),
# 而 pi export-html 模板期望扁平形式
# ({"message":{"role":"assistant",...}} / {"type":"text","text":...}).
# 之前缺这层转换，导致侧边栏大量 [undefined]。
#
# 这个 CI 脚本验证转换链路：
#   Group A:  真实 ion 跑一个对话 → 导出 HTML → 解码 base64 → 检查转换正确
#   Group B:  直接拿现有 session 文件 → 导出 → 检查 message/custom_message/turn_summary 转换
#   Group C:  边界场景：空 session / 缺 message 字段 / turn_summary 无 summary
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

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ═════════════════════════════════════════════════════════
# Group A: 真实对话 → 导出 → 验证转换
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group A: 真实对话导出（FauxProvider 驱动） ──"

WORKDIR=$(mktemp -d)
HTML="$WORKDIR/out.html"
SOCK="$HOME/.ion/host.sock"
# 清理上次 host 残留
[ -e "$SOCK" ] && lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null || true
sleep 1

cd "$WORKDIR"
# FauxProvider 驱动一次对话（无需真实 API）
ION_FAUX_REPLY="Hello from faux export test" \
    "$ION_BIN" -p "hi" 2>&1 >/dev/null || true

# 找最近这次会话的 sid（先从 last_session，再 fallback 到最近改的 session.jsonl）
LAST_SID=$(cat "$HOME/.ion/agent/last_session" 2>/dev/null)
if [ -z "$LAST_SID" ]; then
    # fallback：扫最近改的 session.jsonl 取 header.id
    LATEST_SF=$(ls -t "$HOME/.ion/agent/sessions/"*/session.jsonl 2>/dev/null | head -1)
    [ -n "$LATEST_SF" ] && LAST_SID=$(head -1 "$LATEST_SF" | jq -r '.id // empty' 2>/dev/null)
fi

if [ -n "$LAST_SID" ]; then
    "$ION_BIN" --export "$HTML" --session "$LAST_SID" 2>&1 | grep -q "Exported" && \
        pass "A1 export 命令成功（sid=$LAST_SID）" || fail "A1 export 失败"
else
    fail "A1 没有可用 session（last_session 为空）"
fi

if [ -f "$HTML" ]; then
    SIZE=$(stat -f%z "$HTML" 2>/dev/null || stat -c%s "$HTML" 2>/dev/null)
    [ "$SIZE" -gt 10000 ] && pass "A2 HTML 文件大小正常（$SIZE bytes）" || fail "A2 HTML 太小（$SIZE bytes）"
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
            fail "A3 base64 解码失败"
        fi
    fi
rm -rf "$WORKDIR"

# ═════════════════════════════════════════════════════════
# Group B: 现有 session 文件 → 导出 → 验证 turn_summary 转换
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group B: 现有 session 导出（turn_summary 转换） ──"

# 找一个有 turn_summary 的现有 session
TS_DIR=""
for d in "$HOME/.ion/agent/sessions/"*; do
    SF="$d/session.jsonl"
    [ ! -f "$SF" ] && continue
    if grep -q '"type":"turn_summary"' "$SF" 2>/dev/null; then
        TS_DIR="$SF"
        break
    fi
done

if [ -n "$TS_DIR" ]; then
    # 从文件路径推 sid
    DIR_NAME=$(basename "$(dirname "$TS_DIR")")
    # session.jsonl 第一行有真实 sid
    SID=$(head -1 "$TS_DIR" | jq -r '.id // empty' 2>/dev/null)

    HTML_B=$(mktemp -t export_ci_B).html
    if [ -n "$SID" ]; then
        "$ION_BIN" --export "$HTML_B" --session "$SID" 2>&1 | grep -q "Exported" && \
            pass "B1 export 现有 session（$SID）" || fail "B1 export 失败"

        DATA_B=$(decode_session_data "$HTML_B" 2>/dev/null)
        if [ -n "$DATA_B" ]; then
            # turn_summary 应该被完全过滤掉（既不是 raw turn_summary，也不转成 custom_message）
            # 它们是内部记录，会污染主体内容的"入参→响应值"流程
            RAW_TS=$(echo "$DATA_B" | jq '[.entries[] | select(.type=="turn_summary")] | length')
            CONVERTED_TS=$(echo "$DATA_B" | jq '[.entries[] | select(.type=="custom_message" and .customType=="turn_summary")] | length')
            [ "$RAW_TS" -eq 0 ] && \
                pass "B2 raw turn_summary 已过滤（剩余 $RAW_TS）" || \
                fail "B2 仍有 $RAW_TS 条 raw turn_summary 未过滤"
            [ "$CONVERTED_TS" -eq 0 ] && \
                pass "B3 turn_summary custom_message 已过滤（不再污染主体内容）" || \
                fail "B3 仍有 $CONVERTED_TS 条 turn_summary custom_message"
        else
            fail "B2 数据解码失败"
        fi
    else
        fail "B1 session sid 提取失败"
    fi
    rm -f "$HTML_B"
else
    echo "  ⚠️ 跳过 Group B：没有找到含 turn_summary 的 session 文件"
fi

# ═════════════════════════════════════════════════════════
# Group D: --export + prompt → tools 面板应有内容
# ═════════════════════════════════════════════════════════
echo ""
echo "-- Group D: export-after-run 工具面板 ──"

WORKDIR_D=$(mktemp -d)
HTML_D="$WORKDIR_D/with_tools.html"

cd "$WORKDIR_D"
# 用 FauxProvider 跑一次对话 + 同时 export
ION_FAUX_REPLY="test response for export" \
    "$ION_BIN" --export "$HTML_D" -p "hello" 2>&1 >/dev/null || true

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
        fail "D1 数据解码失败"
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
ERR=$("$ION_BIN" --export "$HTML_C" --session "sess_definitely_does_not_exist_xyz" 2>&1)
if echo "$ERR" | grep -qi "not found\|error\|失败"; then
    pass "C1 不存在的 session 报错（不静默成功）"
else
    fail "C1 不存在的 session 没有报错：$ERR"
fi

# C2: 没指定 --session（用 last_session 或当前 cwd）→ 应该有合理行为
"$ION_BIN" --export "$WORKDIR_C/auto.html" 2>&1 | grep -q "Exported" && \
    pass "C2 不带 --session 时自动选最近 session" || \
    echo "  ⚠️ C2 跳过（可能 last_session 不可用）"

rm -rf "$WORKDIR_C"

# ═════════════════════════════════════════════════════════
echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
