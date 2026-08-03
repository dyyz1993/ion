#!/usr/bin/env bash
# auto_evolve_local.sh — 本地化 A 驱动 B 自循环（不用 container）
#
# 流程：
#   1. git worktree 创建隔离工作目录
#   2. 在 worktree 跑 ion --agent developer 完成任务（read → edit → cargo test → commit）
#   3. 主仓库 fetch worktree 分支 + cargo test --lib 全量验证
#   4. 通过则 merge 到 master；失败则丢弃 worktree
#
# 用法：
#   bash scripts/auto_evolve_local.sh SE-20             # 单任务
#   bash scripts/auto_evolve_local.sh SE-01 SE-05 SE-12 # 多任务
#   bash scripts/auto_evolve_local.sh --all             # 全部 20 个
#   bash scripts/auto_evolve_local.sh --list            # 列出任务清单
#
# 环境：
#   ION_BIN (默认从 cargo which ion) — 用哪个 ion 二进制
#   ION_MODEL (默认 glm-5.2) — agent 用的模型
#   ION_PROVIDER (默认 zai) — provider
#   ION_SECURITY_PROFILE (默认 permissive) — 让 CommandGuard 放行 cargo/git
#   ION_TIMEOUT (默认 1800) — agent 单任务超时秒数
#   ION_SKIP_MERGE (默认 0) — 设 1 只跑不 merge（验证模式）

set -o pipefail
# 注意：不用 set -u，因为 task_prompt 字符串插值 + 多个局部变量在 set -u 下会误报 unbound

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

# 加载任务清单
source "$PROJECT_DIR/scripts/evolve_tasks.sh"

# 配置
ION_BIN="${ION_BIN:-$(which ion)}"
ION_MODEL="${ION_MODEL:-glm-5.2}"
ION_PROVIDER="${ION_PROVIDER:-zai}"
ION_SECURITY_PROFILE="${ION_SECURITY_PROFILE:-permissive}"
ION_TIMEOUT="${ION_TIMEOUT:-1800}"
ION_SKIP_MERGE="${ION_SKIP_MERGE:-0}"
WORKTREE_BASE="/tmp/ion-auto-evolve"
REPORT_DIR="/tmp/evolve_reports"
mkdir -p "$REPORT_DIR"

# 颜色
red()    { echo -e "\033[31m$*\033[0m"; }
green()  { echo -e "\033[32m$*\033[0m"; }
yellow() { echo -e "\033[33m$*\033[0m"; }
blue()   { echo -e "\033[34m$*\033[0m"; }

# ── 帮助 ──
usage() {
    cat <<EOF
用法: bash scripts/auto_evolve_local.sh [TASK_ID...] [--all] [--list]

选项:
  SE-XX          跑指定任务（可多个）
  --all          跑全部 20 个任务
  --list         列出任务清单
  --help         显示此帮助

环境变量:
  ION_BIN            ion 二进制路径（默认 which ion）
  ION_MODEL          模型（默认 glm-5.2）
  ION_PROVIDER       provider（默认 zai）
  ION_SECURITY_PROFILE  安全模式（默认 permissive）
  ION_TIMEOUT        单任务超时秒数（默认 1800）
  ION_SKIP_MERGE     1=只跑不 merge（默认 0）

示例:
  bash scripts/auto_evolve_local.sh SE-20
  bash scripts/auto_evolve_local.sh --all
EOF
}

# ── 列任务 ──
list_tasks() {
    blue "可用任务（${#TASKS[@]} 个）:"
    for task in "${TASKS[@]}"; do
        local id file method commit
        id=$(echo "$task"     | cut -d'|' -f1)
        file=$(echo "$task"   | cut -d'|' -f2)
        method=$(echo "$task" | cut -d'|' -f3)
        commit=$(echo "$task" | cut -d'|' -f6)
        echo "  $id  →  $file"
        echo "       方法: ${method:0:80}..."
        echo "       commit: $commit"
        echo ""
    done
}

# ── 检查前置 ──
check_prereqs() {
    local errs=0
    if [ -z "$ION_BIN" ] || ! command -v "$ION_BIN" >/dev/null 2>&1; then
        red "ERROR: ion 二进制不可用（ION_BIN=$ION_BIN）"
        errs=$((errs+1))
    fi
    if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        red "ERROR: 当前目录不是 git 仓库"
        errs=$((errs+1))
    fi
    # 主仓库必须干净
    if [ -n "$(git status --porcelain)" ]; then
        yellow "WARN: 主仓库有未提交改动，merge 时可能冲突"
    fi
    return $errs
}

# ── 跑单个任务 ──
run_task() {
    local task_str="$1"
    # 用 cut 解析（IFS='|' read 在 here-string + set -u 下不稳）
    local task_id target_file method_spec test_spec test_name commit_msg
    task_id=$(echo "$task_str"      | cut -d'|' -f1)
    target_file=$(echo "$task_str"  | cut -d'|' -f2)
    method_spec=$(echo "$task_str"  | cut -d'|' -f3)
    test_spec=$(echo "$task_str"    | cut -d'|' -f4)
    test_name=$(echo "$task_str"    | cut -d'|' -f5)
    commit_msg=$(echo "$task_str"   | cut -d'|' -f6)

    green "════════════════════════════════════════════════════════════"
    green "  任务 $task_id: $commit_msg"
    green "════════════════════════════════════════════════════════════"
    echo "  目标文件: $target_file"
    echo "  测试函数: $test_name"
    echo ""

    local wt_dir="$WORKTREE_BASE/$task_id"
    local branch="evolve/$task_id"
    local report="$REPORT_DIR/$task_id.md"
    local start_time=$(date +%s)

    # 清掉旧的 worktree（如果上次失败残留）
    if [ -d "$wt_dir" ]; then
        yellow "  清理残留 worktree: $wt_dir"
        git worktree remove --force "$wt_dir" 2>/dev/null
        git branch -D "$branch" 2>/dev/null
    fi

    # Step 1: 创建 worktree
    blue "▶ Step 1: 创建 worktree"
    if ! git worktree add "$wt_dir" -b "$branch" 2>&1; then
        red "  ✗ worktree 创建失败"
        write_report "$task_id" "FAILED" "worktree creation" 0 "" ""
        return 1
    fi
    echo ""

    # Step 2: 在 worktree 跑 agent
    blue "▶ Step 2: 跑 ion agent (cwd=$wt_dir, model=$ION_MODEL, profile=$ION_SECURITY_PROFILE)"
    local agent_log="$REPORT_DIR/${task_id}_agent.log"
    # prompt 写到临时文件，用 @file 引用（避免命令行 UTF-8 / 长度问题）
    # ★ 用 python3 写文件而非 cat heredoc：
    #   bash heredoc 在处理含特殊字节的内容（如非 ASCII 字符、NEL 等）时可能丢字节，
    #   导致文件 invalid UTF-8 → ion ReadTool 报错 "stream did not contain valid UTF-8"。
    #   python3 write + 显式 encoding='utf-8' 保证文件始终是合法 UTF-8。
    # ★ 写完后立即用 python3 再校验一遍，invalid 则 abort（防御性）。
    local prompt_file="$REPORT_DIR/${task_id}_prompt.txt"
    python3 -c "
import sys
prompt = '''Implement task $task_id code change.

[Target file] $target_file

[Method spec]
$method_spec

[Test spec]
$test_spec

[Steps (in order)]
1. Use the read tool to read $target_file and understand existing code
2. Use the edit tool to add the method implementation
3. Use the edit tool to add a unit test (#[test] fn $test_name)
4. Use the bash tool to run: cargo test --lib $test_name 2>&1
   - If it fails, read the error, fix the code, retry up to 3 times
5. After all tests pass, use the bash tool to run:
   git add $target_file && git commit -m \"$commit_msg\"

When done, reply with DONE.
'''
with open('$prompt_file', 'w', encoding='utf-8') as f:
    f.write(prompt)
# Validate UTF-8 (defense in depth)
with open('$prompt_file', 'rb') as f:
    data = f.read()
try:
    data.decode('utf-8')
except UnicodeDecodeError as e:
    print(f'ERROR: prompt file is not valid UTF-8 after write: {e}', file=sys.stderr)
    sys.exit(1)
"
    if [ $? -ne 0 ]; then
        red "  ✗ prompt 文件 UTF-8 校验失败，跳过此任务"
        git worktree remove --force "$wt_dir" 2>/dev/null
        git branch -D "$branch" 2>/dev/null
        write_report "$task_id" "FAILED" "prompt file UTF-8 invalid" 0 "" ""
        return 1
    fi

    (
        cd "$wt_dir"
        ION_SECURITY_PROFILE="$ION_SECURITY_PROFILE" \
        timeout "$ION_TIMEOUT" \
        "$ION_BIN" --agent developer --model "$ION_MODEL" --provider "$ION_PROVIDER" \
            "@$prompt_file" 2>&1 | tee "$agent_log"
    )
    local agent_rc=${PIPESTATUS[0]}
    echo ""

    if [ "$agent_rc" != "0" ]; then
        red "  ✗ agent 失败或超时（rc=$agent_rc）"
        red "  日志: $agent_log"
        cleanup_failed "$task_id" "$wt_dir" "$branch"
        write_report "$task_id" "FAILED" "agent execution (rc=$agent_rc)" $(($(date +%s) - start_time)) "$agent_log" ""
        return 1
    fi

    # Step 3: 检查 git 是否有新 commit
    blue "▶ Step 3: 检查 agent 是否 commit 了改动"
    local new_commits
    new_commits=$(cd "$wt_dir" && git log --oneline master..HEAD 2>/dev/null | wc -l | tr -d ' ')
    if [ "$new_commits" = "0" ]; then
        red "  ✗ agent 没 commit（可能任务没完成）"
        red "  日志: $agent_log"
        cleanup_failed "$task_id" "$wt_dir" "$branch"
        write_report "$task_id" "FAILED" "no commit produced" $(($(date +%s) - start_time)) "$agent_log" ""
        return 1
    fi
    green "  ✓ agent 产生 $new_commits 个 commit"
    (cd "$wt_dir" && git log --oneline master..HEAD)
    echo ""

    # Step 4: 在 worktree 跑全量 cargo test --lib
    blue "▶ Step 4: 全量验证（cargo test --lib）"
    local test_log="$REPORT_DIR/${task_id}_test.log"
    (
        cd "$wt_dir"
        cargo test --lib 2>&1 | tee "$test_log"
    )
    local test_rc=${PIPESTATUS[0]}
    if [ "$test_rc" != "0" ]; then
        red "  ✗ cargo test --lib 失败（rc=$test_rc）"
        red "  日志: $test_log"
        cleanup_failed "$task_id" "$wt_dir" "$branch"
        write_report "$task_id" "FAILED" "cargo test --lib failed" $(($(date +%s) - start_time)) "$agent_log" "$test_log"
        return 1
    fi
    local test_summary
    test_summary=$(grep "test result:" "$test_log" | tail -1)
    green "  ✓ $test_summary"
    echo ""

    # Step 4.5: U+FFFD 乱码检查
    if grep -rlP "[\x{FFFD}]" "$wt_dir/src" 2>/dev/null | head -1; then
        red "  ✗ 检测到 U+FFFD 乱码（agent 写入了非法字符）"
        cleanup_failed "$task_id" "$wt_dir" "$branch"
        write_report "$task_id" "FAILED" "U+FFFD garbage detected" $(($(date +%s) - start_time)) "$agent_log" ""
        return 1
    fi

    # Step 5: merge 回主仓库（除非 ION_SKIP_MERGE=1）
    if [ "$ION_SKIP_MERGE" = "1" ]; then
        yellow "▶ Step 5: 跳过 merge（ION_SKIP_MERGE=1）"
        yellow "  worktree 保留: $wt_dir"
        yellow "  分支: $branch"
        write_report "$task_id" "PASS_NO_MERGE" "skipped merge" $(($(date +%s) - start_time)) "$agent_log" "$test_log"
        return 0
    fi

    blue "▶ Step 5: merge 到 master"
    local merge_msg="auto-evolve: $task_id ($commit_msg)"
    if ! git merge --no-ff "$branch" -m "$merge_msg" 2>&1; then
        red "  ✗ merge 失败（可能冲突）"
        git merge --abort 2>/dev/null
        cleanup_failed "$task_id" "$wt_dir" "$branch"
        write_report "$task_id" "FAILED" "merge conflict" $(($(date +%s) - start_time)) "$agent_log" "$test_log"
        return 1
    fi
    green "  ✓ merged"
    local merged_commit
    merged_commit=$(git log --oneline -1 | awk '{print $1}')
    echo ""

    # Step 6: 清理
    git worktree remove "$wt_dir" 2>/dev/null
    git branch -d "$branch" 2>/dev/null

    green "════════════════════════════════════════════════════════════"
    green "  ✅ 任务 $task_id 完成（commit $merged_commit）"
    green "════════════════════════════════════════════════════════════"
    echo ""

    write_report "$task_id" "PASS" "merged $merged_commit" $(($(date +%s) - start_time)) "$agent_log" "$test_log"
    return 0
}

cleanup_failed() {
    local task_id="$1" wt_dir="$2" branch="$3"
    yellow "  清理失败的 worktree..."
    git worktree remove --force "$wt_dir" 2>/dev/null
    git branch -D "$branch" 2>/dev/null
}

# ── 写报告 ──
write_report() {
    local task_id="$1" status="$2" detail="$3" elapsed="$4" agent_log="$5" test_log="$6"
    local report="$REPORT_DIR/$task_id.md"
    {
        echo "# Auto-evolve Report: $task_id"
        echo ""
        echo "- **Status**: $status"
        echo "- **Detail**: $detail"
        echo "- **Elapsed**: ${elapsed}s"
        echo "- **Time**: $(date '+%Y-%m-%d %H:%M:%S')"
        echo ""
        if [ -n "$agent_log" ]; then
            echo "## Agent log (last 30 lines)"
            echo ""
            echo '```'
            tail -30 "$agent_log" 2>/dev/null
            echo '```'
            echo ""
        fi
        if [ -n "$test_log" ]; then
            echo "## Test result"
            echo ""
            echo '```'
            grep "test result:" "$test_log" 2>/dev/null | tail -3
            echo '```'
        fi
    } > "$report"
    echo "  报告: $report"
}

# ── 主入口 ──
main() {
    if [ $# -eq 0 ]; then
        usage
        exit 1
    fi

    check_prereqs || exit 1

    local tasks_to_run=()
    while [ $# -gt 0 ]; do
        case "$1" in
            --help|-h)
                usage
                exit 0
                ;;
            --list)
                list_tasks
                exit 0
                ;;
            --all)
                for task in "${TASKS[@]}"; do
                    tasks_to_run+=("$task")
                done
                shift
                ;;
            SE-*)
                local found
                found=$(find_task "$1")
                if [ -z "$found" ]; then
                    red "ERROR: 任务 $1 不存在（用 --list 看清单）"
                    exit 1
                fi
                tasks_to_run+=("$found")
                shift
                ;;
            *)
                red "ERROR: 未知参数 $1"
                usage
                exit 1
                ;;
        esac
    done

    if [ ${#tasks_to_run[@]} -eq 0 ]; then
        red "ERROR: 没指定任务"
        usage
        exit 1
    fi

    blue "ion 自循环启动"
    blue "  二进制: $ION_BIN"
    blue "  模型: $ION_MODEL / $ION_PROVIDER"
    blue "  安全模式: $ION_SECURITY_PROFILE"
    blue "  超时: ${ION_TIMEOUT}s"
    blue "  任务数: ${#tasks_to_run[@]}"
    blue "  跳过 merge: $ION_SKIP_MERGE"
    echo ""

    local pass=0 fail=0
    for task in "${tasks_to_run[@]}"; do
        if run_task "$task"; then
            pass=$((pass+1))
        else
            fail=$((fail+1))
        fi
    done

    echo ""
    blue "════════════════════════════════════════════════════════════"
    blue "  汇总: PASS=$pass FAIL=$fail"
    blue "════════════════════════════════════════════════════════════"

    [ "$fail" -eq 0 ]
}

main "$@"
