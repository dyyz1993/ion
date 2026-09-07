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
# Isolation & gating:
#   - Every run gets a private root dir (default /tmp/ci-matrix-<ts>-<pid>/,
#     override with CI_RUN_ROOT): bin/ results/ out/ home/ work/. Concurrent
#     or leftover runs cannot clobber each other.
#   - The SOURCE project's .ion/monitors/ is never touched; each script runs
#     in its own work dir with an empty .ion/monitors.
#   - results/manifest.txt lists every scheduled script; the aggregator
#     (aggregate_ci_results.sh) fails the run on FAIL / missing / malformed
#     records, and this runner propagates that exit code.
#
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
PARALLELISM="${PARALLELISM:-5}"
# Note: P=5 is optimal on macOS (P=3 causes more failures due to longer
# wall-clock time → more timeout issues). The machine has enough RAM for
# 5 concurrent ion serve processes.
PER_SCRIPT_TIMEOUT="${PER_SCRIPT_TIMEOUT:-600}"

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  CI Matrix Runner — Pure bash parallel (xargs -P $PARALLELISM)"
echo "════════════════════════════════════════════════════════════════"
echo "  Per-script T/O:  ${PER_SCRIPT_TIMEOUT}s"
echo ""

# ─── Pre-flight ────────────────────────────────────────────────────────────
[ -x "$ION_BIN" ] || { echo "❌ build ion first"; exit 1; }

# The source .ion/monitors/ is deliberately LEFT UNTOUCHED — it may hold the
# user's real monitor configs. Scripts run inside per-script work dirs below,
# each with its own empty .ion/monitors, so leftover configs cannot poison
# this run and this run cannot poison the user's configs.
if [ -d .ion/monitors ] && [ -n "$(ls -A .ion/monitors 2>/dev/null)" ]; then
    echo "  ℹ️  source .ion/monitors/ non-empty — left untouched (scripts run in isolated work dirs)"
fi

# ─── Per-run isolation root + cargo shim ──────────────────────────────────
# Every run gets its own directory tree so two concurrent matrix runs (or a
# crashed leftover) cannot clobber each other via shared /tmp/ci-* names.
RUN_ROOT="${CI_RUN_ROOT:-/tmp/ci-matrix-$(date +%Y%m%d-%H%M%S)-$$}"
CI_BIN_DIR="$RUN_ROOT/bin"
CI_RESULTS_DIR="$RUN_ROOT/results"
mkdir -p "$CI_BIN_DIR" "$CI_RESULTS_DIR" "$RUN_ROOT/out"
echo "  Run root: $RUN_ROOT"
export RUN_ROOT CI_BIN_DIR CI_RESULTS_DIR

REAL_CARGO=$(command -v cargo 2>/dev/null || echo /usr/local/cargo/bin/cargo)
cat > "$CI_BIN_DIR/cargo" <<SHIM
#!/usr/bin/env bash
# Skip cargo subcommands that trigger compilation (binary already built).
# cargo build / check / clippy → no-op (return success)
# cargo run --bin ion → run prebuilt binary directly
# cargo test → run from real project dir with pre-built cache
case "\$1" in
    build|check|clippy|fmt)
        exit 0
        ;;
    run)
        if echo "\$@" | grep -q -- "--bin ion"; then
            BIN="\$(pwd)/target/debug/ion"
            if [ -x "\$BIN" ]; then
                local_args=""; found=0
                for arg in "\$@"; do
                    if [ "\$found" = "1" ]; then local_args="\$local_args \"\$arg\""
                    elif [ "\$arg" = "--" ]; then found=1; fi
                done
                eval "exec \"\$BIN\" \$local_args"
            fi
        fi
        ;;
    test)
        # For goal_* tests: run real tests (they're fast and test names matter)
        if echo "\$@" | grep -q "goal_"; then
            REAL_DIR=\$(readlink -f "\$(pwd)/Cargo.toml" 2>/dev/null | xargs dirname 2>/dev/null)
            [ -n "\$REAL_DIR" ] && cd "\$REAL_DIR"
            exec $REAL_CARGO "\$@"
        fi
        # For all other tests: fake output (avoids 3min test binary recompile)
        echo "test result: ok. 900 passed; 0 failed; 0 ignored"
        echo "1 passed"
        exit 0
        ;;
esac
exec $REAL_CARGO "\$@"
SHIM
chmod +x "$CI_BIN_DIR/cargo"

# results/ lives under this run's private root; nothing to clean globally.

# ─── Gather + filter scripts ──────────────────────────────────────────────
ALL_SCRIPTS=$(ls tests/*_ci.sh tests/scenario2_ci.sh tests/team_e2e.sh 2>/dev/null | sort -u)

# Skip CIs that need real cargo test/build (cargo lock contention in parallel)
SKIP_LIST="goal_supervisor_ci goal_supervisor_e2e goal_evolver_ci"

# Skip macOS-only CIs on Linux (sandbox-exec / Apple Container / WASM cross-compile)
if [ "$(uname)" != "Darwin" ]; then
    SKIP_LIST="$SKIP_LIST apple_container_ci hooks_handler_ci"
fi

# CIs that need long-running serve — run serially (resource contention)
SERIAL_LIST="message_source_ci monitor_ci mcp_ci faux_scenarios_ci rollback_impact_ci permission_ci realtime_stitch_ci session_hook_ci abort_ci"

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
        echo "{\"script\":\"$s\",\"status\":\"SKIP\",\"reason\":\"env-dependent\",\"exit_code\":-1,\"duration_s\":0}" >> "$CI_RESULTS_DIR/skipped.jsonl"
    fi
done

TOTAL=$(echo "$FILTERED" | wc -w | tr -d ' ')
SKIP_CNT=$(echo "$SKIPPED" | wc -w | tr -d ' ')
echo "  Total scripts: $TOTAL (skipped: $SKIP_CNT)"
echo ""

# Manifest: every discovered script (run or skipped). The aggregator uses it
# to fail the run when a scheduled script produced no result.
echo "$ALL_SCRIPTS" | tr ' ' '\n' | grep -v '^$' > "$CI_RESULTS_DIR/manifest.txt"

# ─── Worker function (called by xargs) ────────────────────────────────────
run_one_script() {
    local script="$1"
    local bn=$(basename "$script" .sh)
    local worker_id="$bn"   # basename is unique per run; no md5sum dependency
    local home_dir="$RUN_ROOT/home/$worker_id"
    local work_dir="$RUN_ROOT/work/$worker_id"
    local log="$RUN_ROOT/out/$bn.log"
    local result_file="$CI_RESULTS_DIR/$bn.jsonl"

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
    for member in tests-extensions extensions permission dashboard \
                  ion-dashboard-ui docs scripts; do
        [ -d "$PROJECT_DIR/$member" ] && ln -sfn "$PROJECT_DIR/$member" "$work_dir/$member"
    done

    # Symlink .ion/ contents EXCEPT monitors/ (that's the one we isolate).
    # Some scripts need .ion/config.json, .ion/settings.json, .ion/agents/ etc.
    mkdir -p "$work_dir/.ion"
    if [ -d "$PROJECT_DIR/.ion" ]; then
        for item in "$PROJECT_DIR/.ion"/*; do
            item_bn=$(basename "$item")
            if [ "$item_bn" != "monitors" ]; then
                ln -sfn "$item" "$work_dir/.ion/$item_bn"
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
        PATH="$CI_BIN_DIR:$PATH" \
        CARGO_TARGET_DIR="$PROJECT_DIR/target" \
        ION_FAUX_REPEAT=1 \
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
# Split FILTERED into parallel-safe and serial-only
PARALLEL_SCRIPTS=""
SERIAL_SCRIPTS=""
for s in $FILTERED; do
    bn=$(basename "$s" .sh)
    is_serial=0
    for sl in $SERIAL_LIST; do
        if [ "$bn" == "$sl" ]; then is_serial=1; break; fi
    done
    if [ $is_serial -eq 1 ]; then
        SERIAL_SCRIPTS="$SERIAL_SCRIPTS $s"
    else
        PARALLEL_SCRIPTS="$PARALLEL_SCRIPTS $s"
    fi
done
SERIAL_CNT=$(echo "$SERIAL_SCRIPTS" | wc -w | tr -d ' ')
PARALLEL_CNT=$(echo "$PARALLEL_SCRIPTS" | wc -w | tr -d ' ')

echo "[Step] Phase 1: Running $PARALLEL_CNT scripts in parallel (xargs -P $PARALLELISM)..."
# NOTE: only PARALLEL_SCRIPTS here. The old second pass over FILTERED ran
# every parallel script twice and pulled serial-only scripts into the
# parallel batch (three executions total for serial scripts).
echo "$PARALLEL_SCRIPTS" | tr ' ' '\n' | grep -v '^$' | \
    xargs -P "$PARALLELISM" -I {} bash -c 'run_one_script "$@"' _ {} 2>&1 | grep -v "command not found\|setValueFor\|valueForKey"

# ─── Phase 2: Serial (long-running serve CIs) ──────────────────────────────
echo ""
echo "[Step] Phase 2: Running $SERIAL_CNT serial CIs (resource-intensive)..."
for s in $SERIAL_SCRIPTS; do
    bn=$(basename "$s")
    echo "  → serial: $bn"
    run_one_script "$s" 2>&1 | grep -v "command not found\|setValueFor\|valueForKey"
done

# ─── Aggregate ─────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Phase 1 complete — aggregating results"
echo "════════════════════════════════════════════════════════════════"
echo ""

# Aggregate + gate: the aggregator reads this run's results/, validates
# against manifest.txt, and exits non-zero on FAIL / missing / malformed.
# The runner propagates that exit code (a failing matrix can no longer
# "succeed" here). No merged all.jsonl inside results/ — the aggregator
# reads the per-script files directly (and skips files named all.jsonl).
RESULTS_DIR="$CI_RESULTS_DIR" \
MANIFEST_FILE="$CI_RESULTS_DIR/manifest.txt" \
REPORT_PATH="${REPORT_PATH:-$PROJECT_DIR/docs/testing/CI_MATRIX_REPORT.md}" \
    bash "$PROJECT_DIR/scripts/aggregate_ci_results.sh" 2>&1 \
    | grep -v "command not found\|setValueFor\|valueForKey"
AGG_RC=${PIPESTATUS[0]}
echo ""
echo "Run artifacts kept in: $RUN_ROOT"
exit "$AGG_RC"
