#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Browser Fetch Tool CI — SPA/CSR 感知抓取（ion 内置 fetch 工具）
#
# 验证：内核直 spawn browser 二进制（~/Project/study-rust/browser）的
#       `browser fetch --json`，经 ion rpc call_tool 从外部完整走通。
#
# 覆盖文档：docs/design/BROWSER_FETCH_TOOL.md
#   Group A：基础抓取（XHR 动态渲染内容进入 RPC 响应 + 结构化字段）
#   Group B：warnings 透传（壳页信号到达调用方）
#   Group C：URL 白名单拒绝（config fetch.allow_urls）
#   Group D：缺二进制 → 报安装指引（config fetch.path 指向不存在路径）
# ──────────────────────────────────────────────────────────
set -uo pipefail

PASS=0; FAIL=0
green() { echo -e "  \033[32m✅ $1\033[0m"; }
red()   { echo -e "  \033[31m❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BROWSER_DIR="$PROJECT_DIR/../browser"
ION_BIN="$PROJECT_DIR/target/debug/ion"

# ── 测试隔离：目录与 socket（HOME 隔离在 build 之后才启用——rustup shim 依赖真实 HOME）──
TEST_ROOT="$(mktemp -d /tmp/ion-fetch-ci-XXXXXX)"
TEST_HOME="$TEST_ROOT/home"; mkdir -p "$TEST_HOME/.ion"
TEST_PROJECT="$TEST_ROOT/project"; mkdir -p "$TEST_PROJECT"
SOCK="$TEST_ROOT/host.sock"
export ION_HOST_SOCKET="$SOCK"
export ION_SESSION_DIR="$TEST_HOME/.ion/agent/sessions"

cleanup() {
    [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
    [ -n "${SRV_PID:-}" ] && kill "$SRV_PID" 2>/dev/null
    rm -f "$SOCK"
}
trap cleanup EXIT

echo "════════════════════════════════════════════════════"
echo "  Browser Fetch Tool CI — $(date)"
echo "════════════════════════════════════════════════════"

# ── Phase 0: 构建（用真实 HOME，rustup 需要它找工具链）──
cd "$PROJECT_DIR"
cargo build --bin ion 2>/dev/null || { echo "❌ ion build failed"; exit 1; }
pass "build ion"

if [ -x "$BROWSER_DIR/target/release/browser" ]; then
    BROWSER_BIN="$BROWSER_DIR/target/release/browser"
else
    echo "  ... browser 二进制缺失，现场构建（首次 ~1min）"
    (cd "$BROWSER_DIR" && cargo build --release -p browser-cli >/dev/null 2>&1) \
        && BROWSER_BIN="$BROWSER_DIR/target/release/browser" \
        || { echo "❌ browser 构建失败"; exit 1; }
fi
pass "browser binary: $BROWSER_BIN"

# 此后切到私有 HOME（ion 侧 config/会话隔离），构建全部结束
export HOME="$TEST_HOME"

# faux config：fetch.path 指向真实二进制（不设 ION_BROWSER_PATH，走 config 查找链）
printf '{"default_provider":"faux","default_model":"faux","fetch":{"path":"%s"}}' \
    "$BROWSER_BIN" > "$TEST_HOME/.ion/config.json"

# ── Phase 1: 本地 SPA fixture（XHR 动态渲染，curl 拿不到内容）──
FIXTURE_DIR="$TEST_ROOT/fixture"; mkdir -p "$FIXTURE_DIR"
cat > "$FIXTURE_DIR/index.html" << 'EOF'
<!DOCTYPE html><html><head><title>ION CI Fixture</title></head><body>
<p>loading</p><div id="list"></div>
<script>var x=new XMLHttpRequest();x.open('GET','/api.json',true);
x.onreadystatechange=function(){if(x.readyState===4&&x.status===200){
document.getElementById('list').textContent=JSON.parse(x.responseText).marker;}};
x.send();</script></body></html>
EOF
echo '{"marker":"ION_FETCH_CI_MARKER_42"}' > "$FIXTURE_DIR/api.json"
# 壳页 fixture（<4KB 文本，触发上游 "content is very short" 反爬启发式）
cat > "$FIXTURE_DIR/shell.html" << 'EOF'
<!DOCTYPE html><html><body><p>网络不给力，请稍后重试</p></body></html>
EOF
python3 -m http.server 0 --directory "$FIXTURE_DIR" >/dev/null 2>&1 &
SRV_PID=$!
# 从 /proc 无法直接拿端口（http.server 0 随机），改用固定端口重试几次
kill $SRV_PID 2>/dev/null
FIX_PORT=18742
for try in 1 2 3 4 5; do
    python3 -m http.server $FIX_PORT --directory "$FIXTURE_DIR" >/dev/null 2>&1 &
    SRV_PID=$!
    sleep 1
    curl -s "http://127.0.0.1:$FIX_PORT/index.html" >/dev/null 2>&1 && break
    kill $SRV_PID 2>/dev/null
done
curl -s "http://127.0.0.1:$FIX_PORT/index.html" | grep -q "loading" \
    && pass "SPA fixture server ready (:$FIX_PORT)" \
    || { fail "fixture server 未就绪"; exit 1; }
FIXTURE_URL="http://127.0.0.1:$FIX_PORT"

# ── Phase 2: serve（faux provider，无需真 LLM）──
rm -f "$SOCK"
"$ION_BIN" serve --provider faux --model faux-test \
    > "$TEST_ROOT/serve.log" 2>&1 &
SERVE_PID=$!
ready=false
for i in $(seq 1 15); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        ready=true; break
    fi
done
$ready && pass "serve ready" || { fail "serve 未启动"; exit 1; }

# create_session
CREATE_OUT=$("$ION_BIN" rpc --method create_session \
    --params '{"agent":"build","cwd":"'"$TEST_PROJECT"'","model":"faux-test","provider":"faux"}' 2>/dev/null)
SID=$(echo "$CREATE_OUT" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    data = d.get('data', {})
    print(data.get('session_id') or data.get('sessionId') or '')
except Exception: print('')")
[ -n "$SID" ] && pass "create_session → $SID" || { fail "create_session 失败"; exit 1; }

fetch_rpc() {
    "$ION_BIN" rpc --session "$SID" --method call_tool \
        --params "$1" 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    out = d.get('data', {}).get('output', '')
    if d.get('success') and out:
        try: print(json.dumps(json.loads(out)))   # 工具返回的结构化 JSON 解包
        except Exception: print(out)
    else:
        print('RPC_ERROR:', d.get('error', ''))
except Exception as e:
    print('PARSE_ERROR:', e)"
}

# ── Group A：基础抓取（XHR 渲染内容 + 结构化字段）──
echo "── Group A: 基础抓取 ──"
RESP=$(fetch_rpc '{"tool":"fetch","args":{"url":"'"$FIXTURE_URL"'/index.html","format":"text","timeout_ms":20000}}')
echo "$RESP" | grep -q "ION_FETCH_CI_MARKER_42" \
    && pass "XHR 动态渲染内容进入 RPC 响应（marker 命中）" \
    || fail "marker 未命中（响应: $(echo "$RESP" | head -c 200)）"
echo "$RESP" | grep -q '"elapsed_ms"' \
    && pass "结构化字段 elapsed_ms 存在" || fail "elapsed_ms 缺失"
echo "$RESP" | grep -q '"warnings"' \
    && pass "结构化字段 warnings 存在（透传通道就位）" || fail "warnings 缺失"
echo "$RESP" | grep -q '"title": *"ION CI Fixture"' \
    && pass "title 提取正确" || fail "title 缺失/错误"

# ── Group B：warnings 透传（壳页信号）──
echo "── Group B: 反爬壳页 warnings 透传 ──"
RESP=$(fetch_rpc '{"tool":"fetch","args":{"url":"'"$FIXTURE_URL"'/shell.html","format":"text","timeout_ms":20000}}')
echo "$RESP" | grep -qE '"warnings": *\[[^]]' \
    && pass "壳页触发 warnings 非空（信号到达调用方）" \
    || fail "壳页 warnings 为空（透传失效？）"

# ── Group C：URL 白名单拒绝 ──
echo "── Group C: URL 白名单 ──"
printf '{"default_provider":"faux","default_model":"faux","fetch":{"path":"%s","allow_urls":["https://*.example.com/*"]}}' \
    "$BROWSER_BIN" > "$TEST_HOME/.ion/config.json"
RESP=$(fetch_rpc '{"tool":"fetch","args":{"url":"'"$FIXTURE_URL"'/index.html"}}')
echo "$RESP" | grep -q "not allowed by fetch.allow_urls" \
    && pass "白名单外 URL 被拒（错误信息含指引）" \
    || fail "白名单未生效（响应: $(echo "$RESP" | head -c 200)）"
# 恢复放行 config
printf '{"default_provider":"faux","default_model":"faux","fetch":{"path":"%s"}}' \
    "$BROWSER_BIN" > "$TEST_HOME/.ion/config.json"

# ── Group D：缺二进制 → 安装指引 ──
echo "── Group D: 缺二进制错误指引 ──"
printf '{"default_provider":"faux","default_model":"faux","fetch":{"path":"/nonexistent/ion_ci_browser"}}' \
    > "$TEST_HOME/.ion/config.json"
RESP=$(fetch_rpc '{"tool":"fetch","args":{"url":"'"$FIXTURE_URL"'/index.html"}}')
echo "$RESP" | grep -q "cargo install --git" \
    && pass "缺二进制报错含安装指引" \
    || fail "错误信息缺安装指引（响应: $(echo "$RESP" | head -c 200)）"

# ── 汇总 ──
echo "════════════════════════════════════════════════════"
echo "  Browser Fetch Tool CI: PASS=$PASS FAIL=$FAIL"
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ]
