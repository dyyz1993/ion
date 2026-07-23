#!/usr/bin/env bash
# trigger_user.sh — 异步触发"使用者"角色（加载历史会话 + 体验新功能）
#
# 用法：bash scripts/trigger_user.sh
#
# 这个脚本：
# 1. 用 --continue 加载 user agent 的历史会话（保持上下文连贯）
# 2. 告诉它最近新增了什么功能（从 git log 提取）
# 3. 让它异步跑测试 + 体验功能 + 提 Issue
# 4. 通过 peer 模式异步运行（不阻塞 coordinator）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"

# 提取最近的 commit 摘要（作为"新功能"提示）
RECENT=$(git log --oneline -10 --no-merges 2>/dev/null | head -10)

echo "=========================================="
echo "  User Experience Tester"
echo "=========================================="
echo "  Model:  $MODEL"
echo "  Recent: "
echo "$RECENT" | sed 's/^/    /'
echo "=========================================="
echo ""

# 构造 prompt——告诉 user agent 最近新增了什么
PROMPT="你是使用者体验测试员。

## 最近新增的功能（git log 最近 10 条）
\`\`\`
$RECENT
\`\`\`

## 你的任务

1. 检查上面的 commit，找出你**还没测过**的功能
2. 实际运行命令测试每个新功能
3. 如果发现问题，用 gh issue create 创建 Issue
4. 汇报：哪些功能正常，哪些有问题

## 重要：保持上下文

你是在历史会话上继续的。你应该记得之前测过什么。
如果这是第一次运行，就把今天当作起点。

先执行步骤 1：读 git log，找出新功能。"

# Use --host so the user agent can spawn sub-tasks (e.g. create issues via sub-agents).
echo "$PROMPT" | timeout 600 ./target/debug/ion --host --agent user --model "$MODEL" --provider "$PROVIDER" --continue 2>&1 | tee /tmp/user_experience.log

echo ""
echo "=========================================="
echo "  User Experience Complete"
echo "=========================================="
echo "  Log: /tmp/user_experience.log"
echo "=========================================="