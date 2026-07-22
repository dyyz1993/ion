#!/usr/bin/env bash
# evolve_parallel.sh — 多 container 并发自进化（macOS bash 3.x 兼容版）
#
# 启动 N 个 container 并行跑 N 个任务（每个任务改不同文件）
# 所有 B 完成后，A 统一守门 + 同步 + cargo test + commit
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
HTML_DIR="/tmp/evolve_parallel_reports"
mkdir -p "$HTML_DIR"

# ── 并发任务清单（每个任务改不同文件！）─────────────────────────
TASKS=(
    "P-01|src/command_guard.rs|Add pub fn count_blocked_patterns() -> usize that returns the number of high-risk patterns. If list_blocked_patterns() exists, return its len(). Otherwise count entries in the RISK_PATTERNS array.|Call count_blocked_patterns(), assert result > 0|test_count_blocked_patterns|feat(guard): count_blocked_patterns"
    "P-02|src/auth.rs|Add pub fn has_provider(provider: &str) -> bool to AuthStorage. Return true if provider_base_urls contains the key.|Create AuthStorage with known providers, assert has_provider('openai')==true, has_provider('unknown')==false|test_has_provider|feat(auth): has_provider"
    "P-03|src/session_index.rs|Add pub fn total_sessions() -> Result<i64, String> that loads sessions.index.json and returns total session count.|Create temp index, call total_sessions, verify count|test_total_sessions|feat(session): total_sessions"
    "P-04|src/agent_config.rs|Add pub fn get_agent_names() -> Vec<String> that returns names of all builtin agents.|Call get_agent_names(), assert it contains 'build' and 'explore'|test_get_agent_names|feat(agent): get_agent_names"
    "P-05|src/paths.rs|Add pub fn cache_size() -> Result<u64, String> that calculates total size of ~/.ion/agent/cache/ directory.|Create temp cache dir, call cache_size, verify|test_cache_size|feat(paths): cache_size"
)

NUM_TASKS=${#TASKS[@]}
echo ""
echo "=========================================="
echo "  Parallel Self-Evolution"
echo "=========================================="
echo "  Tasks:    $NUM_TASKS"
echo "  Model:    $MODEL"
echo "  Provider: $PROVIDER"
echo "=========================================="
echo ""

# ── 找 ion-provider 位置 ──────────────────────────────────────────
GIT_COMMON_DIR=$(cd "$PROJECT_DIR" && git rev-parse --git-common-dir 2>/dev/null || echo "")
HOST_PARENT=""
if [ -n "$GIT_COMMON_DIR" ]; then
    MAIN_REPO=$(cd "$GIT_COMMON_DIR/.." 2>/dev/null && pwd)
    HOST_PARENT=$(dirname "$MAIN_REPO")
fi
ION_PROVIDER_DIR="$HOST_PARENT/ion-provider"

# ── 文件存 container/wt 映射（macOS bash 3.x 不支持 declare -A）────
# 格式：task_id container_name wt_dir
STATE_FILE="/tmp/evolve_parallel_state.txt"
> "$STATE_FILE"

# ── Phase 1: 启动 container（串行启动，但很快因为共享 volume cache）──
echo "Phase 1: Starting $NUM_TASKS containers..."
echo ""

for i in "${!TASKS[@]}"; do
    IFS='|' read -r id target_file method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"
    timestamp=$(date +%Y%m%d-%H%M%S)
    container_name="ion-par-${id}-${timestamp}"
    wt_dir=$(mktemp -d "/tmp/ion-par-${id}-XXXXXX")

    echo -n "  [$id] Starting container..."

    # 用 rsync 复制源码（替代 git worktree——更稳定，不依赖 host gitdir）
    rsync -a --exclude="target" --exclude=".git" --exclude="node_modules" "$PROJECT_DIR/" "$wt_dir/"
    # 转成独立 git repo（让 container 里 git 可用）
    (cd "$wt_dir" && git init -q && git config user.email 'ion@evolver' && git config user.name 'Evolver' && git add -A && git commit -q -m 'init' 2>/dev/null)

    # 启 container（bind mount host cache）
    mkdir -p /tmp/ion-cache-cargo
    local_target_dir="/tmp/ion-cache-target-${id}"
    mkdir -p "$local_target_dir"

    local_cmd=("$CONTAINER_BIN" run --name "$container_name" --detach --rm --network default)
    local_cmd+=("-v" "${wt_dir}:/workspace" -w /workspace --memory 4G --cpus 4)
    [ -d "$ION_PROVIDER_DIR" ] && local_cmd+=("-v" "${ION_PROVIDER_DIR}:/ion-provider")
    local_cmd+=("-v" "${HOME}/.ion:/root/.ion")
    local_cmd+=("-v" "/tmp/ion-cache-cargo:/root/.cargo/registry")  # 共享 cargo 下载缓存
    local_cmd+=("-v" "${local_target_dir}:/workspace/target")       # 独立 target
    local_cmd+=("ion-evolve-rust:latest" sh -lc "sleep infinity")
    "${local_cmd[@]}" 2>&1 | tail -1

    # 修 Cargo.toml
    if [ -d "$ION_PROVIDER_DIR" ]; then
        "$CONTAINER_BIN" exec "$container_name" sh -c \
            "cd /workspace && sed -i 's|path = \"../ion-provider\"|path = \"/ion-provider\"|' Cargo.toml" 2>/dev/null
    fi

    # 记录映射
    echo "$id $container_name $wt_dir $target_file $test_name $commit_msg" >> "$STATE_FILE"
    echo " done ($container_name)"
done

echo ""
echo "Phase 2: Compiling ion in each container (parallel)..."
while read -r sline; do
    sid=$(echo "$sline" | awk '{print $1}')
    scn=$(echo "$sline" | awk '{print $2}')
    (
        "$CONTAINER_BIN" exec "$scn" sh -c \
            'source $HOME/.cargo/env && cd /workspace && cargo build --release --bin ion 2>&1 | tail -1' 2>/dev/null
        echo "$sid compile done"
    ) &
done < "$STATE_FILE"
wait
echo "All compiled."

# ── Phase 3: 并发发送任务 ─────────────────────────────────────────
echo ""
echo "Phase 3: Sending tasks to all containers (PARALLEL)..."
echo ""

while read -r sline; do
    id=$(echo "$sline" | awk '{print $1}')
    container_name=$(echo "$sline" | awk '{print $2}')

    # 找对应的 task spec
    for i in "${!TASKS[@]}"; do
        IFS='|' read -r tid target_file method_spec test_spec test_name commit_msg <<< "${TASKS[$i]}"
        if [ "$tid" = "$id" ]; then
            break
        fi
    done

    prompt="Task: Add a new method to $target_file.

METHOD SPEC:
$method_spec

TEST SPEC:
$test_spec

CRITICAL RULES:
1. Use edit tool, not bash sed
2. ALL comments in ENGLISH ONLY
3. Only ADD new code
4. Run: cargo check
5. Run: cargo test --lib $test_name
6. git add $target_file && git commit -m '$commit_msg'
7. grep -c \$'\xef\xbf\xbd' $target_file (must be 0)"

    echo "  [$id] Dispatching to $container_name..."

    # 后台发送！
    (
        result=$(echo "$prompt" | "$CONTAINER_BIN" exec -i "$container_name" sh -c \
            "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL" 2>&1 | tail -10)
        echo "$result" > "/tmp/par_result_${id}.txt"
    ) &

    echo "  [$id] dispatched (PID $!)"
done < "$STATE_FILE"

echo ""
echo "All $NUM_TASKS tasks running in parallel. Waiting..."
wait
echo "All tasks complete."
echo ""

# ── Phase 4: 逐个守门 + 同步 + commit ─────────────────────────────
echo "Phase 4: Gate check + sync + commit..."
echo ""

SUCCESS=0
FAIL=0

while read -r line; do
    id=$(echo "$line" | awk '{print $1}')
    container_name=$(echo "$line" | awk '{print $2}')
    wt_dir=$(echo "$line" | awk '{print $3}')
    target_file=$(echo "$line" | awk '{print $4}')
    commit_msg=$(echo "$line" | awk '{print $6}')

    echo "  [$id] Processing ($target_file)..."

    # B 输出
    if [ -f "/tmp/par_result_${id}.txt" ]; then
        echo "  [$id] B output (last 2 lines):"
        tail -2 "/tmp/par_result_${id}.txt" | sed 's/^/    /'
    fi

    # 守门
    ufffd=$("$CONTAINER_BIN" exec "$container_name" sh -c "grep -c \$'\\xef\\xbf\\xbd' /workspace/$target_file 2>/dev/null || true" 2>/dev/null | head -1 | tr -d '[:space:]')
    if [ "$ufffd" != "0" ]; then
        echo "  [$id] ❌ Gate FAILED ($ufffd U+FFFD)"
        FAIL=$((FAIL + 1))
        continue
    fi

    # 同步
    cp "$wt_dir/$target_file" "$PROJECT_DIR/$target_file"

    # 主仓库测试
    cd "$PROJECT_DIR"
    test_out=$(cargo test --lib 2>&1)
    if echo "$test_out" | grep -q "test result: ok"; then
        passed=$(echo "$test_out" | grep -oE "[0-9]+ passed" | head -1)
        echo "  [$id] ✅ cargo test: $passed"
        git add "$target_file"
        git commit -m "$commit_msg (parallel self-evolution)" 2>&1 | head -1
        SUCCESS=$((SUCCESS + 1))
    else
        echo "  [$id] ❌ cargo test FAILED"
        echo "$test_out" | grep "error\[" | head -2 | sed 's/^/    /'
        git checkout -- "$target_file"
        FAIL=$((FAIL + 1))
    fi
done < "$STATE_FILE"

# ── Phase 5: 清理 ─────────────────────────────────────────────────
echo ""
echo "Phase 5: Cleanup..."
while read -r line; do
    container_name=$(echo "$line" | awk '{print $2}')
    wt_dir=$(echo "$line" | awk '{print $3}')
    "$CONTAINER_BIN" stop "$container_name" 2>/dev/null
    rm -rf "$wt_dir"
done < "$STATE_FILE"
cd "$PROJECT_DIR"
git worktree prune
rm -f "$STATE_FILE"

# ── 总结 ──────────────────────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Parallel Complete"
echo "=========================================="
echo "  Success: $SUCCESS / $((SUCCESS + FAIL))"
[ $FAIL -gt 0 ] && echo "  Failed:  $FAIL"
echo "=========================================="