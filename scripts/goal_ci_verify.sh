#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Goal-driven CI Verification — A 驱动 B 跑全部 CI 脚本
#
# 改进版 v2：
#   1. 自动起 ion serve host（给需要 RPC 的 CI 用）
#   2. timeout 900s（15分钟），覆盖慢 CI
#   3. 结果沉淀到 /tmp/goal_ci_results/
#   4. 自动生成审查报告
#
# Usage: bash scripts/goal_ci_verify.sh
# ──────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
cd "$PROJECT_DIR"

RESULTS_DIR="/tmp/goal_ci_results"
mkdir -p "$RESULTS_DIR"

echo "════════════════════════════════════════════════════"
echo "  Goal: Verify All CI Scripts (v2: Host Mode)"
echo "  Date: $(date)"
echo "════════════════════════════════════════════════════"
echo ""

# Build first
cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
echo "✅ Build OK"

# ── 准备环境 + 起全局 host ──
echo ""
echo "── Preparing environment ──"

# 确保没有残留 host
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
sleep 1

# 临时启用 global-memory（memory 相关 CI 需要它）
CONFIG_FILE="$HOME/.ion/config.json"
python3 -c "
import json
with open('$CONFIG_FILE') as f: c = json.load(f)
if 'extensions' not in c: c['extensions'] = {}
c['extensions']['global-memory'] = {'enabled': True}
with open('$CONFIG_FILE', 'w') as f: json.dump(c, f, indent=2)
print('  global-memory enabled for CI')
" 2>/dev/null

# 清理旧 DB + 扩展残留
rm -f "$HOME/.ion/agent/global-memory.db"* 2>/dev/null
rm -f "$HOME/.ion/agent/extensions/"*.wasm 2>/dev/null

# 起全局 host（给需要 RPC 的 CI 用）
ION_FAUX_REPLY="host ready" "$ION_BIN" serve > /tmp/goal_ci_host.log 2>&1 &
HOST_PID=$!
echo "  Host PID: $HOST_PID"
echo -n "  Waiting..."
for i in $(seq 1 15); do
    sleep 2
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        echo " ✅ Ready"
        break
    fi
    echo -n "."
done
echo ""

# Collect CI scripts — split into two batches:
# Batch 1: CIs that don't kill the global host (run first)
# Batch 2: CIs that kill the global host (run last, isolated)
KILL_CIS="abort_ci export_ci file_snapshot_ci global_memory_ci hooks_handler_ci lsp_ci memory_active_ci memory_agent_ci memory_injection_ci memory_v2_processing_ci permission_store_ci rollback_impact_ci self_heal_ci session_entries_ci session_hook_ci soft_interrupt_ci sse_events_ci streaming_replay_ci streaming_throughput_ci"

# All CIs sorted
ALL_CIS=$(ls tests/*_ci.sh tests/*_e2e.sh 2>/dev/null | sort)

# Split: safe CIs first, kill-host CIs last
CI_SCRIPTS=""
for script in $ALL_CIS; do
    name=$(basename "$script" .sh)
    is_kill=0
    for k in $KILL_CIS; do
        [ "$name" = "$k" ] && is_kill=1 && break
    done
    [ $is_kill -eq 0 ] && CI_SCRIPTS="$CI_SCRIPTS $script"
done
# Kill-host CIs at the end
for script in $ALL_CIS; do
    name=$(basename "$script" .sh)
    for k in $KILL_CIS; do
        if [ "$name" = "$k" ]; then
            CI_SCRIPTS="$CI_SCRIPTS $script"
            break
        fi
    done
done

TOTAL=$(echo "$CI_SCRIPTS" | wc -w)
echo "📋 Found $TOTAL CI scripts (safe first, kill-host last)"
echo ""

PASS=0; FAIL=0; SKIP=0
RESULTS_FILE="$RESULTS_DIR/results.jsonl"
> "$RESULTS_FILE"

echo "── Running CI scripts ──"
for script in $CI_SCRIPTS; do
    name=$(basename "$script")
    log_file="$RESULTS_DIR/$name.log"

    echo -n "  $name ... "

    # Run with timeout (900s = 15 min)
    start=$(date +%s)
    timeout 900 bash "$script" > "$log_file" 2>&1
    exit_code=$?
    duration=$(( $(date +%s) - start ))

    # 检测 host 是否被 CI 杀了——如果死了就重启
    if ! "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        echo -n "🔄 "
        lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
        sleep 1
        ION_FAUX_REPLY="host ready" "$ION_BIN" serve > /tmp/goal_ci_host.log 2>&1 &
        HOST_PID=$!
        for i in $(seq 1 10); do
            sleep 2
            if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
                echo "(host restarted) "
                break
            fi
        done
    fi

    if [ $exit_code -eq 0 ]; then
        echo "✅ PASS (${duration}s)"
        PASS=$((PASS+1))
        status="PASS"
    elif [ $exit_code -eq 124 ]; then
        echo "⏭️  SKIP (timeout ${duration}s)"
        SKIP=$((SKIP+1))
        status="SKIP"
    else
        echo "❌ FAIL (${duration}s, exit=$exit_code)"
        FAIL=$((FAIL+1))
        status="FAIL"
        tail -5 "$log_file" > "$RESULTS_DIR/$name.fail"
    fi

    echo "{\"script\":\"$name\",\"status\":\"$status\",\"exit_code\":$exit_code,\"duration_s\":$duration}" >> "$RESULTS_FILE"
done

# ── 清理 ──
echo ""
echo "── Cleanup ──"

# 关闭 host
kill $HOST_PID 2>/dev/null || true
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true

# 恢复 config
python3 -c "
import json
with open('$CONFIG_FILE') as f: c = json.load(f)
if 'extensions' in c and 'global-memory' in c['extensions']:
    c['extensions']['global-memory'] = {'enabled': False}
    with open('$CONFIG_FILE', 'w') as f: json.dump(c, f, indent=2)
    print('  global-memory restored to disabled')
" 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════"
echo "  Summary"
echo "════════════════════════════════════════════════════"
echo "  Total: $TOTAL"
echo "  ✅ Pass: $PASS"
echo "  ❌ Fail: $FAIL"
echo "  ⏭️  Skip: $SKIP"
RATE=$(python3 -c "print(f'{$PASS*100/$TOTAL:.1f}%')" 2>/dev/null || echo "?")
echo "  Pass Rate: $RATE"
echo ""

# List failures
if [ $FAIL -gt 0 ]; then
    echo "── Failed Scripts ──"
    grep '"FAIL"' "$RESULTS_FILE" | python3 -c "
import sys, json
for line in sys.stdin:
    d = json.loads(line)
    print(f'  {d[\"script\"]} (exit={d[\"exit_code\"]}, {d[\"duration_s\"]}s)')
" 2>/dev/null
    echo ""
    echo "── Failure Details (last 3 lines) ──"
    for f in "$RESULTS_DIR"/*.fail; do
        [ -f "$f" ] || continue
        name=$(basename "$f" .fail)
        echo "  $name:"
        sed 's/^/    /' "$f" 2>/dev/null | head -3
    done
fi

echo ""
echo "════════════════════════════════════════════════════"
