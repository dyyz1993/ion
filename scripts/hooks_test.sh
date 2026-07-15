#!/bin/bash
# hooks_test.sh — 不依赖 Rust 的 hooks 验证脚本
#
# 用法：
#   bash hooks_test.sh validate [hooks.json路径]          # 校验配置格式
#   bash hooks_test.sh test <event> [--stdin '<json>']     # 模拟触发，真跑 handler
#   bash hooks_test.sh list                                # 列出生效的 hooks
#
# 这个脚本模拟 ion hooks test/validate/list 的核心逻辑，
# 让用户不写 Rust 就能验证自己的 .ion/hooks.json 配置。
# 纯 bash + jq，无外部依赖（除了 jq）。
#
# 用法示例：
#   bash scripts/hooks_test.sh validate .ion/hooks.json
#   bash scripts/hooks_test.sh test PreToolUse --stdin '{"tool_name":"bash","tool_input":{"command":"git commit --no-verify"}}'
#   bash scripts/hooks_test.sh list

set -euo pipefail

# ── 颜色输出 ──
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# ── 找 hooks.json ──
find_hooks_json() {
    local proj_dir="${1:-.}"
    local global="$HOME/.ion/hooks.json"
    local proj="$proj_dir/.ion/hooks.json"

    local found=""
    [ -f "$global" ] && found="$global"
    [ -f "$proj" ] && found="$proj"
    echo "$found"
}

# ── 检查 jq ──
check_jq() {
    if ! command -v jq &>/dev/null; then
        echo -e "${RED}错误：需要 jq 但未安装。请先安装 jq${NC}"
        echo "  macOS: brew install jq"
        echo "  Ubuntu: apt install jq"
        exit 1
    fi
}

# ── validate 子命令 ──
cmd_validate() {
    local path="${1:-.ion/hooks.json}"
    if [ ! -f "$path" ]; then
        echo -e "${RED}错误：文件不存在 $path${NC}"
        exit 1
    fi

    echo -e "${CYAN}校验 $path ...${NC}"
    echo ""

    # 检查是否是合法 JSON
    if ! content=$(cat "$path" | jq . 2>&1); then
        echo -e "${RED}❌ JSON 格式错误：${NC}"
        echo "$content" | head -5
        exit 1
    fi

    # 检查 version 字段
    local version=$(jq -r '.version // 1' "$path")
    if [ "$version" != "1" ]; then
        echo -e "${YELLOW}⚠️  version=$version，当前仅支持 version 1${NC}"
    fi

    # 检查 disableAllHooks
    local disabled=$(jq -r '.disableAllHooks // false' "$path")
    if [ "$disabled" = "true" ]; then
        echo -e "${YELLOW}⚠️  disableAllHooks=true，所有 hooks 被禁用${NC}"
    fi

    # 统计事件和 handler
    local event_count=$(jq '.hooks | keys | length' "$path")
    local handler_count=$(jq '[.hooks[] | .[] | if type == "object" and has("hooks") then .hooks[] else . end] | length' "$path")

    echo -e "${GREEN}✅ Hooks 配置有效${NC}"
    echo "   版本: $version"
    echo "   事件数: $event_count"
    echo "   handler 总数: $handler_count"
    echo ""

    # 列出每个事件的 handler
    echo "事件详情："
    jq -r '.hooks | to_entries[] | "  \(.key): \(.value | length) 个配置"' "$path"
}

# ── list 子命令 ──
cmd_list() {
    local proj_hooks=".ion/hooks.json"
    local global_hooks="$HOME/.ion/hooks.json"

    echo -e "${CYAN}Hooks 配置列表${NC}"
    echo ""

    local any_found=false

    if [ -f "$global_hooks" ]; then
        any_found=true
        echo -e "${CYAN}[全局] $global_hooks${NC}"
        local disabled=$(jq -r '.disableAllHooks // false' "$global_hooks")
        [ "$disabled" = "true" ] && echo -e "  ${YELLOW}⚠️  disabled${NC}"
        jq -r '.hooks | to_entries[] | "  \(.key): \(.value | length) 配置"' "$global_hooks" 2>/dev/null || echo "  (解析失败)"
        echo ""
    fi

    if [ -f "$proj_hooks" ]; then
        any_found=true
        echo -e "${CYAN}[项目] $proj_hooks${NC}"
        local disabled=$(jq -r '.disableAllHooks // false' "$proj_hooks")
        [ "$disabled" = "true" ] && echo -e "  ${YELLOW}⚠️  disabled${NC}"
        jq -r '.hooks | to_entries[] | "  \(.key): \(.value | length) 配置"' "$proj_hooks" 2>/dev/null || echo "  (解析失败)"
        echo ""
    fi

    if [ "$any_found" = false ]; then
        echo -e "${YELLOW}没有找到 hooks.json${NC}"
        echo "  全局: $global_hooks（不存在）"
        echo "  项目: $proj_hooks（不存在）"
    fi
}

# ── test 子命令（模拟触发，真跑 handler）──
cmd_test() {
    local event="$1"
    shift

    # 解析 --stdin 参数
    local stdin_json="{}"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --stdin)
                stdin_json="$2"
                shift 2
                ;;
            *)
                echo -e "${RED}未知参数: $1${NC}"
                exit 1
                ;;
        esac
    done

    local hooks_file=".ion/hooks.json"
    if [ ! -f "$hooks_file" ]; then
        hooks_file="$HOME/.ion/hooks.json"
    fi
    if [ ! -f "$hooks_file" ]; then
        echo -e "${RED}错误：找不到 hooks.json${NC}"
        exit 1
    fi

    # 提取该事件的 handler（展平 group）
    local handlers_json=$(jq -c --arg evt "$event" '
      [.hooks[$evt][] | if type == "object" and has("hooks") then .hooks[] else . end]
    ' "$hooks_file" 2>/dev/null)

    local handler_count=$(echo "$handlers_json" | jq 'length')

    if [ "$handler_count" = "0" ]; then
        echo -e "${YELLOW}事件 '$event' 没有配置 handler${NC}"
        echo "可用事件："
        jq -r '.hooks | keys[]' "$hooks_file" 2>/dev/null | sed 's/^/  /'
        exit 0
    fi

    echo -e "${CYAN}─── 模拟触发: $event ───${NC}"
    echo "stdin: $stdin_json"
    echo "找到 $handler_count 个 handler"
    echo ""

    # 补全 stdin 的通用字段
    local full_stdin=$(echo "$stdin_json" | jq -c --arg evt "$event" --arg sid "$$" '
      {session_id: ($sid | tostring), cwd: (env.PWD // "."), hook_event_name: $evt} * .
    ')

    # 遍历执行每个 handler
    local idx=0
    while [ $idx -lt $handler_count ]; do
        local handler=$(echo "$handlers_json" | jq -c ".[$idx]")
        local htype=$(echo "$handler" | jq -r '.type')
        local hcmd=$(echo "$handler" | jq -r '.command // ""')
        local htimeout=$(echo "$handler" | jq -r '.timeout // 30')

        echo -e "${CYAN}[handler #$((idx+1))] ($htype)${NC}"

        if [ "$htype" != "command" ]; then
            echo -e "  ${YELLOW}⚠️  $htype 类型暂不支持模拟（需 ion hooks test 完整版）${NC}"
            echo ""
            idx=$((idx+1))
            continue
        fi

        if [ -z "$hcmd" ]; then
            echo -e "  ${RED}❌ command handler 缺少 command 字段${NC}"
            echo ""
            idx=$((idx+1))
            continue
        fi

        echo "  执行: $hcmd"
        echo "  超时: ${htimeout}s"

        # 真跑命令，stdin 写 JSON（临时关 errexit，因为 handler 可能 exit 2/3）
        local stdout_file=$(mktemp)
        local stderr_file=$(mktemp)
        set +e
        echo "$full_stdin" | timeout "$htimeout" bash -c "$hcmd" >"$stdout_file" 2>"$stderr_file"
        local exit_code=$?
        set -e

        local stdout=$(cat "$stdout_file")
        local stderr=$(cat "$stderr_file")
        rm -f "$stdout_file" "$stderr_file"

        echo "  exit code: $exit_code"

        # 显示 stdout（截断长输出）
        if [ -n "$stdout" ]; then
            local preview=$(echo "$stdout" | head -5)
            echo "  stdout:"
            echo "$preview" | sed 's/^/    /'
            local lines=$(echo "$stdout" | wc -l)
            [ "$lines" -gt 5 ] && echo "    ... ($lines 行，已截断)"
        fi

        if [ -n "$stderr" ]; then
            echo -e "  ${RED}stderr:${NC}"
            echo "$stderr" | head -3 | sed 's/^/    /'
        fi

        # 解释结果
        echo ""
        case $exit_code in
            0)
                # 尝试解析 stdout JSON
                if echo "$stdout" | jq -e . >/dev/null 2>&1; then
                    local decision=$(echo "$stdout" | jq -r '.decision // empty')
                    local additional=$(echo "$stdout" | jq -r '.hookSpecificOutput.additionalContext // .additionalContext // empty')
                    if [ "$decision" = "block" ]; then
                        local reason=$(echo "$stdout" | jq -r '.reason // "blocked"')
                        echo -e "  ${RED}结果: BLOCK${NC} (reason: $reason)"
                    elif [ -n "$additional" ]; then
                        echo -e "  ${GREEN}结果: CONTINUE${NC} (注入 additionalContext)"
                    else
                        echo -e "  ${GREEN}结果: CONTINUE${NC}"
                    fi
                else
                    # 纯文本 → additionalContext
                    echo -e "  ${GREEN}结果: CONTINUE${NC} (纯文本作为 additionalContext 注入)"
                fi
                ;;
            2)
                local reason="blocked by hook"
                if echo "$stdout" | jq -e . >/dev/null 2>&1; then
                    reason=$(echo "$stdout" | jq -r '.reason // .message // "blocked"')
                elif [ -n "$stderr" ]; then
                    reason="$stderr"
                fi
                echo -e "  ${RED}结果: BLOCK${NC} (exit 2, reason: $reason)"
                ;;
            3)
                echo -e "  ${YELLOW}结果: ASK${NC} (exit 3, 请求用户确认)"
                ;;
            *)
                echo -e "  ${YELLOW}结果: IGNORE${NC} (exit $exit_code, 非标准退出码，忽略)"
                ;;
        esac
        echo ""

        idx=$((idx+1))
    done

    echo -e "${CYAN}─── 完成 ───${NC}"
}

# ── 主入口 ──
check_jq

case "${1:-help}" in
    validate)
        shift
        cmd_validate "$@"
        ;;
    test)
        shift
        if [ $# -lt 1 ]; then
            echo -e "${RED}用法: hooks_test.sh test <event> [--stdin '<json>']${NC}"
            echo "事件名: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, SubagentStop, ..."
            exit 1
        fi
        cmd_test "$@"
        ;;
    list)
        cmd_list
        ;;
    help|--help|-h)
        echo "hooks_test.sh — 不依赖 Rust 的 hooks 验证脚本"
        echo ""
        echo "用法："
        echo "  bash hooks_test.sh validate [hooks.json路径]     校验配置格式"
        echo "  bash hooks_test.sh test <event> [--stdin '<json>']  模拟触发，真跑 handler"
        echo "  bash hooks_test.sh list                           列出生效的 hooks"
        echo ""
        echo "示例："
        echo "  bash scripts/hooks_test.sh validate .ion/hooks.json"
        echo "  bash scripts/hooks_test.sh test PreToolUse --stdin '{\"tool_name\":\"bash\",\"tool_input\":{\"command\":\"git commit --no-verify\"}}'"
        echo "  bash scripts/hooks_test.sh test UserPromptSubmit --stdin '{\"prompt\":\"你好\"}'"
        echo "  bash scripts/hooks_test.sh list"
        ;;
    *)
        echo -e "${RED}未知命令: $1${NC}"
        echo "用 bash hooks_test.sh help 查看用法"
        exit 1
        ;;
esac
