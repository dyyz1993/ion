#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# improver.sh — improver 通用任务智能体的确定性入口
#
# 为什么需要这个脚本？
#   直接 `ion --agent improver "话题"` 时，LLM 可能跳过 workflow 自己直接干
#   （把 improver 当普通 agent 用，不走 stage）。
#   这个脚本绕过 LLM 的"判断"，用 `ion workflow run --set` 确定性注入话题，
#   强制走 workflow.yaml 的 stage 流程（每个 stage 有 gate 校验，不会跳步）。
#
# 用法:
#   bash scripts/improver.sh "话题"                    # 自动判断 modify/research
#   bash scripts/improver.sh "话题" --type modify      # 强制改代码类（开 container）
#   bash scripts/improver.sh "话题" --type research    # 强制调研类（不开 container）
#
# 例子:
#   bash scripts/improver.sh "分析 src/global_memory.rs 的 search 函数潜在问题"
#   bash scripts/improver.sh "给 global_memory 加 fn count_all_archived() 方法" --type modify
#
# 产出:
#   - 跑完自动 open HTML 报告（workflow Stage 8 做）
#   - session 写到 ~/.ion/agent/sessions/，last_session 更新
#   - 改代码类还会在 worktree 分支上留 commit
#
# 前置条件:
#   - 改代码类（--type modify）需要 Apple Container 可用
#   - 调研类（--type research）不需要 container
# ──────────────────────────────────────────────────────────
set -uo pipefail

# ── 参数解析 ──
TOPIC=""
TOPIC_TYPE=""  # 空表示让 workflow 的 classify stage 自己判断

while [ $# -gt 0 ]; do
    case "$1" in
        --type)
            TOPIC_TYPE="$2"
            shift 2
            ;;
        --type=*)
            TOPIC_TYPE="${1#--type=}"
            shift
            ;;
        -h|--help)
            sed -n '2,30p' "$0"
            exit 0
            ;;
        *)
            if [ -z "$TOPIC" ]; then
                TOPIC="$1"
            else
                TOPIC="$TOPIC $1"
            fi
            shift
            ;;
    esac
done

if [ -z "$TOPIC" ]; then
    echo "❌ 用法: bash scripts/improver.sh \"话题\" [--type modify|research]"
    echo "  例子: bash scripts/improver.sh \"分析 xxx 的潜在问题\""
    echo "        bash scripts/improver.sh \"修 xxx 的 bug\" --type modify"
    exit 1
fi

# 校验 --type
if [ -n "$TOPIC_TYPE" ] && [ "$TOPIC_TYPE" != "modify" ] && [ "$TOPIC_TYPE" != "research" ]; then
    echo "❌ --type 只能是 modify 或 research，实际: $TOPIC_TYPE"
    exit 1
fi

# ── 找 workflow.yaml ──
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKFLOW_YAML="$PROJECT_DIR/.ion/workflow.yaml"

# 如果项目没有 workflow.yaml，从模板复制
if [ ! -f "$WORKFLOW_YAML" ]; then
    if [ -f "$PROJECT_DIR/examples/workflows/improver.wf.yaml" ]; then
        mkdir -p "$PROJECT_DIR/.ion"
        cp "$PROJECT_DIR/examples/workflows/improver.wf.yaml" "$WORKFLOW_YAML"
        echo "📋 从模板创建 .ion/workflow.yaml"
    else
        echo "❌ 找不到 workflow.yaml，也没找到模板 examples/workflows/improver.wf.yaml"
        exit 1
    fi
fi

# ── 找 ion binary ──
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
if [ ! -x "$ION_BIN" ]; then
    ION_BIN="ion"  # fallback 到 PATH 里的
fi

# ── 重置 workflow 状态（清掉残留的 status 字段）──
# 用模板覆盖（保留用户可能的定制就太复杂了，这里假设用最新模板）
if [ -f "$PROJECT_DIR/examples/workflows/improver.wf.yaml" ]; then
    cp "$PROJECT_DIR/examples/workflows/improver.wf.yaml" "$WORKFLOW_YAML"
fi

# ── 清掉旧 session（强制 wf agent 用全新 session，不复用历史）──
# 不清的话 wf agent 会复用 cwd 对应的旧 session，"记得上次跑过"导致跳步
rm -f "$PROJECT_DIR/.ion/.improver-state" /tmp/.improver-container-init 2>/dev/null

# ── 设置 timeout（改代码类需要长 timeout，因为 container 编译慢）──
export ION_TOOL_TIMEOUT="${ION_TOOL_TIMEOUT:-1800}"    # 30 分钟
export ION_BASH_TIMEOUT="${ION_BASH_TIMEOUT:-1800}"
export ION_HOST_TIMEOUT="${ION_HOST_TIMEOUT:-1800}"

# ── 跑 ──
echo "════════════════════════════════════════════════════"
echo "  🚀 improver 启动"
echo "════════════════════════════════════════════════════"
echo "  话题:    $TOPIC"
echo "  类型:    ${TOPIC_TYPE:-自动判断}"
echo "  workflow: $WORKFLOW_YAML"
echo "  ion:     $ION_BIN"
echo "  timeout: ${ION_TOOL_TIMEOUT}s"
echo "════════════════════════════════════════════════════"
echo ""

# 构建 --set 参数
SET_ARGS=(--set "topic=$TOPIC")
if [ -n "$TOPIC_TYPE" ]; then
    SET_ARGS+=(--set "topic_type=$TOPIC_TYPE")
fi

# 跑 workflow（wf agent 听话，会按 stage 走）
"$ION_BIN" workflow run "${SET_ARGS[@]}" "$WORKFLOW_YAML"

EXIT_CODE=$?

echo ""
if [ $EXIT_CODE -eq 0 ]; then
    echo "════════════════════════════════════════════════════"
    echo "  ✅ improver 完成"
    echo "════════════════════════════════════════════════════"
    # 报告路径在 .ion/.improver-state（workflow Stage 8 写的，cleanup 前读）
    if [ -f "$PROJECT_DIR/.ion/.improver-state" ]; then
        source "$PROJECT_DIR/.ion/.improver-state"
        if [ -n "$REPORT_PATH" ]; then
            echo "  报告: $REPORT_PATH"
        fi
    fi
    # 找最新的 /tmp/improver_*.html
    LATEST_HTML=$(ls -t /tmp/improver_*.html 2>/dev/null | head -1)
    if [ -n "$LATEST_HTML" ]; then
        echo "  最新 HTML: $LATEST_HTML"
        # macOS 自动打开
        if command -v open >/dev/null 2>&1; then
            open "$LATEST_HTML" 2>/dev/null && echo "  ✅ 已在浏览器打开"
        fi
    fi
else
    echo "════════════════════════════════════════════════════"
    echo "  ❌ improver 失败 (exit $EXIT_CODE)"
    echo "════════════════════════════════════════════════════"
fi

exit $EXIT_CODE
