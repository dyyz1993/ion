#!/usr/bin/env bash
# evolve_self.sh — 自进化批量任务（B 改 ION 自己的源码）
#
# 跟 evolve_batch.sh 区别：
#   - 任务目标是 ION 自己的各个模块（不只是 global_memory.rs）
#   - B 只跑 cargo check（语法检查），不跑 cargo build（太慢）
#   - A 在主仓库跑 cargo build + cargo test --lib 完整验证
#
# Usage: bash scripts/evolve_self.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
# 默认用 GLM-5.2（主力模型，代码质量更好）
# 快速任务（如纯测试）可设 MODEL=deepseek-v4-flash PROVIDER=opencode
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
HTML_DIR="/tmp/evolve_self_reports"
mkdir -p "$HTML_DIR"

source /tmp/.evolver-state 2>/dev/null
if [ -z "$CONTAINER_NAME" ] || [ -z "$WT_DIR" ]; then
    echo "ERROR: /tmp/.evolver-state missing. Run scripts/evolve.sh first."
    exit 1
fi

# ── 自进化任务清单（改 ION 自己的源码）─────────────────────────
# 格式：id | target_file | method_spec | test_spec | test_name | commit_msg
TASKS=(
    "SE-01|src/command_guard.rs|Add a pub fn list_blocked_patterns() -> Vec<&'static str> that returns the list of high-risk command patterns (like 'sed -i', 'python3 -c', etc). Just collect the patterns from the existing RISK_PATTERNS or similar constant and return them as a Vec of str slices.|Test: call list_blocked_patterns(), assert it contains 'sed -i' and the result is not empty|test_list_blocked_patterns|feat(guard): list_blocked_patterns method + test"
    "SE-02|src/session_index.rs|Add a pub fn count_sessions_by_project(project_key: &str) -> Result<i64, String> to SessionIndex. Load ~/.ion/agent/sessions.index.json, count entries where project matches the given key.|Test: create temp index file with known data, call count_sessions_by_project, verify count|test_count_sessions_by_project|feat(session): count_sessions_by_project method + test"
    "SE-03|src/agent/compact.rs|Add a pub fn estimate_compact_tokens(messages: &[Message]) -> usize that estimates total token count of a message list by summing content lengths / 4 (rough heuristic). Iterate messages, sum content text length, divide by 4.|Test: create 3 messages with known content, call estimate_compact_tokens, verify result matches (total_chars / 4)|test_estimate_compact_tokens|feat(compact): estimate_compact_tokens method + test"
    "SE-04|src/auth.rs|Add a pub fn list_providers() -> Vec<String> to AuthStorage that returns the list of provider names from provider_base_urls. Just collect the keys of the HashMap.|Test: create AuthStorage with known provider_base_urls, call list_providers, verify it returns expected providers|test_list_providers|feat(auth): list_providers method + test"
    "SE-05|src/paths.rs|Add a pub fn extensions_size() -> Result<u64, String> that calculates total size in bytes of ~/.ion/agent/extensions/ directory. Walk the dir recursively, sum file sizes. Return Err if dir doesn't exist.|Test: create temp extensions dir with known files, call extensions_size, verify size matches|test_extensions_size|feat(paths): extensions_size method + test"
    "SE-06|src/agent_config.rs|Add a pub fn count_builtin_agents() -> usize that counts the number of builtin agents (call builtin_agents() and return its len).|Test: call count_builtin_agents(), assert result >= 3 (build/explore/plan/improver)|test_count_builtin_agents|feat(agent): count_builtin_agents method + test"
    "SE-07|src/file_snapshot/object_store.rs|Add a pub fn store_count(&self) -> Result<usize, String> to ObjectStore that counts the number of objects in the store directory. List files in the objects dir, return count.|Test: create temp ObjectStore with known files, call store_count, verify|test_store_count|feat(snapshot): store_count method + test"
    "SE-08|src/session_jsonl.rs|Add a pub fn count_entries_by_type(file_path: &Path, entry_type: &str) -> Result<usize, String> that reads a session JSONL file and counts entries matching the given type (e.g. 'message', 'turn_summary', 'custom').|Test: create temp JSONL file with known entries, call count_entries_by_type for each type, verify counts|test_count_entries_by_type|feat(session): count_entries_by_type method + test"
    "SE-09|src/message_retrieval.rs|Add a pub fn count_turns(messages: &[serde_json::Value]) -> usize that counts how many turns (user-assistant pairs) exist in a message list. Iterate, count user messages that start a new turn.|Test: create messages with 3 user + 3 assistant, call count_turns, verify result is 3|test_count_turns|feat(msg): count_turns method + test"
    "SE-10|src/agent/tool.rs|Add a new tool called RandomNumberTool to src/agent/tool.rs. Struct with no fields. Implement Tool trait: name() returns 'random', description() returns 'Generate a random number in [0, max). Args: max (number, default 100).', parameters() accepts max. execute() uses rand::random::<u32>() % max as u64.|Test: create RandomNumberTool, call execute with max=10, verify result is a number between 0 and 9|test_random_number_tool|feat(tool): RandomNumberTool + test"
    "SE-11|src/storage_context.rs|Add a pub fn project_data_size(&self) -> Result<u64, String> to StorageContext that calculates total size of the project-data directory for this context. Walk the dir recursively, sum file sizes.|Test: create temp StorageContext with known files, call project_data_size, verify|test_project_data_size|feat(storage): project_data_size method + test"
    "SE-12|src/global_memory_ext.rs|Add a pub fn extension_status() -> serde_json::Value that returns a JSON object with status info: {enabled, db_exists, db_path}. Read config.json to check if enabled, check if db file exists.|Test: call extension_status(), verify it returns a JSON object with 'enabled' field|test_extension_status|feat(memory): extension_status method + test"
    "SE-13|src/worker_api.rs|Add a pub fn worker_count(&self) -> usize to WorkerHandleRegistry (or equivalent) that returns the current number of registered workers. Just return self.workers.len() or similar.|Test: create registry, add/remove workers, verify count|test_worker_count|feat(worker): worker_count method + test"
    "SE-14|src/runtime.rs|Add a pub fn is_peer(&self) -> bool to SpawnWorkerRequest that returns true if relation is Peer. Match on self.relation, return true for SpawnRelation::Peer, false otherwise.|Test: create SpawnWorkerRequest with Peer, verify is_peer()==true; create with Child, verify is_peer()==false|test_is_peer|feat(runtime): is_peer method + test"
    "SE-15|src/session_tree.rs|Add a pub fn count_branches(entries: &[serde_json::Value]) -> usize that counts the number of branches in a session tree. A branch is identified by entries with different parentId chains. Count unique leaf nodes.|Test: create entries with known tree structure, call count_branches, verify|test_count_branches|feat(session): count_branches method + test"
    "SE-16|src/hooks/mod.rs|Add a pub fn count_hooks(&self) -> usize to HooksConfig that returns the total number of hooks across all events. Iterate self.hooks values, sum lengths.|Test: create HooksConfig with known hooks, call count_hooks, verify total|test_count_hooks|feat(hooks): count_hooks method + test"
    "SE-17|src/mcp/mod.rs|Add a pub fn connected_server_count(&self) -> usize to McpManager that returns the number of currently connected MCP servers. Just return self.connections.len() or similar.|Test: this may need mocking; skip if McpManager struct is too complex. Instead add a simple pub fn server_count_in_config(config: &IonConfig) -> usize that counts mcp_servers keys|test_server_count_in_config|feat(mcp): server_count_in_config method + test"
    "SE-18|src/agent/extension.rs|Add a pub fn loaded_extension_names(&self) -> Vec<String> to ExtensionRegistry that returns the list of loaded extension names. Iterate self.extensions, collect keys.|Test: create registry, register known extensions, call loaded_extension_names, verify|test_loaded_extension_names|feat(ext): loaded_extension_names method + test"
    "SE-19|src/agent/tool.rs|Add a new tool called UuidGeneratorTool. Struct with no fields. Implement Tool trait: name() returns 'uuid', description() returns 'Generate a UUID v4 string.', parameters() returns empty required. execute() uses uuid::Uuid::new_v4().to_string().|Test: create UuidGeneratorTool, call execute, verify result is 36 chars and contains 4 dashes|test_uuid_generator_tool|feat(tool): UuidGeneratorTool + test"
    "SE-20|src/agent/agent_loop.rs|Add a pub fn current_message_count(&self) -> usize to Agent that returns self.messages.len(). Simple getter for diagnostic purposes.|Test: create Agent, push some messages, call current_message_count, verify|test_current_message_count|feat(agent): current_message_count getter + test"
)

echo ""
echo "=========================================="
echo "  Self-Evolution Batch Run"
echo "=========================================="
echo "  Container: $CONTAINER_NAME"
echo "  Tasks:     ${#TASKS[@]}"
echo "=========================================="
echo ""

build_prompt() {
    local id="$1" target_file="$2" method_spec="$3" test_spec="$4" test_name="$5" commit_msg="$6"
    cat <<EOF
Task: Add a new method/function to $target_file.

METHOD SPEC:
$method_spec

TEST SPEC:
$test_spec

CRITICAL RULES (violation = task failure):
1. Use the edit tool. Do NOT use bash sed.
2. ALL comments MUST be in ENGLISH ONLY. Do NOT write Chinese characters.
3. Only ADD new code. Do NOT modify existing lines outside the new function.
4. After editing, run: cargo check 2>&1 | tail -5
   (Use cargo check, NOT cargo build - it's faster)
5. If cargo check succeeds (no error), run: cargo test --lib $test_name 2>&1 | tail -5
6. If tests pass, run: git add $target_file && git commit -m "$commit_msg"
7. Run: grep -c \$'\xef\xbf\xbd' $target_file
   The output MUST be 0.
EOF
}

run_b_task() {
    local prompt="$1"
    echo "$prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
        "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -15
}

fix_ufffd_in_b() {
    local count="$1" target_file="$2"
    echo "  [A] Gate REJECTED ($count U+FFFD in $target_file). Driving B to self-fix..."
    local fix_prompt="Your $target_file contains $count U+FFFD garbled chars. Run: grep -n \$'\xef\xbf\xbd' $target_file to find them. Use the edit tool to fix each line (replace U+FFFD with correct characters). After fixing: grep -c \$'\xef\xbf\xbd' $target_file (must be 0); cargo check; git add $target_file && git commit --amend --no-edit"
    echo "$fix_prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
        "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10
}

gate_check_file() {
    local target_file="$1"
    # 修复：grep -c 返回非零 exit code 当无匹配，但输出 "0"。 || true 避免 ERR trap
    # 只取第一行（避免 "0\n0" 当 file 不存在）
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "grep -c \$'\\xef\\xbf\\xbd' /workspace/$target_file 2>/dev/null || true" 2>/dev/null | head -1 | tr -d '[:space:]'
}

sync_and_commit() {
    local id="$1" target_file="$2" commit_msg="$3"
    cd "$PROJECT_DIR"

    cp "$WT_DIR/$target_file" "$PROJECT_DIR/$target_file"

    # cargo build in main repo (full verification)
    local build_out
    build_out=$(cargo build --bin ion 2>&1)
    if ! echo "$build_out" | grep -q "Finished"; then
        echo "  [A] ❌ Main repo cargo build FAILED"
        echo "$build_out" | grep -E "error" | head -3
        git checkout -- "$target_file"  # revert
        return 1
    fi
    echo "  [A] ✅ Main repo cargo build OK"

    # cargo test --lib
    local test_out
    test_out=$(cargo test --lib 2>&1)
    if ! echo "$test_out" | grep -q "test result: ok"; then
        echo "  [A] ❌ Main repo cargo test FAILED"
        echo "$test_out" | tail -5
        git checkout -- "$target_file"
        return 1
    fi
    local passed=$(echo "$test_out" | grep -oE "[0-9]+ passed" | head -1)
    echo "  [A] ✅ cargo test --lib: $passed"

    git add "$target_file"
    git commit -m "$commit_msg (self-evolution)" 2>&1 | head -1
    return 0
}

# ── 主循环 ────────────────────────────────────────────────────────
SUCCESS=0
FAIL=0
FAILED_TASKS=()

for i in "${!TASKS[@]}"; do
    IFS='|' read -r id target_file method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"
    echo ""
    echo "──────────────────────────────────────────"
    echo "$id: $commit_msg"
    echo "  Target: $target_file"
    echo "──────────────────────────────────────────"

    # Step 1: A 驱动 B 加方法
    echo "  [A→B] Sending task to B..."
    prompt=$(build_prompt "$id" "$target_file" "$method_spec" "$test_spec" "$test_name" "$commit_msg")
    run_b_task "$prompt"

    # Step 2: 守门
    ufffd=$(gate_check_file "$target_file")
    attempt=0
    while [ "$ufffd" != "0" ] && [ $attempt -lt 2 ]; do
        attempt=$((attempt + 1))
        echo "  [A] Gate check: $ufffd U+FFFD in $target_file (attempt $attempt/2)"
        fix_ufffd_in_b "$ufffd" "$target_file"
        ufffd=$(gate_check_file "$target_file")
    done

    if [ "$ufffd" != "0" ]; then
        echo "  [A] ❌ Gate FAILED after 2 attempts. Skipping $id."
        FAIL=$((FAIL + 1))
        FAILED_TASKS+=("$id")
        continue
    fi
    echo "  [A] ✅ Gate passed (0 U+FFFD)"

    # Step 3: 同步 + 主仓库验证 + commit
    if sync_and_commit "$id" "$target_file" "$commit_msg"; then
        SUCCESS=$((SUCCESS + 1))

        # 导出 HTML（去掉 local，直接用普通变量）
        SID=$(head -1 "$HOME/.ion/agent/sessions/"*workspace*"/session.jsonl" 2>/dev/null | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('id',''))" 2>/dev/null | head -1)
        if [ -n "$SID" ]; then
            "$PROJECT_DIR/target/debug/ion" --export "$HTML_DIR/report_${id}.html" --session "$SID" 2>/dev/null
        fi

        echo "  [A] ✅ $id complete"
    else
        FAIL=$((FAIL + 1))
        FAILED_TASKS+=("$id")
    fi
done

# ── 总结 ──────────────────────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Self-Evolution Complete"
echo "=========================================="
echo "  Success: $SUCCESS / $((SUCCESS + FAIL))"
if [ $FAIL -gt 0 ]; then
    echo "  Failed:  ${FAILED_TASKS[*]}"
fi
echo "=========================================="