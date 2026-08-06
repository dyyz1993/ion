#!/usr/bin/env bash
# evolve_monitor_v2.sh — A→B 并行实现 Monitor Extension v2
#
# 3 个并行子任务（基于 evolve_concurrent.sh 改造）：
#   T1: MonitorDef 加 mode/trigger_mode/max_concurrent/cooldown 字段 + 三种 mode 触发逻辑
#   T2: validate + test RPC（src/monitor_extension.rs 不同函数）
#   T3: CI 脚本 Group E-J（tests/monitor_ci.sh 新增 case）
#
# 关键改造点（相比 evolve_concurrent.sh）：
#   1. 每个 T 有独立的 spec 文件（/tmp/monitor_v2_specs/T*.md），任务复杂时 stdin 太长
#   2. T1/T2 都改 src/monitor_extension.rs，用 worktree 隔离 + 最后 merger 合并
#   3. T3 改 tests/monitor_ci.sh，独立无冲突
#
# 用法：bash scripts/evolve_monitor_v2.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
SPEC_DIR="/tmp/monitor_v2_specs"
CONTAINER_NAME=""
WT_DIR=""

echo ""
echo "=========================================="
echo "  Monitor v2 — A→B Parallel Implementation"
echo "=========================================="
echo "  Model:    $MODEL ($PROVIDER)"
echo "  Spec dir: $SPEC_DIR"
echo "  Tasks:    T1 (kernel fields) / T2 (RPC) / T3 (CI)"
echo "=========================================="
echo ""

# ── Phase 0: 检查 spec 文件 ────────────────────────────────────────
echo "Phase 0: Verifying spec files..."

if [ ! -d "$SPEC_DIR" ]; then
    echo "❌ Spec dir $SPEC_DIR not found. ZCode should write specs first."
    exit 1
fi

for t in T1_kernel.md T2_rpc.md T3_ci.md; do
    if [ ! -f "$SPEC_DIR/$t" ]; then
        echo "❌ Missing spec: $SPEC_DIR/$t"
        exit 1
    fi
done
echo "  ✅ All 3 specs present"

# ── Phase 1: 启动 container + 编译 ────────────────────────────────
echo ""
echo "Phase 1: Starting container + compiling ion (reusing evolve.sh)..."

# 复用 evolve.sh 的 container 启动逻辑（含 volume cache）
ION_TOOL_TIMEOUT=1800 bash "$PROJECT_DIR/scripts/evolve.sh" > /tmp/evolve_monitor_v2_phase1.log 2>&1
source /tmp/.evolver-state

if [ "$BUILD_STATUS" != "OK" ]; then
    echo "❌ Container build failed. See /tmp/evolve_monitor_v2_phase1.log"
    tail -20 /tmp/evolve_monitor_v2_phase1.log
    exit 1
fi
echo "  ✅ Container ready: $CONTAINER_NAME"
echo "  ✅ ion binary compiled"

# 把设计文档 + spec 复制到 container（让 B 能 read）
echo ""
echo "  Copying design docs + specs into container..."
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "mkdir -p /workspace/docs/design /workspace/docs/testing /workspace/examples/agents /workspace/specs" 2>/dev/null
"$CONTAINER_BIN" cp "$PROJECT_DIR/docs/design/MONITOR_EXTENSION.md" "$CONTAINER_NAME:/workspace/docs/design/" 2>/dev/null
"$CONTAINER_BIN" cp "$PROJECT_DIR/docs/testing/MONITOR_CLI_TEST.md" "$CONTAINER_NAME:/workspace/docs/testing/" 2>/dev/null
"$CONTAINER_BIN" cp "$PROJECT_DIR/examples/agents/scheduler.md" "$CONTAINER_NAME:/workspace/examples/agents/" 2>/dev/null
"$CONTAINER_BIN" cp "$SPEC_DIR/T1_kernel.md" "$CONTAINER_NAME:/workspace/specs/" 2>/dev/null
"$CONTAINER_BIN" cp "$SPEC_DIR/T2_rpc.md" "$CONTAINER_NAME:/workspace/specs/" 2>/dev/null
"$CONTAINER_BIN" cp "$SPEC_DIR/T3_ci.md" "$CONTAINER_NAME:/workspace/specs/" 2>/dev/null

# ── Phase 2: 创建 3 个 worktree 子目录 ────────────────────────────
echo ""
echo "Phase 2: Creating 3 worktree subdirectories..."

declare -a TASK_IDS=("T1" "T2" "T3")
declare -a TASK_SPECS=("T1_kernel.md" "T2_rpc.md" "T3_ci.md")
declare -a TASK_TARGETS=("src/monitor_extension.rs" "src/monitor_extension.rs" "tests/monitor_ci.sh")

for i in 0 1 2; do
    id="${TASK_IDS[$i]}"
    spec="${TASK_SPECS[$i]}"
    target="${TASK_TARGETS[$i]}"
    n=$((i + 1))

    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c "
        rm -rf /workspace/wt-$n
        mkdir -p /workspace/wt-$n
        cd /workspace && for f in src Cargo.toml Cargo.lock tests docs examples specs; do
            [ -e \"\$f\" ] && cp -r \"\$f\" /workspace/wt-$n/ 2>/dev/null
        done
        cd /workspace/wt-$n && git init -q && git config user.email 'ion@evolver' && git config user.name 'Evolver'
        git add -A && git commit -q -m 'init' 2>/dev/null
    " 2>/dev/null
    echo "  [$id] wt-$n ready (target=$target)"
done

# ── Phase 3: 并行发任务 ────────────────────────────────────────────
echo ""
echo "Phase 3: Dispatching 3 tasks in PARALLEL..."
echo "  (B reads spec → edits code → commits; A verifies on host later)"
echo ""

for i in 0 1 2; do
    id="${TASK_IDS[$i]}"
    spec="${TASK_SPECS[$i]}"
    target="${TASK_TARGETS[$i]}"
    n=$((i + 1))

    (
        echo "=== DEVELOPER ($id) ===" > "/tmp/monitor_v2_${id}.txt"

        # B 的 prompt：先 read spec + 设计文档，再改代码
        prompt="You are implementing Monitor Extension v2 task $id.

READ THESE FIRST (in order):
1. /workspace/specs/$spec           ← YOUR SPEC (primary)
2. /workspace/docs/design/MONITOR_EXTENSION.md  ← full design
3. /workspace/src/monitor_extension.rs  ← current code (T1/T2)
   OR /workspace/tests/monitor_ci.sh    ← current CI (T3)

YOUR TASK:
Follow the spec exactly. Edit $target. Add tests if spec requires.

CRITICAL RULES:
1. ALL comments in ENGLISH ONLY (no Chinese, avoid U+FFFD)
2. Use edit/write tool, NOT bash sed
3. ONLY ADD new code where spec says; do NOT refactor existing lines
4. After editing: git add $target && git commit -m '$id: monitor v2'
5. Verify: grep -c \$'\xef\xbf\xbd' $target must be 0
6. Do NOT run cargo build or cargo test — A will verify on host

REPORT: when done, output 'DONE: $id' on the last line."

        dev_result=$(echo "$prompt" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
            "cd /workspace/wt-$n && /workspace/target/release/ion --agent developer --provider $PROVIDER --model $MODEL --max-turns 30" 2>&1 | tail -30)

        echo "$dev_result" >> "/tmp/monitor_v2_${id}.txt"

        if echo "$dev_result" | grep -q "DONE: $id"; then
            echo "DEV_SUCCESS" >> "/tmp/monitor_v2_${id}.txt"
        else
            echo "DEV_INCOMPLETE" >> "/tmp/monitor_v2_${id}.txt"
        fi

        echo "  [$id] developer done"
    ) &

    echo "  [$id] dispatched to wt-$n (PID $!)"
done

echo ""
echo "All 3 B workers running in parallel. Waiting..."
wait
echo "All B workers complete."
echo ""

# ── Phase 4: A 同步 + 验证 + merge ────────────────────────────────
echo "Phase 4: A syncing + verifying + merging..."

declare -a MERGE_OK=("no" "no" "no")

for i in 0 1 2; do
    id="${TASK_IDS[$i]}"
    target="${TASK_TARGETS[$i]}"
    n=$((i + 1))

    echo ""
    echo "  [$id] Processing wt-$n ($target)..."

    # B 输出
    if [ -f "/tmp/monitor_v2_${id}.txt" ]; then
        echo "  [$id] B output (last 20 lines):"
        tail -20 "/tmp/monitor_v2_${id}.txt" | sed 's/^/    /'
    fi

    if ! grep -q "DEV_SUCCESS" "/tmp/monitor_v2_${id}.txt" 2>/dev/null; then
        echo "  [$id] ⚠️ Developer did not complete. Skipping merge."
        continue
    fi

    # U+FFFD 守门（关键！）
    garbled=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "grep -rl \$'\xef\xbf\xbd' /workspace/wt-$n/$target 2>/dev/null" 2>/dev/null)
    if [ -n "$garbled" ]; then
        echo "  [$id] ❌ U+FFFD detected in $target. Rejecting."
        continue
    fi
    echo "  [$id] ✅ U+FFFD check passed"

    # 从 container 拷改动到 host 临时目录
    "$CONTAINER_BIN" cp "$CONTAINER_NAME:/workspace/wt-$n/$target" "/tmp/monitor_v2_${id}_${target##*/}" 2>/dev/null

    MERGE_OK[$i]="yes"
    echo "  [$id] ✅ Ready to merge"
done

# ── Phase 5: 合并到主仓库 ──────────────────────────────────────────
echo ""
echo "Phase 5: Merging to main repo..."

# T3 (tests/monitor_ci.sh) 独立无冲突，直接 cp
if [ "${MERGE_OK[2]}" = "yes" ]; then
    cp "/tmp/monitor_v2_T3_monitor_ci.sh" "$PROJECT_DIR/tests/monitor_ci.sh"
    echo "  ✅ T3 merged: tests/monitor_ci.sh"
fi

# T1 + T2 都改 src/monitor_extension.rs — 需要三方合并
# 策略：T1 改前半（字段+loop），T2 改后半（RPC handler）
# 用 git merge-file 做 3-way merge
if [ "${MERGE_OK[0]}" = "yes" ] && [ "${MERGE_OK[1]}" = "yes" ]; then
    echo "  T1 + T2 both touch src/monitor_extension.rs — attempting 3-way merge..."

    BASE="$PROJECT_DIR/src/monitor_extension.rs"
    cp "$BASE" /tmp/monitor_v2_base.rs

    # 3-way merge: base | T1 | T2
    if git merge-file -L "T2 (RPC)" -L "BASE" -L "T1 (kernel)" \
        /tmp/monitor_v2_T2_monitor_extension.rs \
        /tmp/monitor_v2_base.rs \
        /tmp/monitor_v2_T1_monitor_extension.rs 2>/dev/null; then
        # T2 文件被原地更新为合并结果
        cp /tmp/monitor_v2_T2_monitor_extension.rs "$BASE"
        echo "  ✅ 3-way merge succeeded (no conflicts)"
    else
        echo "  ⚠️ 3-way merge had conflicts. Manual resolution needed."
        echo "  Falling back: apply T1 first, then T2 patch"
        # Fallback: 先 cp T1，再尝试 patch T2 的改动
        cp /tmp/monitor_v2_T1_monitor_extension.rs "$BASE"
        # 这里不自动 resolve，留给后续 reviewer
        echo "  ⚠️ T1 applied; T2 changes saved to /tmp/monitor_v2_T2_monitor_extension.rs (manual apply needed)"
    fi
elif [ "${MERGE_OK[0]}" = "yes" ]; then
    cp /tmp/monitor_v2_T1_monitor_extension.rs "$PROJECT_DIR/src/monitor_extension.rs"
    echo "  ✅ T1 merged (T2 skipped)"
elif [ "${MERGE_OK[1]}" = "yes" ]; then
    cp /tmp/monitor_v2_T2_monitor_extension.rs "$PROJECT_DIR/src/monitor_extension.rs"
    echo "  ✅ T2 merged (T1 skipped)"
fi

# ── Phase 6: 全量验证 ─────────────────────────────────────────────
echo ""
echo "Phase 6: Full verification on host..."

cd "$PROJECT_DIR"

echo "  → cargo check..."
if cargo check 2>&1 | tail -5; then
    echo "  ✅ cargo check passed"
else
    echo "  ❌ cargo check failed"
fi

echo ""
echo "  → U+FFFD scan on src/monitor_extension.rs..."
GARBLED=$(grep -c $'\xef\xbf\xbd' src/monitor_extension.rs 2>/dev/null || echo 0)
GARBLED=$(echo "$GARBLED" | head -1)
if [ "$GARBLED" = "0" ]; then
    echo "  ✅ 0 U+FFFD chars"
else
    echo "  ❌ $GARBLED U+FFFD chars found"
fi

echo ""
echo "  → cargo test --lib monitor..."
cargo test --lib monitor 2>&1 | tail -10

echo ""
echo "  → cargo build --bin ion..."
if cargo build --bin ion 2>&1 | tail -3; then
    echo "  ✅ ion builds"
else
    echo "  ❌ ion build failed"
fi

# ── Phase 7: 报告 ──────────────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Monitor v2 A→B Complete"
echo "=========================================="
echo "  T1 (kernel fields): ${MERGE_OK[0]}"
echo "  T2 (validate/test RPC): ${MERGE_OK[1]}"
echo "  T3 (CI Group E-J): ${MERGE_OK[2]}"
echo ""
echo "  Next: ZCode reviews + commits + runs CI"
echo "=========================================="

# 不自动 commit/push — 让 ZCode 看结果决定
