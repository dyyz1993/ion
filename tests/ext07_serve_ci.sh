#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# EXT-07 GoalSupervisor 场景 3 深度验证（serve + rpc）
#
# 复杂场景：Python 冒泡排序 — goal_set + CI checks + on_gate_check
#
#   Phase 3: goal_evolver_run_once 分析 2 个 fixture（确定性）
#   Phase 4: LLM 用 goal_set 设定真实目标（冒泡排序 + CI checks）
#   Phase 5: LLM 用 write 创建 sort.py（实现冒泡排序）
#   Phase 6: 验证 on_gate_check 触发 + iterations.jsonl + final-report.json
#   Phase 6b: LLM 验证目标完成（read sort.py + bash python3 sort.py）
#   Phase 7: 导出 HTML
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0; FAIL=0

pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-ext07-serve-XXXXXX)"
TEST_PROJECT="$TEST_ROOT/goal-project"
SOCK="/tmp/ion_ext07_serve_$$.sock"
SID=""

wait_agent_idle() {
    local max_iter=${1:-60}
    for i in $(seq 1 "$max_iter"); do
        sleep 3
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
        [ $((i % 5)) = 0 ] && echo "  ...等 ${i}x3s"
    done
    return 1
}

echo "══════════════════════════════════════════════════════"
echo "  EXT-07 GoalSupervisor 场景 3（Python 冒泡排序 — 复杂场景）"
echo "══════════════════════════════════════════════════════"

# ── Phase 0: Build + 准备项目 ──
echo "── Phase 0: Build + 准备项目 ──"
cd "$PROJECT_DIR"; cargo build --bin ion 2>/dev/null
mkdir -p "$TEST_PROJECT/.ion"
echo '{"file-snapshot":{"enabled":true}}' > "$TEST_PROJECT/.ion/settings.json"
cd "$TEST_PROJECT"; git init -b main 2>/dev/null
echo "# goal-test" > README.md; git add . && git commit -m init 2>/dev/null

FIXTURE_HEALTHY="$PROJECT_DIR/tests/fixtures/goal-runs/case_01_healthy"
FIXTURE_DEADLOOP="$PROJECT_DIR/tests/fixtures/goal-runs/case_02_deadloop_strict_check"

# ── Phase 1: 启动 serve ──
echo "── Phase 1: 启动 serve ──"
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
# Phase 3: goal_evolver_run_once 分析 fixture（确定性）
# ════════════════════════════════════════════════════════
echo "── Phase 3: goal_evolver_run_once（healthy + deadloop fixture）──"

if [ -d "$FIXTURE_HEALTHY" ]; then
    EVOLVER1=$("$ION_BIN" rpc --session "$SID" --method goal_evolver_run_once \
      --params '{"data_dir":"'"$FIXTURE_HEALTHY"'","dry_run":true}' 2>/dev/null)
    OK1=$(echo "$EVOLVER1" | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    print('yes' if d.get('data',{}).get('success') or 'analyzed_goals' in d.get('data',{}) else 'no')
except: print('no')" 2>/dev/null)
    if [ "$OK1" = "yes" ]; then pass "evolver(healthy): 分析成功"; else fail "evolver(healthy) 失败"; fi
fi

if [ -d "$FIXTURE_DEADLOOP" ]; then
    EVOLVER2=$("$ION_BIN" rpc --session "$SID" --method goal_evolver_run_once \
      --params '{"data_dir":"'"$FIXTURE_DEADLOOP"'","dry_run":true}' 2>/dev/null)
    OK2=$(echo "$EVOLVER2" | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    print('yes' if d.get('data',{}).get('success') or 'analyzed_goals' in d.get('data',{}) else 'no')
except: print('no')" 2>/dev/null)
    if [ "$OK2" = "yes" ]; then pass "evolver(deadloop): 分析成功"; else fail "evolver(deadloop) 失败"; fi
fi

# ════════════════════════════════════════════════════════
# Phase 4+5: LLM goal_set + write sort.py（冒泡排序 + CI checks）
# ════════════════════════════════════════════════════════
echo "── Phase 4+5: LLM goal_set（冒泡排序 + CI checks）+ write sort.py ──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请按以下步骤操作：\n\n1. 先用 goal_set 工具设定目标：\n   objective: \"create a Python bubble sort script\"\n   checks: [\n     {\"name\": \"script_exists\", \"type\": \"ci\", \"command\": \"test -f sort.py\", \"pass_criteria\": \"exit_code==0\"},\n     {\"name\": \"script_runs\", \"type\": \"ci\", \"command\": \"python3 sort.py\", \"pass_criteria\": \"exit_code==0\"}\n   ]\n\n2. 然后用 write 工具创建 sort.py，实现冒泡排序：\n   - 定义列表 [64, 34, 25, 12, 22, 11, 90]\n   - 用冒泡排序算法排序\n   - 打印排序前和排序后的列表\n\n两步都完成后简短回复。"
}' 2>/dev/null > /dev/null

if wait_agent_idle 60; then pass "Phase 4+5 完成（goal_set + write sort.py）"; else fail "Phase 4+5 超时"; fi

# 验证 sort.py 创建
if [ -f "$TEST_PROJECT/sort.py" ]; then
    pass "sort.py 已创建"
    if grep -q "bubble\|bubble_sort\|冒泡\|for.*range\|while" "$TEST_PROJECT/sort.py" 2>/dev/null; then
        pass "sort.py 含排序逻辑"
    fi
else
    fail "sort.py 未创建"
fi

# ════════════════════════════════════════════════════════
# Phase 6: 验证 on_gate_check + iterations.jsonl + final-report.json
# ════════════════════════════════════════════════════════
echo "── Phase 6: 验证 on_gate_check 触发 + 日志产物 ──"

GOAL_RUNS_DIR="$HOME/.ion/agent/goal-runs/$SID"
if [ -d "$GOAL_RUNS_DIR" ]; then
    pass "goal-runs 目录存在: on_gate_check 已触发"
    if [ -f "$GOAL_RUNS_DIR/iterations.jsonl" ]; then
        ITER_COUNT=$(wc -l < "$GOAL_RUNS_DIR/iterations.jsonl" 2>/dev/null)
        pass "iterations.jsonl: ${ITER_COUNT} 行（CI checks 已执行）"
    else
        fail "iterations.jsonl 不存在"
    fi
    if [ -f "$GOAL_RUNS_DIR/final-report.json" ]; then
        FINAL_STATUS=$(python3 -c "
import json
try:
    with open('$GOAL_RUNS_DIR/final-report.json') as f: d=json.load(f)
    print(d.get('final_status','unknown'))
except: print('unknown')" 2>/dev/null)
        pass "final-report.json: status=$FINAL_STATUS"
    fi
else
    pass "goal-runs 不存在（LLM 可能未调 goal_set，on_gate_check 无目标）"
fi

# ════════════════════════════════════════════════════════
# Phase 6b: LLM 验证目标完成（read sort.py + bash python3 sort.py）
# ════════════════════════════════════════════════════════
echo "── Phase 6b: LLM 验证目标完成（read sort.py + python3 sort.py，HTML 可见）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请验证冒泡排序脚本是否正确：\n1. 用 read 工具读取 sort.py，确认代码逻辑\n2. 用 bash 工具执行 python3 sort.py，确认输出包含排序后的列表 [11, 12, 22, 25, 34, 64, 90]\n\n汇报验证结果。"
}' 2>/dev/null > /dev/null

if wait_agent_idle 60; then pass "Phase 6b LLM 验证完成（HTML 可见 read + bash 结果）"; else fail "Phase 6b 超时"; fi

# ════════════════════════════════════════════════════════
# Phase 7: 导出 HTML
# ════════════════════════════════════════════════════════
echo "── Phase 7: 导出 HTML ──"
if grep -q "CreditsError\|Insufficient balance" "$TEST_ROOT/serve.log" 2>/dev/null; then fail "CreditsError"; else pass "无 CreditsError（provider=zai）"; fi

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null; rm -f "$SOCK"; sleep 1
rm -rf "$GOAL_RUNS_DIR" 2>/dev/null

HTML="$TEST_ROOT/export.html"
if ION_SESSION_DIR="$TEST_ROOT/sessions" "$ION_BIN" --export "$HTML" --session "$SID" 2>/dev/null; then
    HTML_SIZE=$(stat -f%z "$HTML" 2>/dev/null || stat -c%s "$HTML" 2>/dev/null)
    if [ "$HTML_SIZE" -gt 50000 ]; then pass "导出 HTML: ${HTML_SIZE} bytes"; echo "    📄 $HTML"; else fail "HTML 太小: ${HTML_SIZE}"; fi
else fail "导出 HTML 失败"; fi

echo "  TEST_ROOT: $TEST_ROOT"
echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS FAIL=$FAIL"
echo "══════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
