#!/usr/bin/env bash
# ion-tasks-status-hook.sh — ZCode UserPromptSubmit hook
#
# 读 ~/.ion/active-tasks/*.json，把所有活跃 A→B 任务的状态汇总成
# additionalContext，注入到当前会话的 system prompt 末尾。
#
# 这样所有 ZCode 会话都能感知到："现在有几个 ion 任务在跑、
# 各自的 socket/worktree/进度"。
#
# ZCode hook 协议：
#   - stdin 收到事件 JSON（含 prompt 等字段）
#   - stdout 输出 JSON（additionalContext 字段会被注入）
#   - 或 exit 0（无注入）/ exit 2（block）
#
set -euo pipefail

ACTIVE_DIR="$HOME/.ion/active-tasks"

# 如果没有活跃任务，直接退出（不注入任何东西，省 token）
if [ ! -d "$ACTIVE_DIR" ] || [ -z "$(ls -A "$ACTIVE_DIR"/*.json 2>/dev/null)" ]; then
    exit 0
fi

# 汇总所有活跃任务
SUMMARY=$(python3 << 'PYEOF' 2>/dev/null || true
import json, os, glob, time

active_dir = os.path.expanduser("~/.ion/active-tasks")
tasks = []
now = time.time()

for f in sorted(glob.glob(os.path.join(active_dir, "*.json"))):
    try:
        with open(f) as fh:
            t = json.load(fh)
    except:
        continue

    # 过滤掉 2 小时前的僵尸状态文件（可能 task 崩溃没清理）
    started = t.get("started_at", "")
    # 计算运行时长（粗略）
    age_note = ""
    if started:
        try:
            from datetime import datetime, timezone
            dt = datetime.fromisoformat(started.replace("Z", "+00:00"))
            age_secs = int(now - dt.timestamp())
            if age_secs < 60:
                age_note = f"{age_secs}s"
            elif age_secs < 3600:
                age_note = f"{age_secs // 60}m"
            else:
                age_note = f"{age_secs // 3600}h"
        except:
            age_note = "?"

    # 过滤掉 done/failed 超过 5 分钟的（保留一会儿让用户看到结果）
    status = t.get("status", "unknown")
    if status in ("done", "failed"):
        ended = t.get("ended_at", "")
        if ended:
            try:
                from datetime import datetime
                dt = datetime.fromisoformat(ended.replace("Z", "+00:00"))
                if now - dt.timestamp() > 300:  # 5 分钟
                    continue
            except:
                pass

    tasks.append({
        "name": t.get("task_name", "?")[:50],
        "status": status,
        "age": age_note,
        "socket": t.get("socket", ""),
        "task_id": t.get("task_id", ""),
    })

if not tasks:
    exit(0)

# 构建注入文本
lines = ["", "--- ion-tasks (A→B background tasks) ---"]
lines.append(f"{len(tasks)} active task(s):")
for t in tasks:
    icon = {"running": "🔄", "starting": "🚀", "done": "✅", "failed": "❌"}.get(t["status"], "❓")
    lines.append(f'  {icon} {t["name"]} [{t["status"]}, {t["age"]}]')
    lines.append(f'     socket: {t["socket"]}')
text = "\n".join(lines)

# ZCode hook 输出格式
output = {"additionalContext": text}
print(json.dumps(output))
PYEOF
)

# 如果汇总成功，输出给 ZCode
if [ -n "$SUMMARY" ]; then
    echo "$SUMMARY"
fi
# exit 0 让会话继续
exit 0
