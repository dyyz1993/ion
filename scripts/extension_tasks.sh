#!/usr/bin/env bash
# extension_tasks.sh — 扩展验证任务清单（可 source）
#
# 格式：EXT_ID|EXT_NAME|TEST_PROMPT|EXPECTED_KEYWORDS

EXTENSION_TASKS=(
    "EXT-01|BashExtension|请依次用 bash 工具 background=true 后台运行以下命令：1. sleep 3; echo done-marker  2. for i in \$(seq 1 10); do echo \$i; [ \$i -eq 5 ] && exit 1; done  3. seq 1 500。每条单独调用一次。|bash_result,exit=,truncated,get_background_process"
    "EXT-02|GlobalMemoryExtension|请记住：我喜欢 Rust 语言。然后搜索记忆里关于语言偏好的内容。|memory_save,memory_search,global_memory"
    "EXT-03|DevServerDetectorExtension|请用 bash 工具后台运行命令：python3 -m http.server 8765 &。然后告诉我检测到了什么 dev server。|dev_servers,8765,detected"
    "EXT-04|FileSnapshotExtension|请用 write 工具创建文件 /tmp/snapshot_test.txt 内容为 hello。然后告诉我 snapshot 状态。|snapshot,checkpoint"
    "EXT-05|LspExtension|请用 read 工具读 src/agent/bash.rs 的前 10 行，然后用 lsp_check 工具检查诊断。|lsp_check,diagnostic"
    "EXT-06|HookExtension|请用 bash 工具执行 echo hook-test。检查 hook 是否被触发。|hook,triggered"
)

find_ext_task() {
    local ext_id="$1"
    for task in "${EXTENSION_TASKS[@]}"; do
        if [[ "$task" == "$ext_id|"* ]]; then
            echo "$task"
            return 0
        fi
    done
    return 1
}
