#!/usr/bin/env bash
# evolve_tasks.sh — 自进化任务清单（被 auto_evolve_local.sh / evolve_self.sh source）
#
# 任务格式：id | target_file | method_spec | test_spec | test_name | commit_msg
# - id: SE-XX
# - target_file: 要修改的源文件路径
# - method_spec: 给 agent 的方法实现描述（英文，agent 看得懂）
# - test_spec: 测试用例描述
# - test_name: 测试函数名（cargo test --lib <name>）
# - commit_msg: git commit message
#
# 用法：
#   source scripts/evolve_tasks.sh
#   for task in "${TASKS[@]}"; do
#     IFS='|' read -r id file method test testname commit <<< "$task"
#     ...
#   done

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

# 按 ID 查找任务，返回完整 task 字符串（找不到返回空）
find_task() {
    local task_id="$1"
    for task in "${TASKS[@]}"; do
        if [[ "$task" == "$task_id|"* ]]; then
            echo "$task"
            return 0
        fi
    done
    return 1
}
