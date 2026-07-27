#!/usr/bin/env bash
#
# run_ci_matrix.sh — Phase 1: A drives B to run all CI scripts in parallel (Scenario 2)
#
# Architecture:
#   ZCode  →  bash run_ci_matrix.sh  (this script)
#              │
#              ▼  ion --host --agent ci_runner_coordinator "<prompt>"
#              │
#              │  A = ci_runner_coordinator agent (in ion --host)
#              │  ├─ spawn_worker(developer, wait=false, worktree=true) × 5
#              │  ├─ Each worker runs ~12 CI scripts in its own worktree
#              │  └─ await_worker × 5, collect results to /tmp/ci-result-*.jsonl
#              │
#              ▼  scripts/aggregate_ci_results.sh
#              │
#              docs/testing/CI_MATRIX_REPORT.md
#
# Usage:
#   bash scripts/run_ci_matrix.sh
#
# Tunables:
#   ION_BIN          (default: ./target/debug/ion)
#   PARALLELISM      (default: 5)
#   PER_SCRIPT_TIMEOUT  (default: 120, seconds)
#   HOST_TIMEOUT     (default: 1800, seconds — host hard cap)
#
set -o pipefail
# NOTE: intentionally NOT using `set -u` because bash arrays (ALL_SCRIPTS, BATCHES)
# trip on unbound variable checks when empty.

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
PARALLELISM="${PARALLELISM:-5}"
PER_SCRIPT_TIMEOUT="${PER_SCRIPT_TIMEOUT:-180}"
HOST_TIMEOUT="${HOST_TIMEOUT:-2400}"

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  CI Matrix Runner — Scenario 2 (A → B, 5 parallel worktrees)"
echo "════════════════════════════════════════════════════════════════"
echo "  Binary:          $ION_BIN"
echo "  Parallelism:     $PARALLELISM workers"
echo "  Per-script T/O:  ${PER_SCRIPT_TIMEOUT}s"
echo "  Host T/O:        ${HOST_TIMEOUT}s"
echo ""

# ─── Step 0: Pre-flight ────────────────────────────────────────────────────
if [ ! -x "$ION_BIN" ]; then
    echo "❌ Binary not found: $ION_BIN"
    echo "   Run: cargo build --bin ion"
    exit 1
fi

# Clean prior results
rm -f /tmp/ci-result-*.jsonl /tmp/ci-out-*.log /tmp/ci-status-* 2>/dev/null
mkdir -p /tmp/ci-results

# ─── Step 1: Gather + partition CI scripts ─────────────────────────────────
# macOS bash 3.2 has no mapfile; use a portable while-read loop.
ALL_SCRIPTS=()
while IFS= read -r line; do
    [ -n "$line" ] && ALL_SCRIPTS+=("$line")
done < <(ls tests/*_ci.sh tests/scenario2_ci.sh tests/team_e2e.sh 2>/dev/null | sort -u)

# Skip scripts that need external services we don't have
SKIP_LIST=(
    "tests/apple_container_ci.sh"     # needs Apple Container runtime
    "tests/mcp_ci.sh"                  # may need real MCP server / API key
    "tests/hooks_agent_real.sh"        # needs real LLM (separate flag)
    "tests/streaming_replay_ci.sh"     # needs prior recording
    "tests/self_heal_ci.sh"            # long, may need GH access
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
        echo "  SKIP (env-dep): $s"
        # Pre-mark as skipped in the results
        echo "{\"script\":\"$s\",\"status\":\"SKIP\",\"reason\":\"env-dependent\",\"exit_code\":-1,\"duration_s\":0}" \
            >> /tmp/ci-results/skipped.jsonl
    fi
done

TOTAL=${#FILTERED[@]}
echo ""
echo "  Total scripts:   $TOTAL (skipped: ${#SKIP_LIST[@]})"
echo ""

if [ "$TOTAL" -eq 0 ]; then
    echo "❌ No CI scripts found in tests/"
    exit 1
fi

# Partition into PARALLELISM batches (round-robin)
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

# ─── Step 2: Prepare worktrees ─────────────────────────────────────────────
WORKTREE_ROOT="/tmp/ion-ci-wt"
rm -rf "$WORKTREE_ROOT" 2>/dev/null
mkdir -p "$WORKTREE_ROOT"
git worktree prune 2>/dev/null

# Each worker gets its own HOME (~/.ion isolation) but we symlink ~/.rustup
# and ~/.cargo so cargo/rustup still work.
#
# CRITICAL: To avoid 5 workers fighting over the cargo target/ lock, we put
# a `cargo` shim in each worker's PATH that no-ops `cargo build` (the binary
# is already prebuilt in the host's target/). Other cargo subcommands
# (test, clippy) pass through to real cargo. This drops per-script runtime
# from 3+ minutes to ~10 seconds.
REAL_CARGO=$(command -v cargo 2>/dev/null || echo /usr/local/cargo/bin/cargo)
echo "  Real cargo: $REAL_CARGO"
mkdir -p /tmp/ci-bin
cat > /tmp/ci-bin/cargo <<SHIM
#!/usr/bin/env bash
# Cargo shim: skip 'cargo build' (binary already built), pass through everything else.
if [ "\$1" = "build" ]; then
    echo "    (cargo shim: skipping build — using prebuilt binary)"
    exit 0
fi
exec $REAL_CARGO "\$@"
SHIM
chmod +x /tmp/ci-bin/cargo

for ((i=1; i<=PARALLELISM; i++)); do
    HOME_DIR="/tmp/ci-home-$i"
    rm -rf "$HOME_DIR"
    mkdir -p "$HOME_DIR/.ion/agent" "$HOME_DIR/bin"
    # Symlink rust toolchain dirs (read-only is fine)
    [ -d "$HOME/.rustup" ] && ln -s "$HOME/.rustup" "$HOME_DIR/.rustup"
    [ -d "$HOME/.cargo" ] && ln -s "$HOME/.cargo" "$HOME_DIR/.cargo"
    # Put cargo shim in each worker's PATH
    ln -sf /tmp/ci-bin/cargo "$HOME_DIR/bin/cargo"
done

# ─── Step 3: Build the coordinator prompt ──────────────────────────────────
# The prompt lists 5 batches; the coordinator spawns 5 workers, each runs its batch.
PROMPT="Run the following CI script batches in PARALLEL using 5 worker processes.

For EACH of the 5 batches below, call spawn_worker ONCE with these parameters:
- relation: 'child'
- agent: 'ci_runner_worker'
- worktree: false
- wait: false
- task: the full batch instruction (see below)

After spawning all 5, call await_worker for EACH worker id to collect results.
Finally, write a one-line summary: 'CI MATRIX DONE: <pass_count> pass / <fail_count> fail'.

IMPORTANT:
- Spawn all 5 workers BEFORE awaiting any of them (that's what makes it parallel).
- Each worker writes its results to /tmp/ci-results/batch-N.jsonl (one JSON per script).
- Workers MUST run scripts with 'timeout ${PER_SCRIPT_TIMEOUT}s bash <script>'.
- If a worker's bash command exits non-zero, that's a FAIL — record it, don't retry.
- Do NOT edit or write any files yourself. You only orchestrate.
- Each worker has a 'cargo' shim in PATH that no-ops 'cargo build' (binary already built).
  Workers should NOT pass CARGO_TARGET_DIR — they share the host's prebuilt target/.
- Each worker uses HOME=/tmp/ci-home-<N> to avoid ~/.ion collisions.
"

for ((i=0; i<PARALLELISM; i++)); do
    BATCH_NUM=$((i+1))
    SCRIPTS="${BATCHES[$i]}"
    PROMPT="$PROMPT

=== BATCH $BATCH_NUM (worker $BATCH_NUM) ===
Run these scripts SEQUENTIALLY in your worktree, writing a JSON result for each:
$SCRIPTS

For each script, write a line to /tmp/ci-results/batch-${BATCH_NUM}.jsonl:
  {\"script\":\"<name>\",\"status\":\"PASS\"|\"FAIL\",\"exit_code\":<N>,\"duration_s\":<N>,\"log_path\":\"/tmp/ci-out-${BATCH_NUM}-<name>.log\"}

Capture stdout+stderr to /tmp/ci-out-${BATCH_NUM}-<basename>.log.
Use EXACTLY this command form (PATH includes the cargo shim, HOME isolates ~/.ion):
  HOME=/tmp/ci-home-${BATCH_NUM} PATH=/tmp/ci-home-${BATCH_NUM}/bin:/tmp/ci-bin:/usr/local/bin:/usr/bin:/bin timeout ${PER_SCRIPT_TIMEOUT}s bash <script> > <log> 2>&1
"
done

# ─── Step 4: Launch ion --host with coordinator ────────────────────────────
echo "Launching A (ci_runner_coordinator) via ion --host..."
echo "  Worktrees root: $WORKTREE_ROOT"
echo "  Per-worker HOME: /tmp/ci-home-{1..$PARALLELISM}"
echo ""

ION_WORKTREE_ROOT="$WORKTREE_ROOT" \
ION_HOST_TIMEOUT="$HOST_TIMEOUT" \
ION_HOST_IDLE_GRACE=600 \
RUST_LOG=info \
timeout $((HOST_TIMEOUT + 300)) \
"$ION_BIN" --host --agent ci_runner_coordinator "$PROMPT" \
    2>&1 | tee /tmp/ci-matrix-host.log

HOST_EXIT=$?
echo ""
echo "Host exited with code: $HOST_EXIT"

# ─── Step 5: Aggregate results ─────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Phase 1 complete — aggregating results"
echo "════════════════════════════════════════════════════════════════"
echo ""

if [ -x "$PROJECT_DIR/scripts/aggregate_ci_results.sh" ]; then
    bash "$PROJECT_DIR/scripts/aggregate_ci_results.sh"
else
    echo "⚠️  aggregate_ci_results.sh not found; raw results in /tmp/ci-results/"
    ls -la /tmp/ci-results/ 2>/dev/null
fi

echo ""
echo "Report: docs/testing/CI_MATRIX_REPORT.md"
