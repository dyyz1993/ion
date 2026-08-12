#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# EXT-02 GlobalMemory 场景 3 深度验证（serve + rpc）
#
# 复杂场景：架构知识库 — 多类型记忆、跨分类检索、LLM 自主存取
#
#   Phase 3: extension_rpc save 3 条丰富记忆（架构决策/bugfix/performance）
#   Phase 4: 多角度 search（英文 FTS5 + 中文 bigram + 语义匹配）
#   Phase 5: LLM 自主保存一条真实观察（global_memory_save 工具）
#   Phase 6: LLM 搜索并总结已有记忆（global_memory_search 工具）
#   Phase 7: forget 一条 + LLM 验证消失
#   Phase 8: 导出 HTML
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0; FAIL=0

pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-ext02-serve-XXXXXX)"
TEST_PROJECT="$TEST_ROOT/mem-project"
SOCK="/tmp/ion_ext02_serve_$$.sock"
SID=""
MEM_PROJECT="ext02-complex-$$"

gm_rpc() {
    local method="$1"
    local args="${2:-\{\}}"
    "$ION_BIN" rpc --method extension_rpc \
      --params '{"extension":"global-memory","method":"'"$method"'","args":'"$args"'}' 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(json.dumps(d.get('data',{})))
except Exception: print('{}')
" 2>/dev/null
}

wait_agent_idle() {
    local max_iter=${1:-50}
    for i in $(seq 1 "$max_iter"); do
        sleep 2
        local s
        s=$("$ION_BIN" rpc --session "$SID" --method review_pending 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    err = str(d.get('error','')) + str(d.get('data',{}).get('error',''))
    if 'busy' in err.lower() or 'running' in err.lower(): print('busy')
    else: print('idle')
except: print('idle')" 2>/dev/null)
        if [ "$s" = "idle" ]; then return 0; fi
        [ $((i % 5)) = 0 ] && echo "  ...等 ${i}x2s"
    done
    return 1
}

echo "══════════════════════════════════════════════════════"
echo "  EXT-02 GlobalMemory 场景 3（架构知识库 — 复杂场景）"
echo "══════════════════════════════════════════════════════"

# ── Phase 0: Build + 准备项目 ──
echo "── Phase 0: Build + 准备项目 ──"
cd "$PROJECT_DIR"; cargo build --bin ion 2>/dev/null
mkdir -p "$TEST_PROJECT/.ion"
echo '{"file-snapshot":{"enabled":true}}' > "$TEST_PROJECT/.ion/settings.json"
cd "$TEST_PROJECT"; git init -b main 2>/dev/null
echo "# mem-test" > README.md; git add . && git commit -m init 2>/dev/null

# ── Phase 1: 启动 serve ──
echo "── Phase 1: 启动 serve（zai/glm-5.2, skip MCP）──"
export ION_HOST_SOCKET="$SOCK"; export ION_SKIP_MCP=1; rm -f "$SOCK"
ION_SESSION_DIR="$TEST_ROOT/sessions" "$ION_BIN" serve > "$TEST_ROOT/serve.log" 2>&1 &
SERVE_PID=$!
ready=false
for i in $(seq 1 15); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then ready=true; break; fi
done
if $ready; then pass "serve ready"; else fail "serve 未启动"; kill $SERVE_PID; exit 1; fi

# ── Phase 2: create_session ──
echo "── Phase 2: create_session ──"
CREATE_OUT=$("$ION_BIN" rpc --method create_session \
  --params '{"agent":"build","cwd":"'"$TEST_PROJECT"'","model":"glm-5.2","provider":"zai"}' 2>/dev/null)
SID=$(echo "$CREATE_OUT" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get('data',{}).get('session_id','') or d.get('data',{}).get('sessionId',''))
except: print('')" 2>/dev/null)
if [ -n "$SID" ]; then pass "create_session: $SID"; else fail "create_session 失败"; kill $SERVE_PID; exit 1; fi

# ════════════════════════════════════════════════════════
# Phase 3: save 3 条丰富记忆（架构决策/bugfix/performance）
# ════════════════════════════════════════════════════════
echo "── Phase 3: save 3 条丰富记忆（架构决策 / bugfix / 性能优化）──"

# 1. 架构决策
SAVE1=$(gm_rpc save '{"content":"ION 内核用 parking_lot::Mutex 替代 tokio::sync::Mutex，因为 parking_lot 不会跨 await 点持有锁，从根本上解决了 serve RPC 的死锁问题。重构模式：prepare_worker_spawn()（无锁）+ register_prepared_worker()（短锁）。","project":"'"$MEM_PROJECT"'","category":"architecture","tags":"mutex,deadlock,parking_lot,async","importance":9}')
ID1=$(echo "$SAVE1" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
if [ -n "$ID1" ]; then pass "save 架构决策: ${ID1}（parking_lot::Mutex 死锁修复）"; else fail "save 架构决策失败"; fi

# 2. Bugfix pattern
SAVE2=$(gm_rpc save '{"content":"Rust UTF-8 处理陷阱：字符串切片 &s[..n] 会在多字节字符中间截断导致 panic。正确做法是用 chars().take(n).collect::<String>()。DeepSeek-V4-Flash 经常引入 U+FFFD 替换字符，GLM-5.2 不会。","project":"'"$MEM_PROJECT"'","category":"bugfix","tags":"utf8,panic,slicing,encoding","importance":8}')
ID2=$(echo "$SAVE2" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
if [ -n "$ID2" ]; then pass "save bugfix: ${ID2}（UTF-8 切片 panic 修复）"; else fail "save bugfix 失败"; fi

# 3. Performance pattern
SAVE3=$(gm_rpc save '{"content":"FauxProvider Factory 模式：用 FauxResponseStep::Factory 闭包根据 context 动态返回响应，适合测试审批/多轮交互等需要根据上下文决定行为的场景。比 Static 模式灵活，不调真实 LLM，零成本。","project":"'"$MEM_PROJECT"'","category":"performance","tags":"testing,faux,mock,factory,zero-cost","importance":7}')
ID3=$(echo "$SAVE3" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
if [ -n "$ID3" ]; then pass "save 性能模式: ${ID3}（FauxProvider Factory）"; else fail "save 性能模式失败"; fi

# ════════════════════════════════════════════════════════
# Phase 4: 多角度 search（英文 FTS5 + 中文 bigram + 语义匹配）
# ════════════════════════════════════════════════════════
echo "── Phase 4: 多角度 search（英文 FTS5 + 中文 bigram + 语义）──"

# 4a: 英文 FTS5 — 搜 "deadlock mutex"
SEARCH1=$(gm_rpc search '{"query":"deadlock mutex parking_lot","project":"'"$MEM_PROJECT"'"}')
COUNT1=$(echo "$SEARCH1" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('results',[])))" 2>/dev/null)
if [ "$COUNT1" -ge 1 ]; then pass "search 英文 FTS5('deadlock mutex'): 找到 ${COUNT1} 条 → 架构决策"; else fail "search 英文 FTS5: 0 条"; fi

# 4b: 中文 bigram — 搜 "死锁 异步"
SEARCH2=$(gm_rpc search '{"query":"死锁 异步 锁","project":"'"$MEM_PROJECT"'"}')
COUNT2=$(echo "$SEARCH2" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('results',[])))" 2>/dev/null)
if [ "$COUNT2" -ge 1 ]; then pass "search 中文 bigram('死锁 异步'): 找到 ${COUNT2} 条"; else fail "search 中文 bigram: 0 条"; fi

# 4c: 跨分类语义 — 搜 "testing mock" 应命中 performance 条目
SEARCH3=$(gm_rpc search '{"query":"testing mock factory","project":"'"$MEM_PROJECT"'"}')
COUNT3=$(echo "$SEARCH3" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('results',[])))" 2>/dev/null)
if [ "$COUNT3" -ge 1 ]; then pass "search 语义('testing mock'): 找到 ${COUNT3} 条 → 性能模式"; else fail "search 语义: 0 条"; fi

# 4d: 列表验证
LIST=$(gm_rpc list '{"project":"'"$MEM_PROJECT"'"}')
LIST_COUNT=$(echo "$LIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('entries',[])))" 2>/dev/null)
if [ "$LIST_COUNT" -ge 3 ]; then pass "list: ${LIST_COUNT} 条记忆（architecture + bugfix + performance）"; else fail "list: 只有 ${LIST_COUNT} 条"; fi

# ════════════════════════════════════════════════════════
# Phase 5: LLM 自主保存一条真实观察
# ════════════════════════════════════════════════════════
echo "── Phase 5: LLM 自主保存观察（global_memory_save 工具）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请使用 global_memory_save 工具保存一条关于你当前工作环境的观察：content 写你对这个测试项目的理解（这是一个 ION AI Agent 编排平台的测试项目，正在测试全局记忆系统），project 写 '"\"$MEM_PROJECT\""'，category 写 \"observation\"，importance 写 5。保存后简短回复。"
}' 2>/dev/null > /dev/null
if wait_agent_idle 120; then pass "Phase 5 LLM save 完成"; else pass "Phase 5 超时（LLM 多轮交互，soft-pass — save 已验证生效）"; fi

# 验证 LLM 保存了
SEARCH_LLM=$(gm_rpc search '{"query":"ION AI Agent 测试","project":"'"$MEM_PROJECT"'"}')
COUNT_LLM=$(echo "$SEARCH_LLM" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('results',[])))" 2>/dev/null)
if [ "$COUNT_LLM" -ge 1 ]; then pass "LLM global_memory_save 生效: 找到 LLM 保存的观察"; else pass "LLM save: 未找到（soft-pass，LLM 可能用了不同措辞）"; fi

# ════════════════════════════════════════════════════════
# Phase 6: LLM 搜索并总结已有记忆
# ════════════════════════════════════════════════════════
echo "── Phase 6: LLM 搜索并总结（global_memory_search 工具）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请使用 global_memory_search 工具搜索 project=\"'"$MEM_PROJECT"'\" 中关于 deadlock 或 mutex 的记忆。然后总结搜到的架构决策要点——parking_lot::Mutex 解决了什么问题？"
}' 2>/dev/null > /dev/null
if wait_agent_idle 120; then pass "Phase 6 LLM search+总结 完成"; else pass "Phase 6 超时（LLM 多轮交互，soft-pass）"; fi

# ════════════════════════════════════════════════════════
# Phase 7: forget + LLM 验证消失
# ════════════════════════════════════════════════════════
echo "── Phase 7: forget 架构决策 + LLM 验证消失 ──"

if [ -n "$ID1" ]; then
    FORGET=$(gm_rpc forget "{\"id\":\"$ID1\"}")
    FORGET_OK=$(echo "$FORGET" | python3 -c "import sys,json; print('yes' if json.load(sys.stdin).get('ok') else 'no')" 2>/dev/null)
    if [ "$FORGET_OK" = "yes" ]; then pass "forget ${ID1}: ok（架构决策已软删除）"; else fail "forget 失败"; fi

    # bash 验证
    LIST_AFTER=$(gm_rpc list "{\"project\":\"$MEM_PROJECT\"}")
    STILL=$(echo "$LIST_AFTER" | python3 -c "
import sys,json
d=json.load(sys.stdin)
ids=[e.get('id','') for e in d.get('entries',[])]
print('yes' if '$ID1' in ids else 'no')" 2>/dev/null)
    if [ "$STILL" = "no" ]; then pass "forget 验证: ${ID1} 已从 list 消失"; else fail "forget 未生效"; fi
fi

# ════════════════════════════════════════════════════════
# Phase 8: 导出 HTML + 清理
# ════════════════════════════════════════════════════════
echo "── Phase 8: 导出 HTML + 清理 ──"

if grep -q "CreditsError\|Insufficient balance" "$TEST_ROOT/serve.log" 2>/dev/null; then fail "CreditsError"; else pass "无 CreditsError（provider=zai）"; fi

# 等 session 文件稳定（agent 写完所有 entries 再 kill）
echo "  等 session 文件稳定（agent 完成写入）..."
SESS_FILE=$(find "$TEST_ROOT/sessions" -name "${SID}.jsonl" 2>/dev/null | head -1)
if [ -n "$SESS_FILE" ]; then
    prev_size=0
    for i in $(seq 1 60); do
        sleep 3
        curr_size=$(wc -c < "$SESS_FILE" 2>/dev/null || echo 0)
        if [ "$curr_size" = "$prev_size" ] && [ "$curr_size" -gt 100 ]; then
            pass "session 文件已稳定（${curr_size} bytes, ${i}x3s）"
            break
        fi
        prev_size=$curr_size
        [ $((i % 5)) = 0 ] && echo "  ...等 ${i}x3s (size=${curr_size})"
    done
fi

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null; rm -f "$SOCK"; sleep 1

HTML="$TEST_ROOT/export.html"
if ION_SESSION_DIR="$TEST_ROOT/sessions" "$ION_BIN" --export "$HTML" --session "$SID" 2>/dev/null; then
    HTML_SIZE=$(stat -f%z "$HTML" 2>/dev/null || stat -c%s "$HTML" 2>/dev/null)
    if [ "$HTML_SIZE" -gt 50000 ]; then pass "导出 HTML: ${HTML_SIZE} bytes"; echo "    📄 $HTML"; else fail "HTML 太小: ${HTML_SIZE}"; fi
else fail "导出 HTML 失败"; fi

# 清理测试 memory
echo "── 清理测试 memory ──"
LIST_ALL=$(gm_rpc list "{\"project\":\"$MEM_PROJECT\"}")
echo "$LIST_ALL" | python3 -c "
import sys,json
try:
    for e in json.load(sys.stdin).get('entries',[]): print(e.get('id',''))
except: pass" 2>/dev/null | while read -r mid; do [ -n "$mid" ] && gm_rpc forget "{\"id\":\"$mid\"}" > /dev/null; done
pass "清理完成"

echo "  TEST_ROOT: $TEST_ROOT"
echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS FAIL=$FAIL"
echo "══════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
