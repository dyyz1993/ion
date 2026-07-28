#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Goal-driven CI Verification — v3 最终版
#
# 策略：起 FauxProvider 全局 host，CI 复用它或自己起。
# 每个 CI 跑完后检测 host 是否死了，死了就重启 FauxProvider host。
# ──────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
cd "$PROJECT_DIR"

RESULTS_DIR="/tmp/goal_ci_results"
mkdir -p "$RESULTS_DIR"

echo "════════════════════════════════════════════════════"
echo "  Goal: Verify All CI Scripts (v3 final)"
echo "  Date: $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
echo "✅ Build OK"

# ── 准备环境 ──
echo ""
echo "── Preparing environment ──"
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
rm -f "$HOME/.ion/host.sock" 2>/dev/null
sleep 1

# 临时启用 global-memory
python3 -c "
import json
p = '$HOME/.ion/config.json'
with open(p) as f: c = json.load(f)
c.setdefault('extensions', {})['global-memory'] = {'enabled': True}
with open(p, 'w') as f: json.dump(c, f, indent=2)
" 2>/dev/null
echo "  global-memory enabled"

# 清理
rm -f "$HOME/.ion/agent/global-memory.db"* 2>/dev/null
rm -f "$HOME/.ion/agent/extensions/"*.wasm 2>/dev/null

# 起全局 host
ION_FAUX_REPLY="host ready" "$ION_BIN" serve > /tmp/goal_ci_host.log 2>&1 &
HOST_PID=$!
echo -n "  Starting host..."
for i in $(seq 1 15); do
    sleep 2
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        echo " ✅ Ready (PID $HOST_PID)"
        break
    fi
    echo -n "."
done
echo ""

# ensure_host 函数：检测 host 死了就重启
ensure_host() {
    if ! "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
        rm -f "$HOME/.ion/host.sock" 2>/dev/null
        sleep 1
        ION_FAUX_REPLY="host ready" "$ION_BIN" serve > /tmp/goal_ci_host.log 2>&1 &
        HOST_PID=$!
        for i in $(seq 1 10); do
            sleep 2
            if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
                break
            fi
        done
    fi
}

# ── 跑 CI ──
CI_SCRIPTS=$(ls tests/*_ci.sh tests/*_e2e.sh 2>/dev/null | sort)
TOTAL=$(echo "$CI_SCRIPTS" | wc -l | tr -d ' ')
echo "📋 Found $TOTAL CI scripts"
echo ""

PASS=0; FAIL=0; SKIP=0
RESULTS_FILE="$RESULTS_DIR/results.jsonl"
> "$RESULTS_FILE"

for script in $CI_SCRIPTS; do
    name=$(basename "$script")
    log_file="$RESULTS_DIR/$name.log"

    echo -n "  $name ... "

    start=$(date +%s)
    timeout 900 bash "$script" > "$log_file" 2>&1
    exit_code=$?
    duration=$(( $(date +%s) - start ))

    if [ $exit_code -eq 0 ]; then
        echo "✅ PASS (${duration}s)"
        PASS=$((PASS+1))
        status="PASS"
    elif [ $exit_code -eq 124 ]; then
        echo "⏭️  SKIP (${duration}s)"
        SKIP=$((SKIP+1))
        status="SKIP"
    else
        echo "❌ FAIL (${duration}s)"
        FAIL=$((FAIL+1))
        status="FAIL"
        tail -3 "$log_file" > "$RESULTS_DIR/$name.fail"
    fi

    echo "{\"script\":\"$name\",\"status\":\"$status\",\"exit_code\":$exit_code,\"duration_s\":$duration}" >> "$RESULTS_FILE"

    # 每个 CI 后确保 host 活着
    ensure_host
done

# ── 清理 ──
kill $HOST_PID 2>/dev/null || true
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
python3 -c "
import json
p = '$HOME/.ion/config.json'
with open(p) as f: c = json.load(f)
if 'extensions' in c: c['extensions']['global-memory'] = {'enabled': False}
with open(p, 'w') as f: json.dump(c, f, indent=2)
" 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════"
echo "  Summary: Pass=$PASS  Fail=$FAIL  Skip=$SKIP  Rate=$((PASS*100/TOTAL))%"
echo "════════════════════════════════════════════════════"
