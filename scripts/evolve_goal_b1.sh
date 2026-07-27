#!/usr/bin/env bash
# evolve_goal_b1.sh — Goal Supervisor Phase 1 (core loop) via A→B
#
# Implements docs/design/GOAL_SUPERVISOR_B1_TASK.md
# 3 stages: B1-a (data structs + tool) → B1-b (run_checks) → B1-c (state machine + loop)
#
# Usage:
#   bash scripts/evolve.sh                # init container + worktree (first time ~3min)
#   source /tmp/.evolver-state
#   bash scripts/evolve_goal_b1.sh        # run B1 (20-40 min)
#
# Prereqs:
#   - /tmp/.evolver-state must exist (CONTAINER_NAME, WT_DIR, PROJECT_DIR)
#   - container must be running (container ls shows it)
set -uo pipefail

PROJECT_DIR="${PROJECT_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
HTML_DIR="/tmp/evolve_goal_reports"
mkdir -p "$HTML_DIR"

source /tmp/.evolver-state 2>/dev/null
if [ -z "${CONTAINER_NAME:-}" ] || [ -z "${WT_DIR:-}" ]; then
    echo "ERROR: /tmp/.evolver-state missing or incomplete."
    echo "Run: bash scripts/evolve.sh first"
    exit 1
fi

# Verify container is actually running
if ! "$CONTAINER_BIN" ls 2>/dev/null | grep -q "$CONTAINER_NAME"; then
    echo "ERROR: Container '$CONTAINER_NAME' is not running."
    echo "Run: bash scripts/evolve.sh to restart it"
    exit 1
fi

echo ""
echo "=========================================="
echo "  Goal Supervisor B1 — A→B Evolution"
echo "=========================================="
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo "  Model:     $PROVIDER/$MODEL"
echo "=========================================="
echo ""

# ── Stage prompt builders ──────────────────────────────────────────

build_stage_a_prompt() {
    cat <<'EOF'
Task: Implement Goal Supervisor B1 Stage A (data structures + goal_set tool skeleton).

FIRST: read these two files to understand the full design:
- docs/design/GOAL_SUPERVISOR.md (sections 2, 3, 6, 7)
- docs/design/GOAL_SUPERVISOR_B1_TASK.md (sections 1, 2.1, 2.3)

THEN: create src/goal_supervisor_extension.rs with:

1. Data structures (follow B1_TASK.md section 2.1 exactly):
   - enum CheckType { Ci, Contingency }
   - enum PassCriteria { ExitCode{expected}, GrepEmpty{pattern}, FileExists{path} }
   - struct Check { name, check_type, rationale, command, pass_criteria, must_pass }
   - enum CheckStatus { Pass, Fail, Error, Skipped }
   - struct Evidence { exit_code, stdout_excerpt, artifact_path, matches }
   - struct CheckResult { name, status, evidence, duration_ms, reason }
   - enum GoalStatus { Running, Checking, Complete, Exhausted, Blocked, Cancelled }
   - struct GoalState { goal_id, objective, checks, status, iteration_count, started_at, total_cost_usd, last_action_plan }
   - struct GoalSupervisorConfig { enabled, check_on_agent_end, max_iterations, max_total_duration_min, max_total_cost_usd, repetition_threshold, delay_ms } with Default impl

2. GoalSupervisorExtension struct (state: Arc<Mutex<Option<GoalState>>>, config, session_id)
   - impl AgentExtension with name() = "goal_supervisor"
   - on_agent_end() returns Ok(()) for now (will wire up in Stage C)

3. GoalSetTool struct sharing state with extension (use SharedPlanExtension pattern as reference)
   - impl Tool: name()="goal_set", parameters() per B1_TASK.md section 2.3
   - execute() sets goal state (overriding any previous), returns confirmation

4. Unit tests (at bottom, #[cfg(test)] mod tests):
   - test_goal_set_overrides: set goal A, then set goal B, verify A is cancelled
   - test_config_defaults: default config has expected values
   - test_check_serialization: Check struct serializes/deserializes correctly

CRITICAL RULES (violation = task failure):
1. ALL comments MUST be in ENGLISH ONLY. No Chinese characters anywhere.
2. Use serde derive (Serialize, Deserialize) on all data structs.
3. Use #[serde(tag = "kind")] on PassCriteria enum.
4. Do NOT modify Cargo.toml (async_trait, serde, tokio already available).
5. Reference src/learning_extension.rs or src/monitor_extension.rs for AgentExtension pattern.
6. Reference src/agent/plan_extension.rs + src/agent/plan_tool.rs for shared-state pattern.
7. After creating file, run: cargo check 2>&1 | tail -10
8. If cargo check has errors, fix them. Repeat until clean.
9. Run: grep -c $'\xef\xbf\xbd' src/goal_supervisor_extension.rs  (MUST be 0)
10. Run: cargo test --lib goal_supervisor 2>&1 | tail -10
11. When tests pass: git add src/goal_supervisor_extension.rs && git commit -m "feat(goal): B1-a data structs + goal_set tool skeleton"

Report: file created, cargo check result, test result, commit hash.
EOF
}

build_stage_b_prompt() {
    cat <<'EOF'
Task: Implement Goal Supervisor B1 Stage B (run_all_checks + evidence collection).

FIRST: read docs/design/GOAL_SUPERVISOR.md section 3 (Check data structure) and section 7 (log schema).
THEN: read your existing src/goal_supervisor_extension.rs (Stage A output).

ADD to src/goal_supervisor_extension.rs:

1. impl GoalSupervisorExtension:
   - async fn run_all_checks(&self) -> Result<Vec<CheckResult>, String>
     Iterates self.state.checks, calls run_single_check for each.
   - async fn run_single_check(check: &Check) -> Result<CheckResult, String>
     Uses tokio::process::Command to execute check.command.
     Captures stdout/stderr/exit_code.
     Evaluates pass_criteria:
       - ExitCode{expected}: pass if exit_code == expected
       - GrepEmpty{pattern}: run grep on stdout, pass if no matches
       - FileExists{path}: pass if file exists
     Collects Evidence:
       - exit_code: from command
       - stdout_excerpt: first 2000 chars of stdout
       - artifact_path: write full stdout to /tmp/goal-checks/<check_name>-<timestamp>.log
       - matches: grep matches (for GrepEmpty)
     If evidence cannot be collected -> status=Fail, reason="no evidence"
     Returns CheckResult with duration_ms measured.

2. Unit tests (add to existing test module):
   - test_check_exit_code_pass: command "true", ExitCode(0) -> Pass
   - test_check_exit_code_fail: command "false", ExitCode(0) -> Fail
   - test_check_grep_empty_pass: command "echo hello", GrepEmpty("xyz") -> Pass
   - test_check_grep_empty_fail: command "echo hello", GrepEmpty("hello") -> Fail
   - test_check_file_exists_pass: FileExists("/tmp") -> Pass
   - test_check_file_exists_fail: FileExists("/nonexistent/xyz") -> Fail
   - test_evidence_artifact_written: run a check, verify artifact file exists

CRITICAL RULES:
1. ALL comments MUST be in ENGLISH ONLY.
2. Use tokio::process::Command (NOT std::process::Command).
3. Use std::time::Instant for duration measurement.
4. Create artifact dir with std::fs::create_dir_all if not exists.
5. Do NOT modify Cargo.toml.
6. After editing: cargo check 2>&1 | tail -10 (fix until clean)
7. grep -c U+FFFD must be 0.
8. cargo test --lib goal_supervisor must pass.
9. git add + commit -m "feat(goal): B1-b run_all_checks + evidence collection"

Report: methods added, cargo check, test count passed, commit hash.
EOF
}

build_stage_c_prompt() {
    cat <<'EOF'
Task: Implement Goal Supervisor B1 Stage C (state machine + 6 guards + closed loop + logging).

FIRST: read docs/design/GOAL_SUPERVISOR.md sections 5 (state machine), 6 (6 guards), 7 (log schema).
THEN: read your existing src/goal_supervisor_extension.rs (Stage A+B output).

ADD/WIRE to src/goal_supervisor_extension.rs:

1. impl AgentExtension for GoalSupervisorExtension:
   async fn on_agent_end(&self, ctx: &AgentContext) -> AgentResult<()> {
     - if !config.enabled || no active goal -> return Ok(())
     - set status = Checking
     - results = self.run_all_checks().await?
     - self.log_iteration(&results).await?   // write to iterations.jsonl
     - all_pass = results all Pass
     - if all_pass:
         set status = Complete
         self.write_final_report("complete", "all_checks_passed").await?
         return Ok(())
     - if let Some(reason) = self.check_guards().await?:
         set status = Exhausted
         self.write_final_report("exhausted", &reason).await?
         return Ok(())
     - self.inject_continue(&results).await?  // inject user msg for next turn
     Ok(())
   }

2. async fn check_guards(&self) -> Result<Option<String>, String>
   Checks 6 guards (return first hit):
     ① iteration_count >= max_iterations -> "max_iterations"
     ⑤ elapsed_min >= max_total_duration_min -> "max_duration"
     ⑥ total_cost_usd >= max_total_cost_usd -> "max_cost"
     ③ similarity(last_action_plan, current) >= repetition_threshold -> "repetitive"
   (②④ are decision-time checks, handled in inject_continue)

3. fn calculate_similarity(a: &str, b: &str) -> f64
   Jaccard similarity on whitespace-split tokens (like pi). 0.0 if either empty.

4. async fn inject_continue(&self, results: &[CheckResult]) -> Result<(), String>
   - Build message listing failed checks with evidence excerpts:
     "Goal not complete. Failed checks:\n- <name> (evidence: <excerpt>)"
   - ② confidence guard + ④ repetitive strategy: if repetitive, append "Try a different approach."
   - Inject as user message into agent context (reference how hooks/extension.rs injects context,
     or push to a channel the agent loop reads on next turn).
   - If unsure of injection mechanism: use ctx to push a message. Look at src/hooks/handler_runner.rs
     for how prompt handler injects. Simplest: store pending message, expose via a getter the
     agent loop checks. For B1, even a println! that the test harness greps is acceptable as MVP.

5. async fn log_iteration(&self, results: &[CheckResult]) -> Result<(), String>
   Append JSON line to ~/.ion/agent/goal-runs/<session_id>/iterations.jsonl
   Schema per GOAL_SUPERVISOR.md section 7.2:
     {iter, timestamp, goal_id, objective, guards_hit{}, checks_run[], all_passed, failed_checks[]}

6. async fn write_final_report(&self, status: &str, reason: &str) -> Result<(), String>
   Write ~/.ion/agent/goal-runs/<session_id>/final-report.json

7. Unit tests:
   - test_guard_max_iterations: iteration_count at limit -> Some("max_iterations")
   - test_guard_max_duration: started_at old enough -> Some("max_duration")
   - test_guard_max_cost: cost over limit -> Some("max_cost")
   - test_repetition_detection: high similarity -> Some("repetitive")
   - test_similarity_identical: same strings -> 1.0
   - test_similarity_disjoint: no shared tokens -> 0.0
   - test_log_schema_complete: log a fake iteration, read back, verify all fields present

8. Register extension in src/agent/extension.rs:
   - Read config.goal_supervisor.enabled
   - If enabled, create GoalSupervisorExtension + GoalSetTool sharing state, register both.

CRITICAL RULES:
1. ALL comments MUST be in ENGLISH ONLY.
2. Use chrono or std::time for timestamps (unix ms as u64).
3. Use serde_json for log writing.
4. Do NOT modify Cargo.toml.
5. After editing: cargo check | tail -15 (fix until clean)
6. grep -c U+FFFD src/goal_supervisor_extension.rs src/agent/extension.rs (both must be 0)
7. cargo test --lib goal_supervisor must pass.
8. git add + commit -m "feat(goal): B1-c state machine + 6 guards + closed loop + logging + registration"

Report: what was wired, cargo check, test count, commit hash.
EOF
}

# ── Run a stage: send prompt to B, return B's last output ─────────

run_b_stage() {
    local stage_name="$1" prompt="$2"
    echo ""
    echo "=========================================="
    echo "  Stage $stage_name"
    echo "=========================================="
    echo "  [A→B] Sending prompt to developer..."
    echo "$prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
        "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -30
}

# ── Gate: check U+FFFD in target files ─────────────────────────────

gate_check_ufffd() {
    local file="$1"
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "grep -c \$'\\xef\\xbf\\xbd' /workspace/$file 2>/dev/null || true" 2>/dev/null \
        | head -1 | tr -d '[:space:]'
}

fix_ufffd_in_b() {
    local file="$1" count="$2"
    echo "  [A] Gate REJECTED ($count U+FFFD in $file). Driving B to self-fix..."
    echo "Your $file contains $count U+FFFD garbled chars. Run: grep -n \$'\xef\xbf\xbd' $file to find them. Use edit tool to replace each U+FFFD with correct English text. After fix: grep -c \$'\xef\xbf\xbd' $file (must be 0); cargo check; git add $file && git commit --amend --no-edit" \
        | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10
}

# ── Sync to main repo + verify + commit ────────────────────────────

sync_and_verify() {
    local stage="$1"
    cd "$PROJECT_DIR"

    # Copy all changed/new files from worktree
    local files=(
        "src/goal_supervisor_extension.rs"
        "src/agent/extension.rs"
    )

    echo "  [A] Syncing files from worktree..."
    for f in "${files[@]}"; do
        if [ -f "$WT_DIR/$f" ]; then
            cp "$WT_DIR/$f" "$PROJECT_DIR/$f"
        fi
    done

    # cargo build (full verification in main repo)
    echo "  [A] Running cargo build..."
    local build_out
    build_out=$(cargo build --bin ion 2>&1)
    if ! echo "$build_out" | grep -q "Finished"; then
        echo "  [A] ❌ cargo build FAILED"
        echo "$build_out" | grep -E "^error" | head -5
        for f in "${files[@]}"; do git checkout -- "$f" 2>/dev/null; done
        return 1
    fi
    echo "  [A] ✅ cargo build OK"

    # cargo test --lib goal_supervisor
    echo "  [A] Running cargo test --lib goal_supervisor..."
    local test_out
    test_out=$(cargo test --lib goal_supervisor 2>&1)
    if ! echo "$test_out" | grep -q "test result: ok"; then
        echo "  [A] ❌ cargo test FAILED"
        echo "$test_out" | tail -10
        for f in "${files[@]}"; do git checkout -- "$f" 2>/dev/null; done
        return 1
    fi
    local passed=$(echo "$test_out" | grep -oE "[0-9]+ passed" | head -1)
    echo "  [A] ✅ cargo test: $passed"

    # clippy (no new warnings on our file)
    echo "  [A] Running cargo clippy on goal_supervisor..."
    local clippy_out
    clippy_out=$(cargo clippy --lib 2>&1 | grep -A1 "goal_supervisor" || true)
    if [ -n "$clippy_out" ] && echo "$clippy_out" | grep -q "warning\|error"; then
        echo "  [A] ⚠️  clippy warnings on goal_supervisor:"
        echo "$clippy_out" | head -5
    fi

    # Commit
    git add "${files[@]}" 2>/dev/null
    if git diff --cached --quiet; then
        echo "  [A] ℹ️  No changes to commit (already in sync)"
    else
        git commit -m "feat(goal): B1-$stage via A→B self-evolution" 2>&1 | head -2
        echo "  [A] ✅ Committed"
    fi
    return 0
}

# ── Main: run 3 stages (or single stage if arg given) ──────────────
# Usage: bash scripts/evolve_goal_b1.sh [a|b|c]
#   no arg = run all 3 stages sequentially
#   a/b/c  = run only that stage

TARGET_FILE="src/goal_supervisor_extension.rs"
STAGE_FILTER="${1:-}"

# Determine which stages to run
if [ -n "$STAGE_FILTER" ]; then
    case "$STAGE_FILTER" in
        a|b|c) STAGES_LIST="$STAGE_FILTER" ;;
        *) echo "ERROR: unknown stage '$STAGE_FILTER'. Use a, b, or c."; exit 1 ;;
    esac
    echo "[A] Running single stage: $STAGE_FILTER"
else
    STAGES_LIST="a b c"
fi

# Build prompt for a stage
get_stage_prompt() {
    case "$1" in
        a) build_stage_a_prompt ;;
        b) build_stage_b_prompt ;;
        c) build_stage_c_prompt ;;
    esac
}

get_stage_name() {
    case "$1" in
        a) echo "B1-a: data structs + goal_set tool" ;;
        b) echo "B1-b: run_checks + evidence" ;;
        c) echo "B1-c: state machine + loop + logging" ;;
    esac
}

for stage in $STAGES_LIST; do
    name="$(get_stage_name "$stage")"
    prompt="$(get_stage_prompt "$stage")"

    echo ""
    echo "########################################################"
    echo "#  STAGE B1-$stage: $name"
    echo "########################################################"

    # Step 1: A drives B
    run_b_stage "$name" "$prompt"

    # Step 2: U+FFFD gate (check target file + extension.rs if stage c)
    files_to_check=("$TARGET_FILE")
    [ "$stage" = "c" ] && files_to_check+=("src/agent/extension.rs")

    for fc in "${files_to_check[@]}"; do
        ufffd=$(gate_check_ufffd "$fc")
        attempt=0
        while [ "$ufffd" != "0" ] && [ $attempt -lt 2 ]; do
            attempt=$((attempt + 1))
            echo "  [A] Gate: $ufffd U+FFFD in $fc (attempt $attempt/2)"
            fix_ufffd_in_b "$fc" "$ufffd"
            ufffd=$(gate_check_ufffd "$fc")
        done
        if [ "$ufffd" != "0" ]; then
            echo "  [A] ❌ U+FFFD gate FAILED for $fc after 2 attempts. Aborting stage."
            continue 2
        fi
        echo "  [A] ✅ U+FFFD gate passed ($fc: 0)"
    done

    # Step 3: Sync + verify + commit in main repo
    if sync_and_verify "$stage"; then
        echo "  [A] ✅ Stage B1-$stage complete"
    else
        echo "  [A] ❌ Stage B1-$stage FAILED verification"
        echo "  [A] Continuing to next stage anyway (B may have partial work)"
    fi
done

# ── Final summary ──────────────────────────────────────────────────

echo ""
echo "=========================================="
echo "  Goal Supervisor B1 — Complete"
echo "=========================================="
echo ""
echo "Verify in main repo:"
echo "  cargo build --bin ion"
echo "  cargo test --lib goal_supervisor"
echo "  cargo clippy --lib 2>&1 | grep goal_supervisor"
echo "  grep -rc $'\xef\xbf\xbd' src/goal_supervisor_extension.rs"
echo ""
echo "To enable in config.json:"
echo '  {"goal_supervisor": {"enabled": true}}'
echo "=========================================="
