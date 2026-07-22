#!/usr/bin/env bash
# evolve_batch.sh — 批量跑 A→B 闭环（一次启 container，跑 N 个任务）
#
# Usage: bash scripts/evolve_batch.sh
#
# 任务清单硬编码在脚本里（TASKS 数组），每个任务定义：
#   - name: 方法名（用于 commit message）
#   - method_spec: 方法的功能描述 + 签名（B 自己设计 SQL）
#   - test_spec: 测试要点
#   - test_name: 跑哪个 test 验证
#
# A 流程（每个任务）：
#   1. 用 stdin 喂 prompt 给 B（spec + 全英文 comment 规则）
#   2. 守门检查 U+FFFD
#   3. 如拦截：驱动 B 自修复（最多 2 轮）
#   4. 通过：同步到主仓库 + 主仓库 cargo test + git commit + 导出 HTML
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
HTML_DIR="/tmp/evolve_reports"
mkdir -p "$HTML_DIR"

source /tmp/.evolver-state 2>/dev/null
if [ -z "$CONTAINER_NAME" ] || [ -z "$WT_DIR" ]; then
    echo "ERROR: /tmp/.evolver-state missing. Run scripts/evolve.sh first."
    exit 1
fi

# ── 任务清单（10 个，从简单到复杂）────────────────────────────────
# 每行用 '|' 分隔：name | method_spec | test_spec | test_name | commit_msg
TASKS=(
    "has_tag|Add method has_tag(tag: &str) -> Result<bool, String>. Check if any entry's tags column contains the given tag (use LIKE '%tag%'). Return true if at least one match exists, false otherwise.|clear_all; save entry with tags='rust,sqlite'; assert has_tag('rust')==true; assert has_tag('java')==false|test_has_tag|feat(memory): has_tag method + test"
    "find_by_content_prefix|Add method find_by_content_prefix(prefix: &str) -> Result<Vec<GlobalMemoryEntry>, String>. Return all entries whose content starts with the given prefix (use content LIKE 'prefix%'). Order by created_at DESC.|clear_all; save 'hello world'; save 'hello rust'; save 'goodbye'; assert find_by_content_prefix('hello').len()==2; assert find_by_content_prefix('xyz').is_empty()|test_find_by_content_prefix|feat(memory): find_by_content_prefix method + test"
    "update_importance|Add method update_importance(id: &str, importance: i32) -> Result<(), String>. UPDATE entries SET importance=?2 WHERE id=?1. Return Err if 0 rows affected (id not found).|clear_all; let id = save(...); update_importance(&id, 9); re-fetch and verify importance==9; update_importance('nonexistent', 5) should return Err|test_update_importance|feat(memory): update_importance method + test"
    "archive_by_project|Add method archive_by_project(project: &str) -> Result<usize, String>. UPDATE entries SET archived=1 WHERE project=?1. Return number of rows updated.|clear_all; save 2 to proj-a; save 1 to proj-b; archive_by_project('proj-a')==2; count_active_by_project('proj-a')==0; count_active_by_project('proj-b')==1|test_archive_by_project|feat(memory): archive_by_project method + test"
    "count_archived|Add method count_archived() -> Result<i64, String>. Return total count of archived=1 entries across all projects.|clear_all; save 3; archive first (use forget); count_archived()==1; forget second; count_archived()==2|test_count_archived|feat(memory): count_archived method + test"
    "find_duplicates|Add method find_duplicates() -> Result<Vec<GlobalMemoryEntry>, String>. Return entries whose content_hash appears more than once in the table (use GROUP BY content_hash HAVING COUNT(*)>1). Return one entry per duplicate group (any representative).|clear_all; save 'same content'; save 'same content'; save 'unique'; result = find_duplicates(); result.len() >= 1 (at least one duplicate group); each result entry's content == 'same content'|test_find_duplicates|feat(memory): find_duplicates method + test"
    "list_recent_by_project|Add method list_recent_by_project(project: &str, limit: usize) -> Result<Vec<GlobalMemoryEntry>, String>. Return up to N most recent entries (created_at DESC) for the given project.|clear_all; save 3 to proj-a with 1s sleep between; result = list_recent_by_project('proj-a', 2); result.len()==2; result[0].created_at >= result[1].created_at (DESC order)|test_list_recent_by_project|feat(memory): list_recent_by_project method + test"
    "batch_save|Add method batch_save(entries: Vec<(&str, &str, &str, &str, i32)>) -> Result<Vec<String>, String>. Save multiple entries in a single transaction. Each tuple = (content, category, tags, project, importance). Return list of generated IDs. If any save fails, rollback all.|clear_all; batch_save(vec![('a','note','t','p',5), ('b','note','t','p',5), ('c','note','t','p',5)]); count()==3; returned ids.len()==3|test_batch_save|feat(memory): batch_save method + test"
    "import_json|Add method import_json(json_str: &str) -> Result<usize, String>. Parse JSON array of objects with fields {content, category, tags, project, importance}. Save each. Return count imported. Invalid JSON returns Err.|clear_all; let json = '[{\"content\":\"a\",\"category\":\"note\",\"tags\":\"t\",\"project\":\"p\",\"importance\":5}]'; import_json(json)==1; count()==1; import_json('invalid') returns Err|test_import_json|feat(memory): import_json method + test"
    "export_json|Add method export_json(filter_project: Option<&str>) -> Result<String, String>. Return JSON array of all entries (or filtered by project). Each entry as {id, content, category, project, archived, created_at}.|clear_all; save 2 to proj-a; save 1 to proj-b; let json = export_json(Some('proj-a')); parse json, array.len()==2; let all = export_json(None); parse, array.len()==3|test_export_json|feat(memory): export_json method + test"
)

echo ""
echo "=========================================="
echo "  A→B Batch Run"
echo "=========================================="
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo "  Model:     $MODEL"
echo "  Tasks:     ${#TASKS[@]}"
echo "=========================================="
echo ""

# ── Helper：用模板生成 prompt ─────────────────────────────────────
build_prompt() {
    local name="$1" method_spec="$2" test_spec="$3" test_name="$4" commit_msg="$5"
    cat <<EOF
Task: Add a new method to src/global_memory.rs in the GlobalMemoryStore impl block.

METHOD SPEC:
$method_spec

TEST SPEC:
$test_spec

CRITICAL RULES (violation = task failure):
1. Use the edit tool. Do NOT use bash sed.
2. ALL comments you add (method doc, test comments) MUST be in ENGLISH ONLY.
   Do NOT write any Chinese characters in comments - they get corrupted and
   the U+FFFD guard will reject the merge.
3. Do NOT modify any existing lines outside the new method block.
   Only insert new lines. Do NOT touch db_path() or other existing methods.
4. After editing, run: cargo test --lib $test_name
5. If tests pass, run: git add src/global_memory.rs && git commit -m "$commit_msg"
6. Run: grep -c \$'\xef\xbf\xbd' src/global_memory.rs
   The output MUST be 0. If not, fix any U+FFFD before committing.
EOF
}

# ── Helper：A 驱动 B 跑一个任务 ───────────────────────────────────
run_b_task() {
    local prompt="$1"
    echo "$prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
        "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10
}

# ── Helper：A 驱动 B 自修复 U+FFFD ─────────────────────────────────
fix_ufffd_in_b() {
    local count="$1"
    echo "  [A] Gate REJECTED ($count U+FFFD). Driving B to self-fix..."
    local fix_prompt="Your src/global_memory.rs contains $count U+FFFD garbled chars. Run: grep -n \$'\xef\xbf\xbd' src/global_memory.rs to find them. Use the edit tool to fix each line (replace U+FFFD with correct characters). All comments should be valid UTF-8 with no replacement chars. After fixing: grep -c \$'\xef\xbf\xbd' src/global_memory.rs (must be 0); cargo test --lib global_memory; git add src/global_memory.rs && git commit --amend --no-edit"
    echo "$fix_prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
        "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10
}

# ── Helper：守门检查 ──────────────────────────────────────────────
gate_check() {
    # Returns count of U+FFFD in worktree src/. 0 = pass.
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "grep -rc \$'\\xef\\xbf\\xbd' /workspace/src/ 2>/dev/null | awk -F: '{s+=\$NF} END{print s+0}'" 2>/dev/null
}

# ── Helper：同步 + 主仓库验证 + commit + HTML ──────────────────────
sync_and_commit() {
    local name="$1" commit_msg="$2"
    cd "$PROJECT_DIR"

    # Sync
    cp "$WT_DIR/src/global_memory.rs" "$PROJECT_DIR/src/global_memory.rs"

    # Main repo cargo test
    local test_out
    test_out=$(cargo test --lib global_memory 2>&1)
    if ! echo "$test_out" | grep -q "test result: ok"; then
        echo "  [A] ❌ Main repo cargo test FAILED"
        echo "$test_out" | tail -5
        return 1
    fi
    local passed=$(echo "$test_out" | grep -oE "[0-9]+ passed" | head -1)
    echo "  [A] ✅ Main repo cargo test: $passed"

    # Commit
    git add src/global_memory.rs
    git commit -m "$commit_msg (B via container, A→B 闭环)" 2>&1 | head -1

    # Export HTML
    local sid=$(head -1 "$HOME/.ion/agent/sessions/"*workspace*"/session.jsonl" 2>/dev/null | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('id',''))" 2>/dev/null | head -1)
    if [ -n "$sid" ]; then
        "$PROJECT_DIR/target/debug/ion" --export "$HTML_DIR/report_${name}.html" --session "$sid" 2>/dev/null
        echo "  [A] HTML: $HTML_DIR/report_${name}.html"
    fi
    return 0
}

# ── 主循环 ────────────────────────────────────────────────────────
SUCCESS=0
FAIL=0
FAILED_TASKS=()

for i in "${!TASKS[@]}"; do
    IFS='|' read -r name method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"
    task_num=$((i + 5))  # 从任务 5 开始（前 4 个已手动跑完）
    echo ""
    echo "──────────────────────────────────────────"
    echo "Task $task_num: $name"
    echo "──────────────────────────────────────────"

    # Step 1: A 驱动 B 加方法
    echo "  [A→B] Sending task to B..."
    prompt=$(build_prompt "$name" "$method_spec" "$test_spec" "$test_name" "$commit_msg")
    run_b_task "$prompt"

    # Step 2: 守门检查
    ufffd=$(gate_check)
    attempt=0
    while [ "$ufffd" != "0" ] && [ $attempt -lt 2 ]; do
        attempt=$((attempt + 1))
        echo "  [A] Gate check: $ufffd U+FFFD found (attempt $attempt/2)"
        fix_ufffd_in_b "$ufffd"
        ufffd=$(gate_check)
    done

    if [ "$ufffd" != "0" ]; then
        echo "  [A] ❌ Gate FAILED after 2 fix attempts ($ufffd U+FFFD remain). Skipping."
        FAIL=$((FAIL + 1))
        FAILED_TASKS+=("$name")
        continue
    fi
    echo "  [A] ✅ Gate passed (0 U+FFFD)"

    # Step 3: 同步 + 主仓库验证 + commit + HTML
    if sync_and_commit "$name" "$commit_msg"; then
        SUCCESS=$((SUCCESS + 1))
        echo "  [A] ✅ Task $task_num ($name) complete"
    else
        FAIL=$((FAIL + 1))
        FAILED_TASKS+=("$name")
    fi
done

# ── 总结 ──────────────────────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Batch Complete"
echo "=========================================="
echo "  Success: $SUCCESS / $((SUCCESS + FAIL))"
if [ $FAIL -gt 0 ]; then
    echo "  Failed:  ${FAILED_TASKS[*]}"
fi
echo "  HTML reports: $HTML_DIR/"
echo "=========================================="