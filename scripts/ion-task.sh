#!/usr/bin/env bash
# ion-task.sh — A→B 并发任务启动器
#
# 解决问题：同时跑多个 `ion --host` 任务时，各自独立 socket 互不干扰，
# 并且写状态文件让 ZCode hooks 能感知到所有活跃任务。
#
# 用法：
#   bash scripts/ion-task.sh "任务描述" [其他 ion 参数...]
#   TASK_NAME="改A模块" bash scripts/ion-task.sh "详细任务..."
#
# 它做了什么：
#   1. 生成唯一 task_id + socket 路径
#   2. 写状态文件 ~/.ion/active-tasks/<task_id>.json
#   3. 设 ION_HOST_SOCKET 启动 ion --host
#   4. 任务结束（正常/中断）时删状态文件
#
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
ACTIVE_DIR="$HOME/.ion/active-tasks"
mkdir -p "$ACTIVE_DIR"

# ── 参数解析 ──────────────────────────────────────
TASK_DESC="${1:-untitled task}"
shift || true
TASK_NAME="${TASK_NAME:-$(echo "$TASK_DESC" | head -c 40)}"

# 其他参数透传给 ion（如 --provider, --model, --agent 等）
ION_ARGS=("$@")

# ── 生成唯一 task_id + socket ────────────────────
TASK_ID="task_$(date +%Y%m%d_%H%M%S)_$$_$RANDOM"
SOCK_DIR="/tmp/ion-tasks"
mkdir -p "$SOCK_DIR"
SOCK_PATH="$SOCK_DIR/${TASK_ID}.sock"
WT_DIR=""  # worktree 路径（如果 ion 创建了，这里记录；暂留空）

# ── 写状态文件 ────────────────────────────────────
STATE_FILE="$ACTIVE_DIR/${TASK_ID}.json"
cat > "$STATE_FILE" << EOF
{
  "task_id": "$TASK_ID",
  "task_name": $(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$TASK_NAME"),
  "task_desc": $(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$TASK_DESC"),
  "socket": "$SOCK_PATH",
  "worktree": "$WT_DIR",
  "status": "starting",
  "pid": $$,
  "started_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "project": "$PROJECT_DIR"
}
EOF

echo "🚀 ion-task started: $TASK_NAME"
echo "   task_id: $TASK_ID"
echo "   socket:  $SOCK_PATH"
echo "   state:   $STATE_FILE"
echo ""

# ── 清理函数（正常退出 + 信号中断都触发）──────────────
cleanup() {
    local exit_code=$?
    # 更新状态为 done/failed
    if [ $exit_code -eq 0 ]; then
        python3 -c "
import json
f = '$STATE_FILE'
d = json.load(open(f))
d['status'] = 'done'
d['ended_at'] = __import__('datetime').datetime.utcnow().isoformat() + 'Z'
json.dump(d, open(f,'w'), indent=2)
" 2>/dev/null
        echo ""
        echo "✅ task $TASK_ID done"
    else
        python3 -c "
import json
d = json.load(open('$STATE_FILE'))
d['status'] = 'failed'
d['exit_code'] = $exit_code
d['ended_at'] = __import__('datetime').datetime.utcnow().isoformat() + 'Z'
json.dump(d, open('$STATE_FILE','w'), indent=2)
" 2>/dev/null
        echo ""
        echo "❌ task $TASK_ID failed (exit $exit_code)"
    fi
    # 延迟删除状态文件（让 hook 有时间读到最终状态）
    # 60 秒后自动删（background sleep + rm）
    (sleep 60 && rm -f "$STATE_FILE" "$SOCK_PATH" "${SOCK_PATH%.sock}.pid" 2>/dev/null) &
}
trap cleanup EXIT INT TERM

# ── 更新状态为 running ────────────────────────────
python3 -c "
import json
d = json.load(open('$STATE_FILE'))
d['status'] = 'running'
json.dump(d, open('$STATE_FILE','w'), indent=2)
" 2>/dev/null

# ── 启动 ion --host（透传所有参数）──────────────────
export ION_HOST_SOCKET="$SOCK_PATH"
exec "$ION_BIN" --host "${ION_ARGS[@]}" "$TASK_DESC"
