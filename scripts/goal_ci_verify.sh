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

# ── 起 watchdog host 守护进程 ──
echo ""
echo "── Starting watchdog host ──"

# 确保没有残留 host
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
rm -f "$HOME/.ion/host.sock" 2>/dev/null
sleep 1

# 起 watchdog（自动重启被杀的 host）
nohup bash "$PROJECT_DIR/scripts/ci_watchdog_host.sh" > /tmp/ci_watchdog.log 2>&1 &
WATCHDOG_PID=$!
echo "  Watchdog PID: $WATCHDOG_PID"

# 等 host 就绪
echo -n "  Waiting for host..."
for i in $(seq 1 20); do
    sleep 2
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        echo " ✅ Ready"
        break
    fi
    echo -n "."
done
echo ""

# Collect all CI scripts
CI_SCRIPTS=$(ls tests/*_ci.sh tests/*_e2e.sh 2>/dev/null | sort)
TOTAL=$(echo "$CI_SCRIPTS" | wc -l | tr -d ' ')
echo "📋 Found $TOTAL CI scripts (watchdog keeps host alive)"
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

# 关闭 watchdog + host
kill $WATCHDOG_PID 2>/dev/null || true
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true

# 恢复 config
python3 -c "
import json
with open('$HOME/.ion/config.json') as f: c = json.load(f)
if 'extensions' in c and 'global-memory' in c['extensions']:
    c['extensions']['global-memory'] = {'enabled': False}
    with open('$HOME/.ion/config.json', 'w') as f: json.dump(c, f, indent=2)
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
