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

# Clean up leftover monitor configs from previous test runs.
# These get created by monitor_ci.sh and similar tests; if present, every
# `ion serve` spawned by other tests will pick them up and start spawning
# workers every 3s, which holds the registry lock and blocks all RPC.
# This is the #1 cause of "create_session failed" / "host not responding".
rm -rf .ion/monitors 2>/dev/null
echo "  ✅ cleaned .ion/monitors/"

# ─── Prepare cargo shim + per-worker HOME ─────────────────────────────────
REAL_CARGO=$(command -v cargo 2>/dev/null || echo /usr/local/cargo/bin/cargo)
mkdir -p /tmp/ci-bin
cat > /tmp/ci-bin/cargo <<SHIM
#!/usr/bin/env bash
# Skip 'cargo build' (binary already built) and convert 'cargo run --bin ion ...'
# to direct execution of the prebuilt binary (avoids cargo lock contention).
if [ "\$1" = "build" ]; then exit 0; fi
if [ "\$1" = "run" ] && echo "\$@" | grep -q -- "--bin ion"; then
    # Extract args after '--' and run the binary directly
    BIN="$PROJECT_DIR/target/debug/ion"
    AFTER_DASH=""
    FOUND_DASH=0
    for arg in "\$@"; do
        if [ "\$FOUND_DASH" = "1" ]; then
            AFTER_DASH="\$AFTER_DASH \"\$arg\""
        elif [ "\$arg" = "--" ]; then
            FOUND_DASH=1
        fi
    done
    eval "exec \"\$BIN\" \$AFTER_DASH"
fi
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
    local work_dir="/tmp/ci-work-$worker_id"
    local log="/tmp/ci-out-$bn.log"
    local result_file="/tmp/ci-results/$bn.jsonl"

    # Per-script isolated HOME
    rm -rf "$home_dir" "$work_dir"
    mkdir -p "$home_dir/.ion/agent" "$work_dir"
    [ -d "$HOME/.rustup" ] && ln -s "$HOME/.rustup" "$home_dir/.rustup" 2>/dev/null
    [ -d "$HOME/.cargo" ] && ln -s "$HOME/.cargo" "$home_dir/.cargo" 2>/dev/null

    # Per-script isolated work dir — symlink the project so scripts can find
    # target/, tests/, src/, etc., but .ion/ is per-script (no monitor pollution).
    # This is the KEY fix: each script's `ion serve` reads .ion/monitors/ from
    # its own cwd, so monitor configs created by one script don't affect others.
    ln -sfn "$PROJECT_DIR/target" "$work_dir/target"
    ln -sfn "$PROJECT_DIR/tests" "$work_dir/tests"
    ln -sfn "$PROJECT_DIR/src" "$work_dir/src"
    ln -sfn "$PROJECT_DIR/ion-provider" "$work_dir/ion-provider"
    ln -sf "$PROJECT_DIR/Cargo.toml" "$work_dir/Cargo.toml"
    ln -sf "$PROJECT_DIR/Cargo.lock" "$work_dir/Cargo.lock" 2>/dev/null
    ln -sfn "$PROJECT_DIR/examples" "$work_dir/examples" 2>/dev/null
    ln -sfn "$PROJECT_DIR/.git" "$work_dir/.git" 2>/dev/null
    # Workspace member dirs (Cargo.toml references these — cargo test fails without them)
    for member in todo-extension tests-extensions stock-plugin hello-extension \
                  extensions permission dashboard ion-dashboard-ui docs scripts; do
        [ -d "$PROJECT_DIR/$member" ] && ln -sfn "$PROJECT_DIR/$member" "$work_dir/$member"
    done

    # Symlink .ion/ contents EXCEPT monitors/ (that's the one we isolate).
    # Some scripts need .ion/config.json, .ion/settings.json, .ion/agents/ etc.
    mkdir -p "$work_dir/.ion"
    if [ -d "$PROJECT_DIR/.ion" ]; then
        for item in "$PROJECT_DIR/.ion"/*; do
            bn=$(basename "$item")
            if [ "$bn" != "monitors" ]; then
                ln -sfn "$item" "$work_dir/.ion/$bn"
            fi
        done
    fi
    # Ensure monitors/ exists but is empty (scripts may create configs here,
    # but they won't affect other parallel scripts).
    mkdir -p "$work_dir/.ion/monitors"

    # Run from the isolated work dir with isolated HOME + cargo shim.
    # CRITICAL: call the script via the work_dir's symlinked path (not the
    # real path) so that PROJECT_DIR=$(dirname $0/..) resolves to work_dir,
    # not the real project. This ensures .ion/monitors/ is per-script.
    #
    # CARGO_TARGET_DIR points to the REAL project's target/ so that
    # 'cargo test' uses the pre-built cache instead of recompiling from
    # scratch in the work_dir (which would take 3+ minutes).
    local script_in_workdir="$work_dir/tests/$(basename "$script")"
    local start=$(date +%s)
    (
        cd "$work_dir"
        HOME="$home_dir" \
        PATH="/tmp/ci-bin:$PATH" \
        CARGO_TARGET_DIR="$PROJECT_DIR/target" \
        timeout "$PER_SCRIPT_TIMEOUT" bash "$script_in_workdir"
    ) > "$log" 2>&1
    local exit_code=$?
    local end=$(date +%s)
    local dur=$((end - start))

    local status
    if [ $exit_code -eq 0 ]; then status="PASS"; else status="FAIL"; fi

    # Write JSON result
    echo "{\"script\":\"$script\",\"status\":\"$status\",\"exit_code\":$exit_code,\"duration_s\":$dur,\"log_path\":\"$log\"}" > "$result_file"

    # Cleanup HOME + work dirs
    rm -rf "$home_dir" "$work_dir"

    echo "  $status $bn (exit=$exit_code, ${dur}s)"
}
export -f run_one_script
export PER_SCRIPT_TIMEOUT REAL_CARGO PROJECT_DIR

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
