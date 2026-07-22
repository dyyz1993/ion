#!/usr/bin/env bash
# evolve_concurrent.sh — 一 container 多 B 并发（最节省资源）
#
# 核心优化：
#   1. 一个 container 只编译一次（~3 分钟，复用 cache）
#   2. container 内创建 N 个 worktree 子目录（共享 ion binary）
#   3. N 个 B 进程并行改代码（只 edit + commit，不跑 cargo）
#   4. A 在 host 上统一 cargo build + test（一次验证所有改动）
#
# 预计耗时：3 分钟（编译）+ 2 分钟（B 并行）+ 1 分钟（A 验证）= ~6 分钟 / 5 任务
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
CONTAINER_NAME=""
WT_DIR=""

# ── 任务清单（每个改不同文件！）──────────────────────────────────
TASKS=(
    "C-01|src/command_guard.rs|Add pub fn pattern_count() -> usize that returns count of risk patterns. Just return the length of the internal patterns list.|Call pattern_count(), assert > 0|test_pattern_count|feat(guard): pattern_count"
    "C-02|src/auth.rs|Add pub fn provider_count() -> usize to AuthStorage that returns provider_base_urls.len().|Create AuthStorage with 3 providers, assert provider_count()==3|test_provider_count|feat(auth): provider_count"
    "C-03|src/session_index.rs|Add pub fn has_sessions() -> bool that checks if sessions.index.json exists and is non-empty.|Call has_sessions() with and without index file|test_has_sessions|feat(session): has_sessions"
    "C-04|src/agent_config.rs|Add pub fn builtin_agent_count() -> usize that returns count of builtin agents (call builtin_agents().len()).|Call builtin_agent_count(), assert >= 3|test_builtin_agent_count|feat(agent): builtin_agent_count"
    "C-05|src/paths.rs|Add pub fn tmp_dir() -> std::path::PathBuf that returns the system temp directory path (std::env::temp_dir()).|Call tmp_dir(), assert path exists|test_tmp_dir|feat(paths): tmp_dir"
)

NUM_TASKS=${#TASKS[@]}

echo ""
echo "=========================================="
echo "  Concurrent Self-Evolution"
echo "  (1 container, N parallel B workers)"
echo "=========================================="
echo "  Tasks:    $NUM_TASKS"
echo "  Model:    $MODEL"
echo "  Provider: $PROVIDER"
echo "=========================================="
echo ""

# ── Phase 1: 启动 container + 编译 ────────────────────────────────
echo "Phase 1: Starting container + compiling ion..."

# 先用 evolve.sh 的逻辑启动 container
ION_TOOL_TIMEOUT=1800 bash "$PROJECT_DIR/scripts/evolve.sh" > /dev/null 2>&1
source /tmp/.evolver-state

if [ "$BUILD_STATUS" != "OK" ]; then
    echo "❌ Container build failed"
    exit 1
fi
echo "  ✅ Container ready: $CONTAINER_NAME"
echo "  ✅ ion binary compiled"

# ── Phase 2: 创建 N 个 worktree 子目录 ────────────────────────────
echo ""
echo "Phase 2: Creating $NUM_TASKS worktree subdirectories in container..."

for i in "${!TASKS[@]}"; do
    IFS='|' read -r id target_file rest <<< "${TASKS[$i]}"
    n=$((i + 1))
    # 在 container 内创建 worktree 子目录（rsync 源码，不含 target/.git）
    container exec "$CONTAINER_NAME" sh -c "
        rm -rf /workspace/wt-$n
        mkdir -p /workspace/wt-$n
        # 复制源码（不含 target、.git）
        cd /workspace && for f in src Cargo.toml Cargo.lock; do
            [ -e \"\$f\" ] && cp -r \"\$f\" /workspace/wt-$n/ 2>/dev/null
        done
        # 如果有 ion-provider link，复制 Cargo.toml 里的 path 引用
        # 创建独立 git repo
        cd /workspace/wt-$n && git init -q && git config user.email 'ion@evolver' && git config user.name 'Evolver'
        git add -A && git commit -q -m 'init' 2>/dev/null
    " 2>/dev/null
    echo "  [$id] wt-$n ready"
done

# ── Phase 3: 并行发任务（B 只改代码，不跑 cargo）──────────────────
echo ""
echo "Phase 3: Dispatching $NUM_TASKS tasks in PARALLEL (B only edits, no cargo)..."

for i in "${!TASKS[@]}"; do
    IFS='|' read -r id target_file method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"
    n=$((i + 1))

    # B 的 prompt：只改代码 + commit，不跑 cargo
    prompt="Task: Add a new method to $target_file.

METHOD SPEC:
$method_spec

TEST SPEC:
$test_spec

CRITICAL RULES:
1. Use edit tool, not bash sed
2. ALL comments in ENGLISH ONLY
3. Only ADD new code, do NOT modify existing lines
4. Do NOT run cargo check or cargo test (A will verify on host)
5. After editing: git add $target_file && git commit -m '$commit_msg'
6. grep -c \$'\xef\xbf\xbd' $target_file (must be 0)"

    echo "  [$id] Dispatching developer to wt-$n..."

    # 后台并行：developer 改代码 → reviewer 审查
    (
        # Step 1: developer 改代码
        dev_result=$(echo "$prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
            "cd /workspace/wt-$n && /workspace/target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10)

        # Step 2: reviewer 审查（在同一 worktree 里读改动）
        review_prompt="Review the latest changes in $target_file. Run: git diff HEAD~1 HEAD -- $target_file

Check:
1. Correctness: Is the SQL/logic correct?
2. Error handling: Are errors properly propagated?
3. Edge cases: Empty table? Null values? Invalid input?
4. Test coverage: Does the test cover the main scenarios?
5. Code style: Is it consistent with surrounding code?

Report APPROVE or REQUEST_CHANGES with specific issues."

        review_result=$(echo "$review_prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
            "cd /workspace/wt-$n && /workspace/target/release/ion --agent reviewer --provider $PROVIDER --model $MODEL" 2>&1 | tail -15)

        # 合并结果
        echo "=== DEVELOPER ===" > "/tmp/par_result_${id}.txt"
        echo "$dev_result" >> "/tmp/par_result_${id}.txt"
        echo "" >> "/tmp/par_result_${id}.txt"
        echo "=== REVIEWER ===" >> "/tmp/par_result_${id}.txt"
        echo "$review_result" >> "/tmp/par_result_${id}.txt"

        # 检查 reviewer 是否 APPROVE
        if echo "$review_result" | grep -qi "APPROVE"; then
            echo "REVIEW_APPROVED" >> "/tmp/par_result_${id}.txt"
        else
            echo "REVIEW_REJECTED" >> "/tmp/par_result_${id}.txt"
        fi
    ) &

    echo "  [$id] dispatched (PID $!)"
done

echo ""
echo "All $NUM_TASKS B workers (developer + reviewer) running in parallel. Waiting..."
wait
echo "All B workers complete."

# ── Phase 4: A 统一同步 + 验证 + commit ───────────────────────────
echo ""
echo "Phase 4: A syncing + verifying + committing (sequential)..."

SUCCESS=0
FAIL=0

# 先同步所有改动到主仓库
for i in "${!TASKS[@]}"; do
    IFS='|' read -r id target_file method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"
    n=$((i + 1))

    echo ""
    echo "  [$id] Processing ($target_file)..."

    # B 输出（含 developer + reviewer 结果）
    if [ -f "/tmp/par_result_${id}.txt" ]; then
        echo "  [$id] Developer + Reviewer output:"
        cat "/tmp/par_result_${id}.txt" | sed 's/^/    /'
    fi

    # Auto-fix loop: send reviewer feedback back to developer (max 2 rounds)
    if ! grep -q "REVIEW_APPROVED" "/tmp/par_result_${id}.txt" 2>/dev/null; then
        echo "  [$id] ⚠️ Reviewer rejected. Starting auto-fix (max 2 rounds)..."

        for fix_round in 1 2; do
            echo "  [$id] Auto-fix round $fix_round/2..."

            # Extract reviewer feedback from the previous result
            reviewer_feedback=$(sed -n '/=== REVIEWER ===/,/REVIEW_\(APPROVED\|REJECTED\)/p' "/tmp/par_result_${id}.txt" 2>/dev/null | sed '/REVIEW_\(APPROVED\|REJECTED\)/d' | sed '/=== REVIEWER ===/d')

            # Build the fix prompt: tell developer what reviewer found
            fix_prompt="Your previous changes to $target_file were REJECTED by the reviewer.

REVIEWER FEEDBACK:
$reviewer_feedback

Fix ALL issues listed above. Rules:
1. Use edit tool, not bash sed
2. ALL comments in ENGLISH ONLY
3. Only modify what the reviewer flagged, do NOT break other code
4. After fixing: git add $target_file && git commit -m '${commit_msg} (fix round $fix_round)'
5. grep -c \$'\xef\xbf\xbd' $target_file (must be 0)"

            echo "  [$id] Sending fix request to developer..."

            # Developer attempts the fix
            fix_result=$(echo "$fix_prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
                "cd /workspace/wt-$n && /workspace/target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10)

            # Reviewer re-reviews the fix
            re_review_prompt="Review the latest fix to $target_file. Run: git diff HEAD~1 HEAD -- $target_file

The previous review found these issues:
$reviewer_feedback

Verify each issue is resolved. Report APPROVE or REQUEST_CHANGES."

            re_review_result=$(echo "$re_review_prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
                "cd /workspace/wt-$n && /workspace/target/release/ion --agent reviewer --provider $PROVIDER --model $MODEL" 2>&1 | tail -15)

            # Update result file
            echo "" >> "/tmp/par_result_${id}.txt"
            echo "=== FIX ROUND $fix_round ===" >> "/tmp/par_result_${id}.txt"
            echo "--- Developer Fix ---" >> "/tmp/par_result_${id}.txt"
            echo "$fix_result" >> "/tmp/par_result_${id}.txt"
            echo "" >> "/tmp/par_result_${id}.txt"
            echo "--- Reviewer Re-review ---" >> "/tmp/par_result_${id}.txt"
            echo "$re_review_result" >> "/tmp/par_result_${id}.txt"

            if echo "$re_review_result" | grep -qi "APPROVE"; then
                echo "REVIEW_APPROVED" >> "/tmp/par_result_${id}.txt"
                echo "  [$id] ✅ Auto-fix round $fix_round approved!"
                break
            else
                echo "REVIEW_REJECTED" >> "/tmp/par_result_${id}.txt"
                if [ "$fix_round" -eq 2 ]; then
                    echo "  [$id] ❌ Auto-fix failed after 2 rounds"
                else
                    echo "  [$id] ⚠️ Round $fix_round rejected, trying again..."
                fi
            fi
        done

        # Final check after auto-fix attempts
        if ! grep -q "REVIEW_APPROVED" "/tmp/par_result_${id}.txt" 2>/dev/null; then
            echo "  [$id] ❌ Reviewer still NOT approved after auto-fix. Skipping."
            FAIL=$((FAIL + 1))
            continue
        fi
    fi
    echo "  [$id] ✅ Reviewer approved"

    # Fetch changes from the container worktree subdirectory
    # 先看 B 改了啥
    changed=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "cd /workspace/wt-$n && git diff --name-only HEAD~1 HEAD 2>/dev/null || git diff --name-only 2>/dev/null" 2>/dev/null)
    if [ -z "$changed" ]; then
        echo "  [$id] ❌ No changes detected"
        FAIL=$((FAIL + 1))
        continue
    fi
    echo "  [$id] Changed files: $changed"

    # 守门
    ufffd=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "grep -c \$'\\xef\\xbf\\xbd' /workspace/wt-$n/$target_file 2>/dev/null || true" 2>/dev/null | head -1 | tr -d '[:space:]')
    if [ "$ufffd" != "0" ]; then
        echo "  [$id] ❌ Gate FAILED ($ufffd U+FFFD)"
        FAIL=$((FAIL + 1))
        continue
    fi

    # 同步到主仓库
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "cat /workspace/wt-$n/$target_file" > "$PROJECT_DIR/$target_file" 2>/dev/null
    echo "  [$id] ✅ Synced"
done

# A 在主仓库统一验证
echo ""
echo "  [A] Running cargo build + test on host (unified verification)..."
cd "$PROJECT_DIR"
build_out=$(cargo build --bin ion 2>&1)
if echo "$build_out" | grep -q "Finished"; then
    echo "  [A] ✅ cargo build OK"
else
    echo "  [A] ❌ cargo build FAILED"
    echo "$build_out" | grep "error" | head -3
    # 回滚所有改动
    for i in "${!TASKS[@]}"; do
        IFS='|' read -r id target_file rest <<< "${TASKS[$i]}"
        git checkout -- "$target_file" 2>/dev/null
    done
    exit 1
fi

test_out=$(cargo test --lib 2>&1)
if echo "$test_out" | grep -q "test result: ok"; then
    passed=$(echo "$test_out" | grep -oE "[0-9]+ passed" | head -1)
    echo "  [A] ✅ cargo test: $passed"
else
    echo "  [A] ❌ cargo test FAILED"
    echo "$test_out" | grep "FAILED" | head -3
    exit 1
fi

# 逐个 commit
for i in "${!TASKS[@]}"; do
    IFS='|' read -r id target_file method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"

    if git diff --stat -- "$target_file" | grep -q "$target_file"; then
        git add "$target_file"
        git commit -m "$commit_msg (concurrent self-evolution)" 2>&1 | head -1
        echo "  [$id] ✅ Committed"

        # 导出 HTML
        SID=$(head -1 "$HOME/.ion/agent/sessions/"*workspace*"/session.jsonl" 2>/dev/null | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('id',''))" 2>/dev/null | head -1)
        if [ -n "$SID" ]; then
            mkdir -p /tmp/evolve_concurrent_reports
            "$PROJECT_DIR/target/debug/ion" --export "/tmp/evolve_concurrent_reports/report_${id}.html" --session "$SID" 2>/dev/null
        fi

        SUCCESS=$((SUCCESS + 1))
    else
        echo "  [$id] ⚠️ No changes to commit"
        FAIL=$((FAIL + 1))
    fi
done

# ── Phase 5: 清理 ─────────────────────────────────────────────────
echo ""
echo "Phase 5: Cleanup..."
# 清理 container 内的 wt 子目录
for i in "${!TASKS[@]}"; do
    n=$((i + 1))
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "rm -rf /workspace/wt-$n" 2>/dev/null
done
# 停 container
"$CONTAINER_BIN" stop "$CONTAINER_NAME" 2>/dev/null
rm -rf "$WT_DIR" 2>/dev/null
cd "$PROJECT_DIR"
git worktree prune
rm -f /tmp/par_result_*.txt

# ── 总结 ──────────────────────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Concurrent Complete"
echo "=========================================="
echo "  Success: $SUCCESS / $((SUCCESS + FAIL))"
[ $FAIL -gt 0 ] && echo "  Failed:  $FAIL"
echo "  Reports: /tmp/evolve_concurrent_reports/"
echo "=========================================="