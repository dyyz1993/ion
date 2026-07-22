#!/usr/bin/env bash
# evolve_native.sh — 用 ION 原生多智能体（spawn_worker）做自进化
#
# 不再用 bash & 并发，而是让 coordinator agent 自己用 spawn_worker 编排：
#   coordinator → spawn_worker(developer) × 3 并行 → await_worker → reviewer → 汇报
#
# 这才是真正的"ION 用自己的能力进化自己"
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MODEL="${MODEL:-deepseek-v4-flash}"
PROVIDER="${PROVIDER:-opencode}"

source /tmp/.evolver-state 2>/dev/null
if [ -z "$CONTAINER_NAME" ] || [ -z "$WT_DIR" ]; then
    echo "ERROR: Run scripts/evolve.sh first"
    exit 1
fi

CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"

# ── 注册 zai provider（如果用 GLM）────────────────────────────────
if [ "$PROVIDER" = "zai" ]; then
    python3 -c "
import json
cfg = json.load(open('$HOME/.ion/config.json'))
cfg.setdefault('providers', {}).setdefault('zai', {
    'name': 'zai', 'api': 'openai-completions',
    'base_url': 'https://p.19930810.xyz:8443/k/group/https://api.z.ai/api/coding/paas/v4',
    'api_key': 'any-token-here',
    'models': [{'id': 'glm-5.2', 'name': 'GLM-5.2', 'reasoning': True, 'context_window': 128000}]
})
print(json.dumps(cfg, indent=2, ensure_ascii=False))
" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c 'cat > /root/.ion/config.json' 2>&1
fi

# ── 复制 agent 定义到 container ───────────────────────────────────
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'mkdir -p /workspace/.ion/agents' 2>/dev/null
for agent in coordinator developer reviewer evolver_agent; do
    "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c "cat > /workspace/.ion/agents/${agent}.md" \
        < "$PROJECT_DIR/examples/agents/${agent}.md" 2>/dev/null
done

# ── 任务清单 ──────────────────────────────────────────────────────
TASK_FILE="${1:-src/global_memory.rs}"
TASK_DESC="${2:-Add a utility method to $TASK_FILE with test. SQL must be parameterized. ALL comments ENGLISH. Use edit tool. cargo check. git add + commit.}"

echo ""
echo "=========================================="
echo "  Native Multi-Agent Self-Evolution"
echo "  (coordinator + spawn_worker)"
echo "=========================================="
echo "  Model:    $MODEL"
echo "  Provider: $PROVIDER"
echo "  Task:     $TASK_DESC"
echo "=========================================="
echo ""

# ── 核心：coordinator prompt ──────────────────────────────────────
COORDINATOR_PROMPT="你是 coordinator。你有以下任务需要完成。

## 任务
$TASK_DESC

## 执行流程

### Phase 1: 同步 spawn developer（单个任务）
用 spawn_worker(relation='child', agent='developer', wait=true) 创建 developer。
在 task 参数里传入详细的任务描述。
等 developer 完成后你会拿到结果。

### Phase 2: 同步 spawn reviewer（审查）
用 spawn_worker(relation='child', agent='reviewer', wait=true) 创建 reviewer。
让 reviewer 审查 developer 的改动：
  task='Review the latest git commit. Run: git diff HEAD~1 HEAD. Check: SQL injection, error handling, edge cases, test coverage. Report APPROVE or REQUEST_CHANGES.'

### Phase 3: 如果 reviewer REJECT，用 resume_worker 让 developer 修复
resume_worker(worker_id=<developer_id>, text='Reviewer issues: <paste>. Please fix.')

### Phase 4: 汇报
汇报最终结果：文件、commit、reviewer 结论。

先执行 Phase 1。"

# ── 执行 ──────────────────────────────────────────────────────────
echo "Launching coordinator..."
echo "$COORDINATOR_PROMPT" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
    "cd /workspace && ./target/release/ion --host --agent coordinator --provider $PROVIDER --model $MODEL" 2>&1 | tee /tmp/evolve_native.log

echo ""
echo "=========================================="
echo "  Native Evolution Complete"
echo "=========================================="

# ── 导出报告 ──────────────────────────────────────────────────────
echo "Exporting HTML reports..."
REPORT_DIR="/tmp/evolve_native_reports"
mkdir -p "$REPORT_DIR"

# 找所有最近创建的 session
"$PROJECT_DIR/target/debug/ion" --export "$REPORT_DIR/report_coordinator.html" --session "$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'cat /root/.ion/agent/last_session 2>/dev/null' 2>/dev/null | head -1)" 2>/dev/null

echo "Report: $REPORT_DIR/report_coordinator.html"
echo "=========================================="