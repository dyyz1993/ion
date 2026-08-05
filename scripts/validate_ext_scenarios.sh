#!/usr/bin/env bash
# validate_ext_scenarios.sh — 跑扩展多场景测试
#
# 用法：
#   bash scripts/validate_ext_scenarios.sh EXT-02              # 单扩展所有场景
#   bash scripts/validate_ext_scenarios.sh EXT-02 02-S1        # 单个场景
#   bash scripts/validate_ext_scenarios.sh --all               # 全部
#
# 每个场景：跑 prompt → 导出 HTML → 跑专属指标 → 收集结果

set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

source "$PROJECT_DIR/scripts/ext_scenarios.sh"

ION_BIN="${ION_BIN:-$(which ion)}"
ION_MODEL="${ION_MODEL:-glm-5.2}"
ION_PROVIDER="${ION_PROVIDER:-zai}"
CHROME="${CHROME:-/Applications/Chromium.app/Contents/MacOS/Chromium}"
REPORT_DIR="/tmp/ext_scenario_reports"
mkdir -p "$REPORT_DIR"

red()    { printf "\033[31m%s\033[0m\n" "$*"; }
green()  { printf "\033[32m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
blue()   { printf "\033[34m%s\033[0m\n" "$*"; }

# 跑单个场景
# 输入：SCENARIO_LINE="ID|EXT_ID|NAME|PROMPT|PRE_SETUP|EXPECTED"
run_scenario() {
    local scenario_line="$1"
    IFS='|' read -r sid ext_id name prompt pre_setup expected <<< "$scenario_line"

    green "════════════════════════════════════════════════════════════"
    green "  $sid ($ext_id): $name"
    green "════════════════════════════════════════════════════════════"

    local work_dir="/tmp/ext_scenario_${sid}"
    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    # pre_setup
    if [ -n "$pre_setup" ] && type "$pre_setup" &>/dev/null; then
        blue "▶ Pre-setup: $pre_setup"
        "$pre_setup" "$work_dir"
    fi

    # 写 prompt 文件（UTF-8 安全）
    local prompt_file="$REPORT_DIR/${sid}_prompt.txt"
    python3 -c "
with open('$prompt_file', 'w', encoding='utf-8') as f:
    f.write('''$prompt''')
"

    # 跑 ion
    blue "▶ Step 1: 跑 prompt"
    local start_ts=$(date +%s)
    (
        cd "$work_dir"
        ION_PROJECT_DIR="$work_dir" \
        timeout 600 \
        "$ION_BIN" --agent developer --model "$ION_MODEL" --provider "$ION_PROVIDER" \
            "@$prompt_file" 2>&1 | tail -3
    )
    local rc=$?
    local elapsed=$(( $(date +%s) - start_ts ))
    if [ $rc -ne 0 ]; then
        yellow "  ⚠ ion exit=$rc (${elapsed}s)"
    else
        green "  ✓ ion 完成 (${elapsed}s)"
    fi

    # 导出 HTML
    blue "▶ Step 2: 导出 HTML"
    local latest_dir
    latest_dir=$(ls -dt ~/.ion/agent/sessions/* 2>/dev/null | head -1)
    local jsonl_file
    jsonl_file=$(ls "$latest_dir"/sess_*.jsonl "$latest_dir"/session.jsonl 2>/dev/null | head -1)
    if [ -z "$jsonl_file" ]; then
        red "  ✗ 找不到 session"
        return 1
    fi
    local sid_from_jsonl
    sid_from_jsonl=$(basename "$jsonl_file" .jsonl)
    local html_file="$REPORT_DIR/${sid}_${ext_id}.html"
    rm -f "$html_file"
    (cd "$latest_dir" && "$ION_BIN" --export "$sid_from_jsonl" 2>/dev/null)
    mv "$latest_dir/$sid_from_jsonl" "$html_file" 2>/dev/null
    if [ ! -s "$html_file" ]; then
        red "  ✗ HTML 导出失败"
        return 1
    fi
    # Copy hook_log if exists (for EXT-06)
    if [ -f "$work_dir/hook_log.txt" ]; then
        cp "$work_dir/hook_log.txt" "$REPORT_DIR/${sid}_hook_log.txt"
    fi
    green "  ✓ HTML: $html_file ($(wc -c < "$html_file") bytes)"

    # 跑专属指标
    blue "▶ Step 3: 专属硬性指标 (--ext $ext_id)"
    local report_json="$REPORT_DIR/${sid}_report.json"
    python3 "$PROJECT_DIR/scripts/validate_html.py" "$html_file" \
        --chrome "$CHROME" --ext "$ext_id" 2>"$report_json" | grep -E "✅|❌" | head -25

    # 解析 expected 是否全过
    python3 << PYEOF
import json, sys
with open("$report_json") as f:
    data = json.load(f)
expected_ids = "$expected".split(",") if "$expected" else []
checks = {c["id"]: c["status"] for c in data.get("checks", [])}
miss = [(eid, checks.get(eid, "MISSING")) for eid in expected_ids if checks.get(eid) != "PASS"]
if miss:
    print(f"  ⚠ 期望全过但失败：{miss}")
    sys.exit(2)
else:
    print(f"  ✓ 所有期望指标 PASS：{expected_ids}")
PYEOF
    local metric_rc=$?

    # 汇总：场景 PASS = 期望指标全过（metric_rc==0）。其他非期望指标失败算 warning。
    local total_pass=$(python3 -c "import json; print(json.load(open('$report_json'))['passed'])")
    local total_fail=$(python3 -c "import json; print(json.load(open('$report_json'))['failed'])")
    if [ $metric_rc -eq 0 ]; then
        if [ "$total_fail" = "0" ]; then
            green "  ✅ $sid PASS ($total_pass/$((total_pass+total_fail)))"
        else
            yellow "  ⚠ $sid PASS-with-warnings ($total_pass/$((total_pass+total_fail)), expected all PASS)"
        fi
        echo "$sid|PASS|$total_pass|$total_fail|$elapsed|$html_file" >> "$REPORT_DIR/summary.csv"
    else
        red "  ❌ $sid FAIL (pass=$total_pass fail=$total_fail elapsed=${elapsed}s)"
        red "     HTML: $html_file"
        echo "$sid|FAIL|$total_pass|$total_fail|$elapsed|$html_file" >> "$REPORT_DIR/summary.csv"
    fi
    echo ""
}

# ── 主入口 ──
main() {
    rm -f "$REPORT_DIR/summary.csv"
    echo "scenario|status|pass|fail|elapsed|html" > "$REPORT_DIR/summary.csv"

    if [ $# -eq 0 ]; then
        red "用法: bash scripts/validate_ext_scenarios.sh EXT-02 [scenario_id] | --all"
        exit 1
    fi

    local scenarios_to_run=()

    if [ "$1" = "--all" ]; then
        while IFS= read -r line; do
            [ -n "$line" ] && scenarios_to_run+=("$line")
        done < <(get_scenarios_for_ext "")
    elif [[ "$1" =~ ^EXT- ]]; then
        local ext_id="$1"
        if [ -n "$2" ]; then
            # 指定场景 ID
            while IFS= read -r line; do
                if [[ "$line" == "$2|"* ]]; then
                    scenarios_to_run+=("$line")
                    break
                fi
            done < <(get_scenarios_for_ext "$ext_id")
            if [ ${#scenarios_to_run[@]} -eq 0 ]; then
                red "场景 $2 不存在"
                exit 1
            fi
        else
            while IFS= read -r line; do
                [ -n "$line" ] && scenarios_to_run+=("$line")
            done < <(get_scenarios_for_ext "$ext_id")
        fi
    else
        red "未知参数: $1"
        exit 1
    fi

    blue "ion 扩展多场景测试"
    blue "  场景数: ${#scenarios_to_run[@]}"
    blue "  模型: $ION_MODEL / $ION_PROVIDER"
    echo ""

    local pass=0 fail=0
    for scenario in "${scenarios_to_run[@]}"; do
        if run_scenario "$scenario"; then
            pass=$((pass+1))
        else
            fail=$((fail+1))
        fi
    done

    blue "════════════════════════════════════════════════════════════"
    blue "  汇总: SCENARIO PASS=$pass FAIL=$fail"
    blue "  CSV: $REPORT_DIR/summary.csv"
    blue "════════════════════════════════════════════════════════════"

    cat "$REPORT_DIR/summary.csv"

    [ "$fail" -eq 0 ]
}

main "$@"
