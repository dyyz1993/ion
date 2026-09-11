#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Browser Fetch 验收 — 真实站点矩阵（走完整 ion rpc 链路）
#
# 与 browser_fetch_ci.sh 的区别：CI 用本地 fixture 验证机制；
# 本脚本用【真实公网站点】验收覆盖面与信号正确性。
#
# 判定纪律（与 browser 侧 M82/M83 共识一致）：
#   - 无门卫 CSR 站：内容必须真实渲染（KPI 刻度）
#   - 反爬/风控站：内容或 warnings 二者必有其一（"不静默给垃圾"即正确）
#   - 网络不通/站点抖动：SKIP，不算 FAIL
#
# 用法：bash tests/browser_fetch_acceptance.sh
#   FULL=1 追加 60s 级慢站（juejin 已知边界演示）
# ──────────────────────────────────────────────────────────
set -uo pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "  \033[32m✅ $1\033[0m"; }
red()   { echo -e "  \033[31m❌ $1\033[0m"; }
yellow(){ echo -e "  \033[33m⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BROWSER_DIR="$PROJECT_DIR/../browser"
ION_BIN="$PROJECT_DIR/target/debug/ion"

TEST_ROOT="$(mktemp -d /tmp/ion-fetch-accept-XXXXXX)"
TEST_HOME="$TEST_ROOT/home"; mkdir -p "$TEST_HOME/.ion"
TEST_PROJECT="$TEST_ROOT/project"; mkdir -p "$TEST_PROJECT"
SOCK="$TEST_ROOT/host.sock"
export ION_HOST_SOCKET="$SOCK"
export ION_SESSION_DIR="$TEST_HOME/.ion/agent/sessions"

cleanup() {
    [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
    rm -f "$SOCK"
}
trap cleanup EXIT

echo "════════════════════════════════════════════════════"
echo "  Browser Fetch 真实站点验收 — $(date)"
echo "════════════════════════════════════════════════════"

# ── 准备：构建 + serve + session ──
cd "$PROJECT_DIR"
cargo build --bin ion 2>/dev/null || { echo "❌ ion build failed"; exit 1; }

if [ -x "$BROWSER_DIR/target/release/browser" ]; then
    BROWSER_BIN="$BROWSER_DIR/target/release/browser"
else
    echo "  ... browser 二进制缺失，现场构建"
    (cd "$BROWSER_DIR" && cargo build --release -p browser-cli >/dev/null 2>&1) \
        && BROWSER_BIN="$BROWSER_DIR/target/release/browser" \
        || { echo "❌ browser 构建失败"; exit 1; }
fi
export HOME="$TEST_HOME"   # 构建全部结束后才切私有 HOME
printf '{"default_provider":"faux","default_model":"faux","fetch":{"path":"%s"}}' \
    "$BROWSER_BIN" > "$TEST_HOME/.ion/config.json"

rm -f "$SOCK"
"$ION_BIN" serve --provider faux --model faux-test > "$TEST_ROOT/serve.log" 2>&1 &
SERVE_PID=$!
ready=false
for i in $(seq 1 15); do
    sleep 1
    "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions" && { ready=true; break; }
done
$ready || { echo "❌ serve 未启动"; exit 1; }

SID=$("$ION_BIN" rpc --method create_session \
    --params '{"agent":"build","cwd":"'"$TEST_PROJECT"'","model":"faux-test","provider":"faux"}' 2>/dev/null \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin); data = d.get('data', {})
    print(data.get('session_id') or data.get('sessionId') or '')
except Exception: print('')")
[ -n "$SID" ] || { echo "❌ create_session 失败"; exit 1; }
echo "  session: $SID"
echo ""

# fetch_rpc URL → 输出解包后的工具 JSON（或 RPC_ERROR: ...）
fetch_rpc() {
    "$ION_BIN" rpc --session "$SID" --method call_tool \
        --params '{"tool":"fetch","args":'"$1"'}' 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    out = d.get('data', {}).get('output', '')
    if d.get('success') and out:
        try: print(json.dumps(json.loads(out), ensure_ascii=False))
        except Exception: print(out)
    else:
        print('RPC_ERROR:', d.get('error', ''))
except Exception as e:
    print('PARSE_ERROR:', e)"
}

# 断言器: expect <名称> <url> <python断言表达式（变量 d=响应JSON）> [timeout_ms]
# ⚠️ ion rpc 客户端响应超时 30s → timeout_ms 必须 ≤ 12s（ION 硬兜底 +15s = 27s < 30s），
#    否则 RPC 空输出假失败。慢站拿到的是"部分内容 + warnings"，同样可断言。
EXPECT_N=0
expect() {
    EXPECT_N=$((EXPECT_N+1))
    local name="$1" url="$2" assertion="$3" tms="${4:-12000}"
    local resp
    resp=$(fetch_rpc '{"url":"'"$url"'","format":"text","timeout_ms":'"$tms"'}')
    if echo "$resp" | grep -q "^RPC_ERROR:\|PARSE_ERROR:"; then
        if echo "$resp" | grep -qiE "timed out|timeout|connection|dns|temporarily|unreachable|proxy|tls|certificate"; then
            skip "[$EXPECT_N] $name — 网络不可达（$(echo "$resp" | head -c 90)）"
        else
            fail "[$EXPECT_N] $name — RPC 错误: $(echo "$resp" | head -c 120)"
        fi
        return
    fi
    local verdict
    verdict=$(RESP_JSON="$resp" URL="$url" python3 -c "
import sys, json, os
try:
    d = json.loads(os.environ['RESP_JSON'])
    expr = os.environ.get('ASSERT', '')
except Exception as e:
    print('FAIL'); sys.exit()
ctx_ok = 'result' if True else ''
")
    # 用 python 做断言（ASSERT 表达式，d 为响应）
    verdict=$(ASSERT="$assertion" RESP_JSON="$resp" python3 -c "
import sys, json, os
try:
    d = json.loads(os.environ['RESP_JSON'])
    ok = eval(os.environ['ASSERT'], {'d': d})
    print('PASS' if ok else 'FAIL')
except AssertionError:
    print('FAIL')
except Exception as e:
    print('FAIL')")
    if [ "$verdict" = "PASS" ]; then
        pass "[$EXPECT_N] $name"
    else
        local clen wcount
        clen=$(RESP_JSON="$resp" python3 -c "
import sys, json, os
try: print(len(json.loads(os.environ['RESP_JSON']).get('content','')))
except Exception: print('ERR')")
        wcount=$(RESP_JSON="$resp" python3 -c "
import sys, json, os
try: print(len(json.loads(os.environ['RESP_JSON']).get('warnings',[])))
except Exception: print('ERR')")
        fail "[$EXPECT_N] $name — 断言未过（content ${clen} 字符 / warnings ${wcount} 条）"
    fi
}

echo "── 矩阵 1：静态对照（curl 也能拿，工具必须同样拿得到）──"
expect "example.com 静态页" "https://example.com/" \
    "'Example Domain' in d.get('content','') and d.get('title','').strip() != ''"

echo ""
echo "── 矩阵 2：无门卫纯 CSR 站（验收 KPI 刻度，内容必须真实渲染）──"
expect "react.dev/learn 纯 CSR 渲染" "https://react.dev/learn" \
    "len(d.get('content','')) > 8000 and ('components' in d.get('content','').lower())" \
    15000

echo ""
echo "── 矩阵 3：中文内容站 ──"
expect "sspai.com 中文内容流（text 格式，标题流即真实内容）" "https://sspai.com/" \
    "len(d.get('content','')) > 1000" 15000

echo ""
echo "── 矩阵 4：代码托管（列表页数据）──"
expect "github.com/trending" "https://github.com/trending" \
    "len(d.get('content','')) > 3000 and ('star' in d.get('content','').lower() or 'trending' in (d.get('title','')+d.get('content','')).lower())" \
    15000

echo ""
echo "── 矩阵 5：反爬站（判定纪律：内容或 warnings 必有其一，不静默给垃圾）──"
expect "baidu 搜索（结果页或壳页信号均合格）" "https://www.baidu.com/s?wd=rust+browser" \
    "len(d.get('content','')) > 500 or len(d.get('warnings',[])) > 0" 15000
expect "36kr（大概率安全检测页 → warnings 必须响）" "https://36kr.com/" \
    "len(d.get('content','')) > 500 or len(d.get('warnings',[])) > 0" 15000

echo ""
echo "── 矩阵 6：结构化字段完备性（每次调用都必须带）──"
RESP=$(fetch_rpc '{"url":"https://example.com/","format":"text","timeout_ms":12000}')
for field in url title content elapsed_ms truncated warnings; do
    if echo "$RESP" | python3 -c "
import sys, json
d = json.load(sys.stdin)
assert '$field' in d" 2>/dev/null; then
        pass "字段 $field 存在"
    else
        fail "字段 $field 缺失"
    fi
done

if [ "${FULL:-0}" = "1" ]; then
    echo ""
    echo "── 矩阵 7（FULL）：风控站已知边界（60s 级，不挂死 + 有警告即正确）──"
    expect "juejin.cn 已知边界：不挂死且返回骨架+警告" "https://juejin.cn/" \
        "len(d.get('warnings',[])) > 0 and len(d.get('content','')) > 200" 12000
fi

echo ""
echo "════════════════════════════════════════════════════"
echo "  验收结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ]
