#!/usr/bin/env bash
# validate_extension.sh — 扩展验证循环（goal 驱动 + 硬性校验）
#
# 用法：
#   bash scripts/validate_extension.sh EXT-01            # 验证单个扩展
#   bash scripts/validate_extension.sh EXT-01 EXT-02     # 验证多个
#   bash scripts/validate_extension.sh --all             # 全部
#   bash scripts/validate_extension.sh --list            # 列任务
#
# 流程：跑 prompt → 导出 HTML → 9 项硬性校验 → 报告

set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

source "$PROJECT_DIR/scripts/extension_tasks.sh"

ION_BIN="${ION_BIN:-$(which ion)}"
ION_MODEL="${ION_MODEL:-glm-5.2}"
ION_PROVIDER="${ION_PROVIDER:-zai}"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
REPORT_DIR="/tmp/ext_validation_reports"
mkdir -p "$REPORT_DIR"

red()    { echo -e "\033[31m$*\033[0m"; }
green()  { echo -e "\033[32m$*\033[0m"; }
yellow() { echo -e "\033[33m$*\033[0m"; }
blue()   { echo -e "\033[34m$*\033[0m"; }

# ── 列任务 ──
list_tasks() {
    blue "扩展验证任务（${#EXTENSION_TASKS[@]} 个）:"
    for task in "${EXTENSION_TASKS[@]}"; do
        IFS='|' read -r id name prompt keywords <<< "$task"
        echo "  $id  $name"
    done
}

# ── 验证单个扩展 ──
validate_one() {
    local task_str="$1"
    IFS='|' read -r ext_id ext_name test_prompt expected_keywords <<< "$task_str"

    green "════════════════════════════════════════════════════════════"
    green "  $ext_id: $ext_name"
    green "════════════════════════════════════════════════════════════"

    local work_dir="/tmp/ext_validate_$ext_id"
    local html_file="$REPORT_DIR/${ext_id}_${ext_name}.html"
    local report_file="$REPORT_DIR/${ext_id}_report.json"
    local start_time=$(date +%s)

    mkdir -p "$work_dir"

    # Step 1: 跑 prompt
    blue "▶ Step 1: 跑测试 prompt"
    local prompt_file="$REPORT_DIR/${ext_id}_prompt.txt"
    python3 -c "
with open('$prompt_file', 'w', encoding='utf-8') as f:
    f.write('''$test_prompt''')
# Validate UTF-8
with open('$prompt_file', 'rb') as f:
    data = f.read()
try:
    data.decode('utf-8')
except UnicodeDecodeError:
    import sys; sys.exit(1)
"
    if [ $? -ne 0 ]; then
        red "  ✗ prompt 文件 UTF-8 校验失败"
        echo '{"ext_id":"'$ext_id'","status":"FAIL","reason":"prompt UTF-8"}' > "$report_file"
        return 1
    fi

    (
        cd "$work_dir"
        ION_GRACEFUL_DRAIN_MS=5000 \
        timeout 300 \
        "$ION_BIN" --agent developer --model "$ION_MODEL" --provider "$ION_PROVIDER" \
            "@$prompt_file" 2>&1 | tail -3
    )
    local agent_rc=$?
    echo ""

    if [ $agent_rc -ne 0 ]; then
        yellow "  ⚠ agent 超时或非零退出（rc=$agent_rc），继续导出+校验"
    fi

    # Step 2: 导出 HTML
    blue "▶ Step 2: 导出 HTML"
    local latest_session_dir
    latest_session_dir=$(ls -dt ~/.ion/agent/sessions/* 2>/dev/null | head -1)
    local jsonl_file
    jsonl_file=$(ls "$latest_session_dir"/sess_*.jsonl "$latest_session_dir"/session.jsonl 2>/dev/null | head -1)

    if [ -z "$jsonl_file" ]; then
        red "  ✗ 找不到 session 文件"
        echo '{"ext_id":"'$ext_id'","status":"FAIL","reason":"no session"}' > "$report_file"
        return 1
    fi

    local sid
    sid=$(head -1 "$jsonl_file" | python3 -c "import json,sys;print(json.load(sys.stdin).get('id',''))" 2>/dev/null)

    rm -f "$html_file"
    "$ION_BIN" --export "$html_file" --session "$sid" 2>/dev/null
    if [ ! -f "$html_file" ]; then
        red "  ✗ 导出 HTML 失败"
        echo '{"ext_id":"'$ext_id'","status":"FAIL","reason":"export failed"}' > "$report_file"
        return 1
    fi
    local html_size
    html_size=$(wc -c < "$html_file")
    green "  ✓ HTML 导出成功: $html_file ($html_size bytes)"

    # Step 3: 硬性校验
    blue "▶ Step 3: 9 项硬性校验"
    python3 "$PROJECT_DIR/scripts/validate_html.py" "$html_file" --chrome "$CHROME" 2>"$report_file"
    local validate_rc=$?
    echo ""

    # 汇总
    local elapsed=$(( $(date +%s) - start_time ))
    if [ $validate_rc -eq 0 ]; then
        green "════════════════════════════════════════════════════════════"
        green "  ✅ $ext_id ($ext_name) 全部通过（${elapsed}s）"
        green "════════════════════════════════════════════════════════════"
    else
        red "════════════════════════════════════════════════════════════"
        red "  ❌ $ext_id ($ext_name) 有指标未通过（${elapsed}s）"
        red "  HTML: $html_file"
        red "════════════════════════════════════════════════════════════"
    fi

    # open HTML
    open "$html_file" 2>/dev/null
    return $validate_rc
}

# ── 主入口 ──
main() {
    if [ $# -eq 0 ]; then
        echo "用法: bash scripts/validate_extension.sh EXT-01 [EXT-02 ...] [--all] [--list]"
        exit 1
    fi

    local tasks_to_run=()
    while [ $# -gt 0 ]; do
        case "$1" in
            --list) list_tasks; exit 0 ;;
            --all)
                for task in "${EXTENSION_TASKS[@]}"; do
                    tasks_to_run+=("$task")
                done
                shift ;;
            EXT-*)
                local found
                found=$(find_ext_task "$1")
                if [ -z "$found" ]; then
                    red "ERROR: 任务 $1 不存在（用 --list 看清单）"
                    exit 1
                fi
                tasks_to_run+=("$found")
                shift ;;
            *) red "未知参数: $1"; exit 1 ;;
        esac
    done

    if [ ${#tasks_to_run[@]} -eq 0 ]; then
        red "没指定任务"
        exit 1
    fi

    blue "ion 扩展验证循环"
    blue "  任务数: ${#tasks_to_run[@]}"
    blue "  模型: $ION_MODEL / $ION_PROVIDER"
    echo ""

    local pass=0 fail=0
    for task in "${tasks_to_run[@]}"; do
        if validate_one "$task"; then
            pass=$((pass+1))
        else
            fail=$((fail+1))
        fi
        echo ""
    done

    blue "════════════════════════════════════════════════════════════"
    blue "  汇总: PASS=$pass FAIL=$fail"
    blue "════════════════════════════════════════════════════════════"

    [ "$fail" -eq 0 ]
}

main "$@"
