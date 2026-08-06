#!/usr/bin/env bash
# user_simulation.sh — 模拟 12 个不同场景的用户体验测试
#
# 每个用户：
# 1. fork 当前 session（--fork-from-leaf）
# 2. 用 fast 模型（deepseek-v4-flash）
# 3. 从用户角度读 README + 跑命令
# 4. 发现问题提 GitHub Issue
#
# Usage: bash scripts/user_simulation.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

MODEL="deepseek-v4-flash"
PROVIDER="opencode"
REPO="https://github.com/dyyz1993/ion"

# ── 12 个用户场景 ──────────────────────────────────────────────
SCENARIOS=(
    "You are a new Rust developer. You just found ION on GitHub ($REPO). Read the README, try to build it, and use it. Report any confusion or errors as GitHub Issues."
    "You are a Python developer who has never used Rust. You want to try ION. Follow the Quick Start guide. Report any steps that are unclear or fail."
    "You are a DevOps engineer. You want to run ION as a persistent service (Scenario 3: ion serve). Try it and report issues."
    "You are an AI researcher. You want to use ION with multiple LLM providers (OpenAI, Anthropic, Google). Try configuring different providers. Report issues."
    "You are a solo developer who wants to use ION for code review. Try: ion --agent reviewer 'review src/auth.rs'. Report issues."
    "You are a team lead who wants multi-agent orchestration. Try: ion --host --agent coordinator 'add a method'. Report issues."
    "You are a CLI power user. Explore all ion commands: ion --help, ion sessions, ion history, ion list-models, ion list-agents. Report confusing UX."
    "You are testing the Memory system. Run: ion rpc --method extension_rpc --params '{\"extension\":\"global-memory\",\"method\":\"save\",...}'. Report issues."
    "You are testing HTML export. Run: ion --export /tmp/test.html. Open the HTML and check if tools + system prompt show correctly. Report issues."
    "You are a security researcher. Check: Does ion have permission controls? Try ion config, check ~/.ion/auth.json permissions. Report concerns."
    "You are a documentation reviewer. Read README.md, CLI_USAGE.md, CONTRIBUTING.md. Find broken links, missing info, or confusing descriptions. Report as Issues."
    "You are a performance tester. Run ion multiple times, check startup time, memory usage (ps aux | grep ion). Report if anything is slow."
)

NUM=${#SCENARIOS[@]}

echo ""
echo "=========================================="
echo "  User Simulation: $NUM scenarios"
echo "  Model: $MODEL ($PROVIDER)"
echo "  Repo: $REPO"
echo "=========================================="
echo ""

# 找当前 session（用于 fork）
CURRENT_SID=$(cat ~/.ion/agent/last_session 2>/dev/null || echo "")
echo "Current session: $CURRENT_SID"
echo ""

SUCCESS=0
FAIL=0

for i in "${!SCENARIOS[@]}"; do
    NUM=$((i + 1))
    SCENARIO="${SCENARIOS[$i]}"

    echo "──────────────────────────────────────────"
    echo "User $NUM / $NUM"
    echo "──────────────────────────────────────────"
    echo "Scenario: $(echo "$SCENARIO" | head -c 80)..."
    echo ""

    # 用 -p 模式（不走 host，避免 spawn_worker 复杂性）
    # 每个用户独立 session（不用 --continue）
    RESULT=$(echo "$SCENARIO

IMPORTANT:
1. Actually RUN commands using bash tool (don't just read code)
2. If you find problems, create GitHub Issues: gh issue create --title '...' --body '...'
3. Report what you tested and what you found
4. You are a REAL USER, not a developer. Test from user perspective." \
        | timeout 120 ./target/debug/ion -p --agent user --model "$MODEL" --provider "$PROVIDER" 2>&1 | tail -10)

    echo "$RESULT"
    echo ""

    # 检查是否提了 Issue
    ISSUES_BEFORE=$(gh issue list --state open --limit 100 2>/dev/null | wc -l)

    if echo "$RESULT" | grep -qi "issue\|gh issue\|bug\|problem\|error\|confus"; then
        echo "  ✅ User $NUM reported findings"
        SUCCESS=$((SUCCESS + 1))
    else
        echo "  ⚠️ User $NUM completed (no issues mentioned)"
        SUCCESS=$((SUCCESS + 1))
    fi
    echo ""
done

# 最终统计
ISSUES_AFTER=$(gh issue list --state all --limit 100 2>/dev/null | wc -l)

echo ""
echo "=========================================="
echo "  User Simulation Complete"
echo "=========================================="
echo "  Users completed: $SUCCESS / $NUM"
echo "  Total GitHub Issues: $ISSUES_AFTER"
echo "==========================================
"
echo "=== All Issues ==="
gh issue list --state all --limit 20 2>&1 | head -20