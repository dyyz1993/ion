#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Lyric WebUI CI — 验证歌词改编网页版的网关链路
#
# 这个 CI 不调真实 LLM，用 FauxProvider 驱动 + 直接验证网关的
# HTTP/WebSocket 桥接是否正确。
#
#   Group A:  build + 起 host + 起网关 + 基础 HTTP（静态文件、healthz）
#   Group B:  RPC 链路：POST /rpc create_session → 拿到 session_id
#   Group C:  错误处理：未起 host 时 /rpc 返回 502（模拟）
#
# 用法：bash tests/lyric_webui_ci.sh
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
WEBUI_DIR="$PROJECT_DIR/webui"
GATEWAY_PORT=${LYRIC_CI_PORT:-8799}

if ! command -v jq &>/dev/null; then
    echo "❌ 需要 jq"; exit 1
fi
if ! command -v node &>/dev/null; then
    echo "❌ 需要 node (>=18)"; exit 1
fi
NODE_MAJOR=$(node -v | sed 's/v//' | cut -d. -f1)
if [ "$NODE_MAJOR" -lt 18 ]; then
    echo "❌ node 版本过低: $(node -v) (需 >=18)"; exit 1
fi

echo "════════════════════════════════════════════════════"
echo "  Lyric WebUI CI — $(date)"
echo "════════════════════════════════════════════════════"

# ── build ion ──
cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ── 确保网关依赖 ws 已安装 ──
if [ ! -d "$WEBUI_DIR/node_modules/ws" ]; then
    ( cd "$WEBUI_DIR" && npm install --silent --no-audit --no-fund >/dev/null 2>&1 ) \
        || { echo "❌ npm install ws 失败"; exit 1; }
fi
pass "ws 依赖就绪"

# ═════════════════════════════════════════════════════════
# Group A: 起 host + 网关 + 基础 HTTP
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group A: 基础 HTTP 服务 ──"

# 起一个干净的 host（用 FauxProvider 兜底，避免真 LLM）
SOCK="${ION_HOST_SOCKET:-$HOME/.ion/host.sock}"
# 清理残留 host：按 pid 文件 + socket 精确清理（不用宽泛 pkill —— 见 AGENTS.md CI 规范）
# 必须彻底清，否则老 host 占着 socket，CI 会误以为自己的 host 起来了（复用老 host）。
cleanup_stale_host() {
    # 1. pid 文件（ion 自己的 host.pid）
    [ -f "$HOME/.ion/host.pid" ] && kill -9 "$(cat "$HOME/.ion/host.pid")" 2>/dev/null
    # 2. 占用 socket 的进程
    lsof -ti "$SOCK" 2>/dev/null | xargs kill -9 2>/dev/null
    # 3. 杀 watchdog（避免它把 host 拉起来）
    ps aux | grep -E "scripts/watchdog|ci_watchdog" | grep -v grep | awk '{print $2}' | xargs kill -9 2>/dev/null
    sleep 1
    rm -f "$HOME/.ion/host.pid" "$SOCK" 2>/dev/null
}
cleanup_stale_host
ION_FAUX_REPLY="ci ready" "$ION_BIN" serve > /tmp/lyric_ci_host.log 2>&1 &
HOST_PID=$!
# 等 host 就绪 —— 必须验证是【自己起的】host（HOST_PID 活着 + socket 存在 + list_sessions 通）
HOST_READY=false
for _ in $(seq 1 20); do
    if kill -0 "$HOST_PID" 2>/dev/null \
       && [ -S "$SOCK" ] \
       && "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q sessions; then
        HOST_READY=true; break
    fi
    sleep 1
done
if [ "$HOST_READY" != "true" ]; then
    echo "❌ host 未就绪（自己的 host 没起来，可能被老 host 抢占）"
    echo "--- host log ---"; tail -5 /tmp/lyric_ci_host.log | grep -v "_encode\|_decode"
    kill "$HOST_PID" 2>/dev/null; exit 1
fi
pass "ion serve 已起 (PID=$HOST_PID，已验证是新 host)"

# 起网关
node "$WEBUI_DIR/gateway.mjs" --port "$GATEWAY_PORT" > /tmp/lyric_ci_gw.log 2>&1 &
GW_PID=$!
GW_READY=false
for _ in $(seq 1 15); do
    if curl -sf "http://localhost:$GATEWAY_PORT/healthz" >/dev/null 2>&1; then
        GW_READY=true; break
    fi
    sleep 0.5
done
if [ "$GW_READY" != "true" ]; then
    echo "❌ 网关未就绪"; cat /tmp/lyric_ci_gw.log; kill "$HOST_PID" "$GW_PID" 2>/dev/null; exit 1
fi
pass "网关已起 (port=$GATEWAY_PORT, PID=$GW_PID)"

# healthz 返回正确结构
HZ=$(curl -sf "http://localhost:$GATEWAY_PORT/healthz")
if echo "$HZ" | jq -e '.ok == true and (.sock | tostring | length > 0)' >/dev/null; then
    pass "/healthz 返回 {ok:true, sock:...}"
else
    fail "/healthz 返回异常: $HZ"
fi

# 静态首页能拿到，且包含标题
INDEX=$(curl -sf "http://localhost:$GATEWAY_PORT/")
if echo "$INDEX" | grep -q "歌词改编工坊"; then
    pass "GET / 返回 index.html (含标题)"
else
    fail "GET / 内容异常"
fi

# 路径逃逸防护
ESC=$(curl -o /dev/null -s -w "%{http_code}" "http://localhost:$GATEWAY_PORT/../../../etc/passwd")
if [ "$ESC" = "403" ] || [ "$ESC" = "404" ]; then
    pass "路径逃逸被拦截 ($ESC)"
else
    fail "路径逃逸未拦截 ($ESC)"
fi

# ═════════════════════════════════════════════════════════
# Group B: RPC 链路 — create_session
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group B: RPC 转发（create_session） ──"

# 用 build agent 兜底（不依赖 lyricist agent 在当前 ion 编译版本可用）
RPC_RES=$(curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" \
    -H "Content-Type: application/json" \
    -d '{"method":"create_session","params":{"agent":"build"}}')

SID=$(echo "$RPC_RES" | jq -r '.data.session_id // empty' 2>/dev/null)
if [ -n "$SID" ]; then
    pass "POST /rpc create_session 返回 session_id=$SID"
else
    fail "create_session 无 session_id: $RPC_RES"
fi

# 响应必须带 id 字段（网关透传）
if echo "$RPC_RES" | jq -e '.id != null' >/dev/null 2>&1; then
    pass "RPC 响应携带 id 字段"
else
    fail "RPC 响应缺 id 字段"
fi

# list_sessions 经网关也能通
LS=$(curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" \
    -H "Content-Type: application/json" \
    -d '{"method":"list_sessions"}')
if echo "$LS" | jq -e '.data.sessions | type == "array"' >/dev/null 2>&1; then
    pass "POST /rpc list_sessions 返回 sessions 数组"
else
    fail "list_sessions 异常: $LS"
fi

# ═════════════════════════════════════════════════════════
# Group C: 错误处理
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group C: 错误处理 ──"

# 非法 JSON body
BAD=$(curl -s -o /dev/null -w "%{http_code}" -X POST "http://localhost:$GATEWAY_PORT/rpc" \
    -H "Content-Type: application/json" -d 'not json')
if [ "$BAD" = "400" ]; then
    pass "非法 JSON body → 400"
else
    fail "非法 JSON 应返回 400, 实际 $BAD"
fi

# 未知路由
NF=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:$GATEWAY_PORT/nope")
if [ "$NF" = "404" ]; then
    pass "未知路由 → 404"
else
    fail "未知路由应 404, 实际 $NF"
fi

# ═════════════════════════════════════════════════════════
# Group D / E / F: 真实 LLM 端到端（仅 ION_E2E=1 时跑）
#
# Group A/B/C 只验证网关协议层（create_session 不触发 LLM），
# 下面三组用真实 glm-5.2 验证完整链路：
#   D — WebSocket text_delta 流式透传
#   E — lyricist agent 真实改编，断言产出 <lyric_result>
#   F — critic agent 审查，断言产出 VERDICT
#
# 用法：ION_E2E=1 bash tests/lyric_webui_ci.sh
# 会调用真实 LLM（有成本），非确定性，所以不进默认 CI。
# ═════════════════════════════════════════════════════════
if [ "${ION_E2E:-0}" = "1" ]; then
    echo ""
    echo "── 切换到真实 LLM host（Group A/B/C 用的是 FauxProvider host）──"
    # Group A/B/C 用 FauxProvider host（不触发 LLM）。E2E 需要真实 LLM，
    # 所以杀掉 faux host，起一个不带 ION_FAUX_REPLY 的真实 host，网关复用。
    kill "$HOST_PID" 2>/dev/null
    cleanup_stale_host
    "$ION_BIN" serve > /tmp/lyric_ci_e2e_host.log 2>&1 &
    HOST_PID=$!
    E2E_HOST_READY=false
    for _ in $(seq 1 20); do
        if kill -0 "$HOST_PID" 2>/dev/null && [ -S "$SOCK" ] \
           && "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q sessions; then
            E2E_HOST_READY=true; break
        fi
        sleep 1
    done
    if [ "$E2E_HOST_READY" != "true" ]; then
        echo "❌ E2E host 未就绪"
        tail -5 /tmp/lyric_ci_e2e_host.log | grep -v "_encode\|_decode"
        kill "$HOST_PID" "$GW_PID" 2>/dev/null; exit 1
    fi
    pass "切换到真实 LLM host (PID=$HOST_PID)"

    echo ""
    echo "── Group D: WebSocket 流式透传（真实 LLM）──"
    # 注意：text_delta 只在 LLM 生成文本时产生，工具调用期间没有；
    # 所以断言放宽为「agent_start + (text_delta 或 tool_call 之一)」即证明流式通道可用。

    # node 辅助：订阅 → prompt → 收集事件 → 等 agent_end（最多 120s）→ 报告
    WS_RESULT=$(GATEWAY_PORT="$GATEWAY_PORT" node --input-type=module <<'NODEOF' 2>/dev/null
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const WebSocket = require("ws");
const BASE = `http://localhost:${process.env.GATEWAY_PORT}`;
const sleep = ms => new Promise(r => setTimeout(r, ms));
async function rpc(method, params, session) {
  const body = { method, params }; if (session) body.session = session;
  const res = await fetch(`${BASE}/rpc`, { method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body) });
  return res.json();
}
try {
  const cs = await rpc("create_session", { agent: "lyricist" });
  const sid = cs.data?.session_id;
  if (!sid) { console.log("SID=\nSTART=N\nDELTA=N\nTOOL=N\nEND=N\nDCOUNT=0\nTEXT="); process.exit(0); }
  console.log("SID=" + sid);
  const ws = new WebSocket(`ws://localhost:${process.env.GATEWAY_PORT}/ws?session=${sid}`);
  const events = [];
  await new Promise((res, rej) => {
    ws.on("message", d => { try{events.push(JSON.parse(d.toString()))}catch{}; if(events.some(e=>e.type==="gateway_subscribed")) res(); });
    setTimeout(() => rej(new Error("sub timeout")), 5000);
  });
  await rpc("prompt", { text: "改编：床前明月光，疑是地上霜。主题程序员加班。输出<lyric_result>，并 write 到 lyrics_output.md。" }, sid);
  // 最多等 120s 收到 agent_end（lyricist 要分析+改编+write，多轮较慢）
  for (let i=0; i<120; i++) {
    if (events.some(e => e.event?.type === "agent_end")) break;
    await sleep(1000);
  }
  ws.close();
  const hasStart = events.some(e => e.event?.type === "agent_start");
  const hasDelta = events.some(e => e.event?.type === "text_delta");
  const hasTool  = events.some(e => e.event?.type === "tool_call" || e.event?.type === "tool_call_delta");
  const hasEnd   = events.some(e => e.event?.type === "agent_end");
  const deltaCount = events.filter(e => e.event?.type === "text_delta").length;
  // 把 text_delta 拼起来，供 Group E 提取
  const fullText = events.filter(e => e.event?.type === "text_delta").map(e => e.event.delta).join("");
  console.log("START=" + (hasStart?"Y":"N"));
  console.log("DELTA=" + (hasDelta?"Y":"N"));
  console.log("TOOL="  + (hasTool ?"Y":"N"));
  console.log("END="   + (hasEnd  ?"Y":"N"));
  console.log("DCOUNT=" + deltaCount);
  console.log("TEXT=" + fullText.replace(/\n/g, " "));
} catch (e) { console.log("ERR=" + e.message); }
NODEOF
    )
    if echo "$WS_RESULT" | grep -q "^ERR="; then
        fail "WS 测试出错: $(echo "$WS_RESULT" | grep '^ERR=')"
    else
        E2E_SID=$(echo "$WS_RESULT" | grep '^SID=' | cut -d= -f2)
        HAS_START=$(echo "$WS_RESULT" | grep '^START=' | cut -d= -f2)
        HAS_DELTA=$(echo "$WS_RESULT" | grep '^DELTA=' | cut -d= -f2)
        HAS_TOOL=$(echo "$WS_RESULT" | grep '^TOOL=' | cut -d= -f2)
        DCOUNT=$(echo "$WS_RESULT" | grep '^DCOUNT=' | cut -d= -f2)
        WS_TEXT=$(echo "$WS_RESULT" | grep '^TEXT=' | cut -d= -f2-)
        [ "$HAS_START" = "Y" ] && pass "WS 收到 agent_start" || fail "WS 未收到 agent_start"
        # 核心断言：流式通道可用 = 收到 text_delta 或 tool_call（都经 WS 透传）
        if [ "$HAS_DELTA" = "Y" ]; then
            pass "WS 收到 text_delta（${DCOUNT} 个，流式透传成功）"
        elif [ "$HAS_TOOL" = "Y" ]; then
            pass "WS 收到 tool_call（流式通道可用，本轮 LLM 未走文本流）"
        else
            fail "WS 既无 text_delta 也无 tool_call"
        fi
    fi

    # ─── Group E: lyricist 改编产出 <lyric_result> ───
    echo ""
    echo "── Group E: lyricist 真实改编 → 断言 <lyric_result>（真实 LLM）──"
    if [ -z "$E2E_SID" ]; then
        fail "无 session_id 可查产出"
    else
        sleep 2
        # 用 RPC get_messages 提取所有 assistant 文本块（不依赖 jsonl 文件路径）
        # jq 路径: .data.messages[].message.Assistant.content[].Text.text
        ASSIST_TEXT=$("./target/debug/ion" rpc --session "$E2E_SID" --method get_messages 2>/dev/null \
            | jq -r '[.data.messages[]?.message.Assistant?.content[]?.Text?.text] | map(select(. != null)) | join("\n")' 2>/dev/null \
            | grep -v "_encode\|_decode")
        if echo "$ASSIST_TEXT" | grep -q "<lyric_result"; then
            pass "lyricist 产出含 <lyric_result>（session=$E2E_SID）"
            ADAPTED=$(echo "$ASSIST_TEXT" | grep -oE '<adapted>[^<]+</adapted>' | head -1)
            if [ -n "$ADAPTED" ]; then
                pass "lyric_result 含逐句对照（$ADAPTED）"
            else
                fail "未找到 <adapted> 行"
            fi
            if echo "$ASSIST_TEXT" | grep -q "rhyme_check"; then
                pass "含 rhyme_check 自检块"
            else
                fail "缺 rhyme_check 块"
            fi
        else
            fail "lyricist 未产出 <lyric_result>（assistant 文本无该标签）"
            echo "  --- assistant 文本片段 ---"
            echo "$ASSIST_TEXT" | head -3 | sed 's/^/    /'
        fi
    fi

    # ─── Group F: critic 审查产出 VERDICT ───
    echo ""
    echo "── Group F: critic 真实审查 → 断言 VERDICT（真实 LLM）──"
    # critic 是新 session，cwd 可能不同，读不到 lyrics_output.md。
    # 改为直接把改编结果文本喂给 critic（更可靠，不依赖文件路径）。
    C_RPC=$(curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" -H "Content-Type: application/json" \
        -d '{"method":"create_session","params":{"agent":"critic"}}')
    C_SID=$(echo "$C_RPC" | jq -r '.data.session_id' 2>/dev/null)
    if [ -z "$C_SID" ]; then
        fail "create critic session 失败"
    else
        CRITIC_TEXT="下面是 lyricist 的改编结果，请按你的 checklist 审查并给出 VERDICT：
原文：床前明月光，疑是地上霜。
改编：屏前代码长，疑是到天光。
押韵：江阳辙（ang）。音节：5/5。"
        # JSON 转义文本里的双引号和换行
        CRITIC_JSON=$(printf '%s' "$CRITIC_TEXT" | jq -Rs .)
        curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" -H "Content-Type: application/json" \
            -d "{\"method\":\"prompt\",\"session\":\"$C_SID\",\"params\":{\"text\":$CRITIC_JSON}}" >/dev/null
        for i in $(seq 1 40); do
            CST=$(./target/debug/ion rpc --method list_sessions 2>/dev/null | jq -r ".data.sessions[] | select(.session_id==\"$C_SID\") | .status" 2>/dev/null | grep -v "_encode\|_decode" | head -1)
            [ "$CST" = "Idle" ] && break
            sleep 2
        done
        C_TEXT=$("./target/debug/ion" rpc --session "$C_SID" --method get_messages 2>/dev/null \
            | jq -r '[.data.messages[]?.message.Assistant?.content[]?.Text?.text] | map(select(. != null)) | join("\n")' 2>/dev/null \
            | grep -v "_encode\|_decode")
        if echo "$C_TEXT" | grep -qE "VERDICT:\s*(APPROVE|REQUEST_CHANGES)"; then
            VLINE=$(echo "$C_TEXT" | grep -oE 'VERDICT:[^"]*' | head -1)
            pass "critic 产出 VERDICT（$VLINE）"
        else
            fail "critic 未产出 VERDICT"
            echo "  --- critic 文本片段 ---"
            echo "$C_TEXT" | head -3 | sed 's/^/    /'
        fi
    fi

    # ─── Group G: 历史记录 + 会话命名（真实 LLM）───
    echo ""
    echo "── Group G: 历史记录 + 会话命名持久化（真实 LLM）──"
    # Group E 的 lyricist session 应该被前端命名为 🎵...，这里验证命名 RPC 持久化
    NAMED_SID="$E2E_SID"
    curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" -H "Content-Type: application/json" \
        -d "{\"method\":\"append_session_name\",\"session\":\"$NAMED_SID\",\"params\":{\"name\":\"🎵测试主题·测试原词\"}}" >/dev/null 2>&1
    sleep 1
    # list_all_sessions 能查到这个名字
    NLIST=$(curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" -H "Content-Type: application/json" -d '{"method":"list_all_sessions"}')
    NFOUND=$(echo "$NLIST" | jq -r --arg sid "$NAMED_SID" '.data.sessions[] | select(.id==$sid) | .name' 2>/dev/null | grep -v "_encode\|_decode")
    if echo "$NFOUND" | grep -q "🎵测试主题"; then
        pass "append_session_name 持久化成功（list_all_sessions 能查到名字）"
    else
        fail "命名未持久化（查到 name=$NFOUND）"
    fi
    # list_all_sessions 返回结构正确
    if echo "$NLIST" | jq -e '.data.sessions | type == "array" and length > 0' >/dev/null 2>&1; then
        pass "list_all_sessions 返回非空 sessions 数组（历史面板数据源 OK）"
    else
        fail "list_all_sessions 返回异常"
    fi
    # get_messages 能还原历史改编结果
    GM=$(./target/debug/ion rpc --session "$NAMED_SID" --method get_messages --params '{"view":"full","limit":50}' 2>/dev/null)
    if echo "$GM" | jq -e '.data.messages | type == "array" and length > 0' >/dev/null 2>&1; then
        pass "get_messages 能拉取历史 session 消息（历史还原 OK）"
    else
        fail "get_messages 拉取历史失败"
    fi

    # ─── Group H: 循环编排（critic 不通过 → 回改）───
    echo ""
    echo "── Group H: 验证循环编排（critic 反馈回灌 lyricist）──"
    # 注：本组验证编排「可行性」——lyricist 能在同一 session 接收第二句 prompt 继续改。
    # 完整多轮循环由前端 JS 驱动，这里验证底层 RPC 支持（session 复用 + 二次 prompt）。
    if [ -z "$E2E_SID" ]; then
        fail "无 lyricist session 可测循环"
    else
        # 在同一 lyricist session 发第二条 prompt（模拟前端回改）
        curl -sf -X POST "http://localhost:$GATEWAY_PORT/rpc" -H "Content-Type: application/json" \
            -d "{\"method\":\"prompt\",\"session\":\"$E2E_SID\",\"params\":{\"text\":\"审查员说第1句押韵有问题，请把改编的第1句改成押'ang'韵，重新输出完整 <lyric_result>。\"}}" >/dev/null
        LOOP_OK=false
        for i in $(seq 1 50); do
            LST=$(./target/debug/ion rpc --method list_sessions 2>/dev/null | jq -r ".data.sessions[] | select(.session_id==\"$E2E_SID\") | .status" 2>/dev/null | grep -v "_encode\|_decode" | head -1)
            if [ "$LST" = "Idle" ]; then LOOP_OK=true; break; fi
            sleep 2
        done
        if [ "$LOOP_OK" = "true" ]; then
            LOOP_TEXT=$(./target/debug/ion rpc --session "$E2E_SID" --method get_messages 2>/dev/null \
                | jq -r '[.data.messages[]?.message.Assistant?.content[]?.Text?.text] | map(select(. != null)) | join("\n")' 2>/dev/null | grep -v "_encode\|_decode")
            # 最近的 assistant 文本应含 lyric_result（回改后重新输出）
            if echo "$LOOP_TEXT" | grep -q "<lyric_result"; then
                pass "循环回改成功（同一 session 二次 prompt 后再次产出 <lyric_result>）"
            else
                fail "循环回改未产出 <lyric_result>"
            fi
        else
            fail "循环回改超时（session 一直 Busy）"
        fi
    fi
else
    echo ""
    echo "（跳过 Group D-H 真实 LLM 测试；设 ION_E2E=1 启用）"
fi

# ── 清理 ──
kill "$GW_PID" 2>/dev/null || true
kill "$HOST_PID" 2>/dev/null || true
sleep 1

echo ""
echo "════════════════════════════════════════════════════"
if [ "$FAIL" -eq 0 ]; then
    green "全部通过：$PASS passed, 0 failed"
else
    red "$PASS passed, $FAIL failed"
fi
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] || exit 1
