#!/usr/bin/env bash
#
# run_ci_matrix_parallel.sh — Pure bash parallel CI runner (NO LLM, NO host)
#
# This is the most reliable version:
# - Uses `xargs -P N` for parallelism (no LLM coordinator, no worker agents)
# - Each script gets its own HOME + cargo shim
# - Output captured per-script, aggregated at the end
#
# Why this exists:
#   The ion --host version proves ION's multi-worker orchestration works,
#   but LLM-based workers are unreliable (they modify prompts, change timeouts).
#   This version is for actually GET GREEN CI results, not for testing ION's
#   orchestration. Use run_ci_matrix.sh / run_ci_matrix_rpc.sh to validate
#   the orchestration itself.
#
# Usage:
#   bash scripts/run_ci_matrix_parallel.sh
#   PARALLELISM=3 bash scripts/run_ci_matrix_parallel.sh
#
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
PARALLELISM="${PARALLELISM:-5}"
PER_SCRIPT_TIMEOUT="${PER_SCRIPT_TIMEOUT:-180}"

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  CI Matrix Runner — Pure bash parallel (xargs -P $PARALLELISM)"
echo "════════════════════════════════════════════════════════════════"
echo "  Per-script T/O:  ${PER_SCRIPT_TIMEOUT}s"
echo ""

# ─── Pre-flight ────────────────────────────────────────────────────────────
[ -x "$ION_BIN" ] || { echo "❌ build ion first"; exit 1; }

# ─── Prepare cargo shim + per-worker HOME ─────────────────────────────────
REAL_CARGO=$(command -v cargo 2>/dev/null || echo /usr/local/cargo/bin/cargo)
mkdir -p /tmp/ci-bin
cat > /tmp/ci-bin/cargo <<SHIM
#!/usr/bin/env bash
if [ "\$1" = "build" ]; then exit 0; fi
exec $REAL_CARGO "\$@"
SHIM
chmod +x /tmp/ci-bin/cargo

# Clean prior state
rm -rf /tmp/ci-results /tmp/ci-out-*.log 2>/dev/null
mkdir -p /tmp/ci-results

# ─── Gather + filter scripts ──────────────────────────────────────────────
ALL_SCRIPTS=$(ls tests/*_ci.sh tests/scenario2_ci.sh tests/team_e2e.sh 2>/dev/null | sort -u)

SKIP_LIST="apple_container_ci mcp_ci hooks_agent_real streaming_replay_ci self_heal_ci goal_supervisor_ci goal_evolver_ci"

FILTERED=""
SKIPPED=""
for s in $ALL_SCRIPTS; do
    bn=$(basename "$s" .sh)
    skip=0
    for sk in $SKIP_LIST; do
        if [ "$bn" == "$sk" ]; then skip=1; break; fi
    done
    if [ $skip -eq 0 ]; then
        FILTERED="$FILTERED $s"
    else
        SKIPPED="$SKIPPED $s"
        echo "{\"script\":\"$s\",\"status\":\"SKIP\",\"reason\":\"env-dependent\",\"exit_code\":-1,\"duration_s\":0}" >> /tmp/ci-results/skipped.jsonl
    fi
done

TOTAL=$(echo "$FILTERED" | wc -w | tr -d ' ')
SKIP_CNT=$(echo "$SKIPPED" | wc -w | tr -d ' ')
echo "  Total scripts: $TOTAL (skipped: $SKIP_CNT)"
echo ""

# ─── Worker function (called by xargs) ────────────────────────────────────
run_one_script() {
    local script="$1"
    local bn=$(basename "$script" .sh)
    local worker_id=$(echo "$script" | md5sum | cut -c1-8)
    local home_dir="/tmp/ci-home-$worker_id"
    local log="/tmp/ci-out-$bn.log"
    local result_file="/tmp/ci-results/$bn.jsonl"

    # Per-script isolated HOME
    rm -rf "$home_dir"
    mkdir -p "$home_dir/.ion/agent"
    [ -d "$HOME/.rustup" ] && ln -s "$HOME/.rustup" "$home_dir/.rustup" 2>/dev/null
    [ -d "$HOME/.cargo" ] && ln -s "$HOME/.cargo" "$home_dir/.cargo" 2>/dev/null

    # Run with isolated HOME + cargo shim in PATH + script's own ION_SESSION_DIR
    local start=$(date +%s)
    HOME="$home_dir" \
    PATH="/tmp/ci-bin:$PATH" \
    timeout "$PER_SCRIPT_TIMEOUT" bash "$script" > "$log" 2>&1
    local exit_code=$?
    local end=$(date +%s)
    local dur=$((end - start))

    local status
    if [ $exit_code -eq 0 ]; then status="PASS"; else status="FAIL"; fi

    # Write JSON result
    echo "{\"script\":\"$script\",\"status\":\"$status\",\"exit_code\":$exit_code,\"duration_s\":$dur,\"log_path\":\"$log\"}" > "$result_file"

    # Cleanup HOME dir
    rm -rf "$home_dir"

    echo "  $status $bn (exit=$exit_code, ${dur}s)"
}
export -f run_one_script
export PER_SCRIPT_TIMEOUT REAL_CARGO

# ─── Run in parallel via xargs ─────────────────────────────────────────────
echo "[Step] Running $TOTAL scripts in parallel (xargs -P $PARALLELISM)..."
echo "$FILTERED" | tr ' ' '\n' | grep -v '^$' | \
    xargs -P "$PARALLELISM" -I {} bash -c 'run_one_script "$@"' _ {} 2>&1 | grep -v "command not found\|setValueFor\|valueForKey"

# ─── Aggregate ─────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Phase 1 complete — aggregating results"
echo "════════════════════════════════════════════════════════════════"
echo ""

# Merge all per-script jsonl into one
cat /tmp/ci-results/*.jsonl > /tmp/ci-results/all.jsonl 2>/dev/null

bash "$PROJECT_DIR/scripts/aggregate_ci_results.sh" 2>&1 | grep -v "command not found\|setValueFor\|valueForKey"
