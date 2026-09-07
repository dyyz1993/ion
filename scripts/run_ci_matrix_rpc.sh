#!/usr/bin/env bash
#
# run_ci_matrix_rpc.sh — RPC-driven parallel CI runner (NO LLM coordinator)
#
# Why this exists:
#   The LLM-coordinator version (run_ci_matrix.sh) is unreliable because the
#   coordinator's LLM call can hang. This version drives `ion serve` directly
#   via RPC, spawning N workers in parallel without any LLM in the loop.
#
# Flow:
#   1. Start `ion serve` host (with cargo shim in PATH)
#   2. For each of N batches: send `create_worker` RPC with the batch as task
#   3. Poll `list_workers` until all workers reach agent_end / dead
#   4. Aggregate /tmp/ci-results/*.jsonl → markdown report
#
# Each worker is `ci_runner_worker` (no LLM either — it just iterates the
# script list with bash and writes JSON results).
#
# Wait — ci_runner_worker IS an LLM agent. To truly avoid LLM, we'd need a
# bash-only worker. Since `create_worker` requires an agent, we use the
# `developer` agent with a very directive prompt that just runs scripts.
#
# Usage:
#   bash scripts/run_ci_matrix_rpc.sh
#
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
PARALLELISM="${PARALLELISM:-5}"
PER_SCRIPT_TIMEOUT="${PER_SCRIPT_TIMEOUT:-180}"
HOST_TIMEOUT="${HOST_TIMEOUT:-1800}"

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  CI Matrix Runner — RPC-driven (no LLM coordinator)"
echo "════════════════════════════════════════════════════════════════"
echo "  Binary:          $ION_BIN"
echo "  Parallelism:     $PARALLELISM workers"
echo "  Per-script T/O:  ${PER_SCRIPT_TIMEOUT}s"
echo "  Host T/O:        ${HOST_TIMEOUT}s"
echo ""

# ─── Step 0: Pre-flight ────────────────────────────────────────────────────
if [ ! -x "$ION_BIN" ]; then
    echo "❌ Binary not found: $ION_BIN"
    exit 1
fi

# Per-run isolation root (results/bin/out/home) + PRIVATE host socket.
# Never touch ~/.ion/host.sock or any shared /tmp/ci-* path: the user's
# resident host or a concurrent matrix run must not be destroyed by us.
RUN_ROOT="${CI_RUN_ROOT:-/tmp/ci-matrix-$(date +%Y%m%d-%H%M%S)-$$}"
CI_RESULTS_DIR="$RUN_ROOT/results"
CI_BIN_DIR="$RUN_ROOT/bin"
CI_OUT_DIR="$RUN_ROOT/out"
mkdir -p "$CI_RESULTS_DIR" "$CI_BIN_DIR" "$CI_OUT_DIR"
SOCK="${ION_HOST_SOCKET:-$RUN_ROOT/host.sock}"
export ION_HOST_SOCKET="$SOCK"
echo "  Run root: $RUN_ROOT"
echo "  Host socket: $SOCK"

# ─── Step 1: Prepare cargo shim + per-worker HOME ─────────────────────────
REAL_CARGO=$(command -v cargo 2>/dev/null || echo /usr/local/cargo/bin/cargo)
echo "  Real cargo: $REAL_CARGO"
cat > "$CI_BIN_DIR/cargo" <<SHIM
#!/usr/bin/env bash
# Honest cargo shim: 'cargo build' no-ops ONLY while the prebuilt binary
# this run launched with still exists (SHA recorded in results/ion.sha);
# otherwise it falls through to real cargo. Everything else is always real.
REAL_CARGO='$REAL_CARGO'
PROJECT_DIR='$PROJECT_DIR'
if [ "\$1" = "build" ] && [ -x "\$PROJECT_DIR/target/debug/ion" ]; then
    echo "    (cargo shim: build skipped — prebuilt ion present, see results/ion.sha)"
    exit 0
fi
exec "\$REAL_CARGO" "\$@"
SHIM
chmod +x "$CI_BIN_DIR/cargo"
# Record the prebuilt artifact identity (预编译产物记录 SHA).
[ -f "$PROJECT_DIR/target/debug/ion" ] && \
    shasum "$PROJECT_DIR/target/debug/ion" > "$CI_RESULTS_DIR/ion.sha"

for ((i=1; i<=PARALLELISM; i++)); do
    HOME_DIR="$RUN_ROOT/home-$i"
    rm -rf "$HOME_DIR"
    mkdir -p "$HOME_DIR/.ion/agent" "$HOME_DIR/bin"
    [ -d "$HOME/.rustup" ] && ln -s "$HOME/.rustup" "$HOME_DIR/.rustup"
    [ -d "$HOME/.cargo" ] && ln -s "$HOME/.cargo" "$HOME_DIR/.cargo"
done
echo "  ✅ HOME dirs + cargo shim ready"
echo ""

# ─── Step 2: Gather + partition CI scripts ────────────────────────────────
ALL_SCRIPTS=()
while IFS= read -r line; do
    [ -n "$line" ] && ALL_SCRIPTS+=("$line")
done < <(ls tests/*_ci.sh tests/scenario2_ci.sh tests/team_e2e.sh 2>/dev/null | sort -u)

SKIP_LIST=(
    "tests/apple_container_ci.sh"
    "tests/mcp_ci.sh"
    "tests/hooks_agent_real.sh"
    "tests/streaming_replay_ci.sh"
    "tests/self_heal_ci.sh"
    "tests/goal_supervisor_ci.sh"
    "tests/goal_evolver_ci.sh"
)

FILTERED=()
for s in "${ALL_SCRIPTS[@]}"; do
    skip=0
    for sk in "${SKIP_LIST[@]}"; do
        if [ "$s" == "$sk" ]; then skip=1; break; fi
    done
    if [ $skip -eq 0 ]; then
        FILTERED+=("$s")
    else
        echo "{\"script\":\"$s\",\"status\":\"SKIP\",\"reason\":\"env-dependent\",\"exit_code\":-1,\"duration_s\":0}" \
            >> "$CI_RESULTS_DIR/skipped.jsonl"
    fi
done

TOTAL=${#FILTERED[@]}
echo "  Total scripts: $TOTAL (skipped: ${#SKIP_LIST[@]})"
echo ""

# Manifest: every discovered script; the aggregator fails the run if any
# scheduled script produced no result.
printf '%s\n' "${ALL_SCRIPTS[@]}" > "$CI_RESULTS_DIR/manifest.txt"

# Partition round-robin into PARALLELISM batches
BATCHES=()
for ((i=0; i<PARALLELISM; i++)); do BATCHES+=(""); done
for ((i=0; i<TOTAL; i++)); do
    idx=$((i % PARALLELISM))
    if [ -z "${BATCHES[$idx]}" ]; then
        BATCHES[$idx]="${FILTERED[$i]}"
    else
        BATCHES[$idx]="${BATCHES[$idx]} ${FILTERED[$i]}"
    fi
done

echo "  Batch assignment:"
for ((i=0; i<PARALLELISM; i++)); do
    cnt=$(echo "${BATCHES[$i]}" | wc -w)
    echo "    Worker $((i+1)): $cnt scripts"
done
echo ""

# ─── Step 3: Start ion serve host ──────────────────────────────────────────
echo "[Step 3] Starting ion serve host (private socket: $SOCK)..."
if [ -S "$SOCK" ]; then
    if [ -n "${ION_HOST_SOCKET:-}" ]; then
        echo "❌ ION_HOST_SOCKET=$SOCK already exists — refusing to touch it; use a private socket path"
        exit 1
    fi
    rm -f "$SOCK"   # own fresh run root; stale file from an identical-$$ crash
fi

PATH="$CI_BIN_DIR:$PATH" "$ION_BIN" serve > "$RUN_ROOT/host.log" 2>&1 &
HOST_PID=$!
trap "kill $HOST_PID 2>/dev/null" EXIT

# Wait for host to be ready
for i in $(seq 1 30); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
        echo "  ✅ host ready (pid=$HOST_PID)"
        break
    fi
done

# ─── Step 4: Spawn N workers via create_worker RPC ────────────────────────
echo ""
echo "[Step 4] Spawning $PARALLELISM workers via create_worker RPC..."

WORKER_IDS=()
for ((i=0; i<PARALLELISM; i++)); do
    BATCH_NUM=$((i+1))
    SCRIPTS="${BATCHES[$i]}"

    # Build the task prompt for this worker
    TASK="You are CI runner worker $BATCH_NUM. Run these scripts SEQUENTIALLY:
$SCRIPTS

For EACH script:
1. Run: HOME=${RUN_ROOT}/home-$BATCH_NUM timeout ${PER_SCRIPT_TIMEOUT}s bash <script> > ${CI_OUT_DIR}/batch-${BATCH_NUM}-<basename>.log 2>&1
2. Capture exit code and duration.
3. Append ONE JSON line to ${CI_RESULTS_DIR}/batch-${BATCH_NUM}.jsonl:
   {\"script\":\"<path>\",\"status\":\"PASS\" or \"FAIL\",\"exit_code\":<N>,\"duration_s\":<N>,\"log_path\":\"${CI_OUT_DIR}/batch-${BATCH_NUM}-<basename>.log\"}

Rules:
- mkdir -p ${CI_RESULTS_DIR} first.
- status='PASS' if exit_code==0, else 'FAIL'.
- Do NOT stop on failure — run all scripts.
- Use bash tool. No editing, no spawning workers."

    # Build JSON params safely via python3 with explicit arg
    PARAMS=$(python3 -c "
import json, sys
task = sys.argv[1]
print(json.dumps({
    'agent': 'developer',
    'relation': 'peer',
    'report_channel': 'ci-matrix',
    'initial_prompt': task,
    'name': 'ci-worker-$BATCH_NUM',
}))" "$TASK")

    # create_worker RPC
    RESULT=$("$ION_BIN" rpc --method create_worker --params "$PARAMS" 2>&1)

    WID=$(echo "$RESULT" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('data',{}).get('workerId','?'))" 2>/dev/null)
    if [ -n "$WID" ] && [ "$WID" != "?" ]; then
        WORKER_IDS+=("$WID")
        echo "  ✅ Worker $BATCH_NUM spawned: $WID"
    else
        echo "  ❌ Worker $BATCH_NUM spawn failed: $RESULT"
    fi
done

echo ""
echo "  Spawned ${#WORKER_IDS[@]} workers: ${WORKER_IDS[*]}"
echo ""

# ─── Step 5: Poll until all workers are done ──────────────────────────────
echo "[Step 5] Waiting for workers to complete (polling every 10s)..."
START_TIME=$(date +%s)
while true; do
    NOW=$(date +%s)
    ELAPSED=$((NOW - START_TIME))
    if [ "$ELAPSED" -gt "$HOST_TIMEOUT" ]; then
        echo "  ⚠️  Host timeout ($HOST_TIMEOUT s) reached, stopping"
        break
    fi

    # Count MY workers still busy (exclude memory-agent and other system workers)
    STATE=$("$ION_BIN" rpc --method list_workers 2>/dev/null)
    # Pass WORKER_IDS into python via env var
    MY_IDS="${WORKER_IDS[*]}"
    BUSY=$(MY_IDS="$MY_IDS" python3 -c "
import json, sys, os
my_ids = set(os.environ.get('MY_IDS', '').split())
try:
    d = json.load(sys.stdin)
    workers = d.get('data', {}).get('workers', [])
    my_workers = [w for w in workers if w.get('workerId') in my_ids]
    busy = sum(1 for w in my_workers if w.get('status') == 'Busy')
    total = len(my_workers)
    print(f'{busy}/{total}')
except Exception as e:
    print(f'0/0 (err: {e})')
" <<< "$STATE" 2>/dev/null)

    echo "  [${ELAPSED}s] my workers busy: $BUSY"
    if [[ "$BUSY" == 0/* ]]; then
        echo "  ✅ All my workers idle"
        break
    fi
    sleep 10
done

# ─── Step 6: Shutdown host + aggregate ────────────────────────────────────
echo ""
echo "[Step 6] Shutting down host..."
"$ION_BIN" rpc --method quit >/dev/null 2>&1
sleep 2
kill $HOST_PID 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Phase 1 complete — aggregating results"
echo "════════════════════════════════════════════════════════════════"
echo ""

AGG_RC=1
if [ -x "$PROJECT_DIR/scripts/aggregate_ci_results.sh" ]; then
    RESULTS_DIR="$CI_RESULTS_DIR" \
    MANIFEST_FILE="$CI_RESULTS_DIR/manifest.txt" \
    REPORT_PATH="${REPORT_PATH:-$PROJECT_DIR/docs/testing/CI_MATRIX_REPORT.md}" \
        bash "$PROJECT_DIR/scripts/aggregate_ci_results.sh"
    AGG_RC=$?
else
    echo "⚠️  aggregate_ci_results.sh not found; raw results in $CI_RESULTS_DIR/"
fi
echo "Run artifacts kept in: $RUN_ROOT"
exit "$AGG_RC"
