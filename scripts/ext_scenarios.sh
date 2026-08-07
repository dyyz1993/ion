#!/usr/bin/env bash
# ext_scenarios.sh — 扩展多场景测试清单（可 source）
#
# 每个场景格式：
#   SCENARIO_ID|EXT_ID|NAME|PROMPT|PRE_SETUP|EXPECTED_METRICS
#
# PRE_SETUP：bash 函数名（在跑 prompt 前调用，准备 hooks.json / 文件系统等）
# EXPECTED_METRICS：跑 validate_html.py --ext 时期望全过的指标 ID 列表（逗号分隔）
#
# 设计原则：每个 prompt 含 10 个有序主题，强制 LLM 多轮迭代（避免 1-2 轮就结束）

# ── EXT-02 GlobalMemory 场景（每个 10 步序列）──
EXT02_SCENARIOS=(
    "02-S1|EXT-02|完整 save+search 工作流|按以下 10 步顺序执行，每步用对应工具：1. 用 memory_save 保存：内容='用户喜欢 Rust 语言'，tags=[language,rust,preference]，category=user-preferences。2. 用 memory_save 保存：内容='用户在 macOS Sonoma 上工作'，tags=[env,macos,os]，category=environment。3. 用 memory_save 保存：内容='项目使用 SQLite + tokio 技术栈'，tags=[stack,sqlite,tokio,rust]，category=tech-stack。4. 用 memory_save 保存：内容='代码风格禁止 any 类型，必须显式返回类型'，tags=[style,rules]，category=coding-rules。5. 用 memory_search 搜 'language preference'，确认找到第 1 条。6. 用 memory_search 搜 'macos'，确认找到第 2 条。7. 用 memory_search 搜 'rust'，应该命中多条。8. 用 memory_search global=true 搜 'rust'，看跨 project 是否有更多。9. 用 memory_search 搜 '完全没可能存在的关键字 xyzqwerty'，确认返回空数组不报错。10. 用 bash 创建 /tmp/mem_audit.txt 写入 audit log，最后报告：保存了几条 memory、搜了几个关键字、命中模式总结。||02-M1,02-M2,02-M3,02-M4,02-M7,02-M8"
    "02-S2|EXT-02|查询边界 + 错误处理|按以下 10 步顺序执行：1. 用 memory_save 保存一条正常 memory。2. 用 memory_search 搜一个肯定没有的关键字 zzqqxxww。3. 用 memory_search 搜空字符串。4. 用 memory_save 故意不传 content 参数（应该返回错误）。5. 用 memory_save 传空 content（应该报错或拒绝）。6. 用 memory_save 传超长 content（重复 a 字符 5000 次）。7. 用 memory_search 搜超长 query（重复 b 字符 1000 次）。8. 用 memory_save 保存一条 tags 为空数组的 memory。9. 用 memory_save 保存一条 category 为空的 memory。10. 报告：哪些操作成功、哪些失败、错误信息格式。||02-M3,02-M8"
    "02-S3|EXT-02|tags + category 组合检索|按以下 10 步顺序执行：1. memory_save: 内容='async 函数不要 unwrap，用问号传播错误'，tags=[async,error-handling,rust]，category=best-practice。2. memory_save: 内容='tokio::spawn 需要 Send bound'，tags=[tokio,async,send],category=warning。3. memory_save: 内容='SQLite 连接池用 r2d2'，tags=[sqlite,pool,rust],category=recommendation。4. memory_save: 内容='HTTP 客户端推荐 reqwest'，tags=[http,client,recommendation],category=recommendation。5. memory_search 搜 'async'，应找到第 1+2 条。6. memory_search 搜 'recommendation'，应找到第 3+4 条。7. memory_search 搜 'rust'，应找到全部 4 条。8. memory_search 搜 'tokio'，应找到第 2+3 条。9. memory_search global=true 搜 'recommendation'，看跨 project 是否有更多。10. 报告：检索结果分类、tag 命中模式、跨 project 差异。||02-M1,02-M2,02-M3,02-M4"
)

# ── EXT-03 DevServerDetector 场景（10 步多 server + 验证 + 清理）──
EXT03_SCENARIOS=(
    "03-S1|EXT-03|多 dev server 启停 + 验证|按以下 10 步顺序执行：1. 用 bash background=true 启 python3 -m http.server 8765。2. 用 get_background_process 列出进程，确认 8765 在运行。3. 用 bash curl -s http://localhost:8765/ 验证服务可访问。4. 用 bash background=true 启 python3 -m http.server 8766。5. 用 bash background=true 启 python3 -m http.server 8767。6. 用 get_background_process 列出所有 3 个 server。7. 用 bash curl http://localhost:8766/ 和 http://localhost:8767/ 都验证。8. 用 kill_process 杀 8765（用 bid）。9. 用 get_background_process 确认 8765 已死，8766/8767 还活着。10. 报告：检测到的端口列表、PID 清单、当前存活 server。||03-M1,03-M2,03-M4,03-M5"
    "03-S2|EXT-03|端口占用 + 失败处理|按以下 10 步顺序执行：1. 用 bash 占用端口 9999（前台启 python3 -m http.server 9999）。2. 用 bash background=true 再启 python3 -m http.server 9999（应该失败）。3. 用 get_background_process 看失败原因。4. 用 bash 杀掉第 1 步前台进程。5. 用 bash background=true 启 python3 -m http.server 9999（现在应该成功）。6. 用 bash background=true 启 python3 -m http.server 8888。7. 用 bash curl http://localhost:9999/ 和 http://localhost:8888/ 验证。8. 用 get_background_process 列出活跃 server。9. 用 kill_process 杀 8888。10. 报告：端口冲突如何检测、错误信息、最终存活 server。||03-M1,03-M5"
    "03-S3|EXT-03|多语言 dev server|按以下 10 步顺序执行：1. 用 bash background=true 启 python3 -m http.server 8000。2. 用 bash background=true 启 python3 -m http.server 8001。3. 用 bash background=true 启 python3 -m http.server 8002。4. 用 get_background_process 列出所有 server。5. 用 bash curl http://localhost:8000/ 验证。6. 用 bash curl http://localhost:8001/ 验证。7. 用 bash curl http://localhost:8002/ 验证。8. 用 kill_process 杀 8001。9. 用 get_background_process 确认 8001 死了，8000/8002 活着。10. 报告：3 个 server 同时检测能力、并发性能、清理策略。||03-M1,03-M2,03-M5"
)

# ── EXT-04 FileSnapshot 场景（10 步多文件 + 编辑 + diff）──
EXT04_SCENARIOS=(
    "04-S1|EXT-04|完整项目搭建 + 多次编辑|按以下 10 步顺序执行：1. 用 write 创建 Cargo.toml，内容是基本 Rust lib 配置。2. 用 write 创建 src/lib.rs，内容是 pub fn add 函数。3. 用 write 创建 src/main.rs，内容是 fn main 调用 add。4. 用 read 读 src/lib.rs 确认写入。5. 用 bash 跑 ls -la 看目录结构。6. 用 write 覆盖 src/lib.rs，新增 sub 函数。7. 用 write 再次覆盖 src/lib.rs，加 mul 函数。8. 用 read 读 src/lib.rs 看最终内容。9. 用 grep 在 src/ 下搜 fn 关键字看所有函数。10. 报告：创建/修改了几个文件、snapshot 数量、最终函数清单。||04-M1,04-M4,04-M5"
    "04-S2|EXT-04|多文件 + 验证 + diff|按以下 10 步顺序执行：1. 用 write 创建 README.md 写项目说明。2. 用 write 创建 src/utils.rs。3. 用 write 创建 src/models.rs。4. 用 write 创建 tests/integration.rs。5. 用 bash 跑 ls -R 看目录结构。6. 用 write 覆盖 src/utils.rs 加新内容。7. 用 write 覆盖 src/models.rs 加新字段。8. 用 read 读所有文件确认。9. 用 grep 搜所有文件里的特定关键字。10. 报告：文件总数、修改次数、snapshot 链路。||04-M1,04-M4"
    "04-S3|EXT-04|编辑链 + 文件删除|按以下 10 步顺序执行：1. 用 write 创建 config.toml。2. 用 write 覆盖 config.toml 加配置项。3. 用 write 再次覆盖 config.toml 改值。4. 用 write 第四次覆盖。5. 用 read 读 config.toml 看最终值。6. 用 bash 创建 data.txt。7. 用 bash 跑 mv data.txt data_renamed.txt。8. 用 bash 跑 rm config.toml.bak 如果存在。9. 用 grep 在当前目录找特定内容。10. 报告：编辑历史、snapshot 状态、文件最终清单。||04-M1,04-M4,04-M5"
)

# ── EXT-05 Lsp 场景（10 步 create → error → fix → verify 循环）──
# 关键：write 在当前目录，cargo check 在 session cwd 跑（不跟随 file_path）
EXT05_SCENARIOS=(
    "05-S1|EXT-05|完整 Rust 项目 + 干净编译|按以下 10 步顺序执行（必须在当前目录写）：1. 用 write 创建 Cargo.toml（含 lib path 配置）。2. 用 write 创建 src/lib.rs，内容是 pub fn add 函数。3. 用 bash 跑 ls -la 确认目录结构。4. 用 bash 跑 cargo build --lib 验证编译。5. 用 read 读 src/lib.rs 确认内容。6. 用 write 覆盖 src/lib.rs 加新函数 sub。7. 用 bash 跑 cargo build --lib 确认还是干净。8. 用 grep 在 src/ 下搜 fn 关键字看所有函数。9. 用 write 在 src/lib.rs 末尾加 cfg test mod。10. 报告：编译状态、文件最终内容、函数清单。||05-M1,05-M6,05-M7"
    "05-S2|EXT-05|引入类型错误 + 验证诊断|按以下 10 步顺序执行（必须在当前目录写）：1. 用 write 创建 Cargo.toml。2. 用 write 创建 src/lib.rs，写一个正确的函数。3. 用 bash 跑 cargo build --lib 确认初始干净。4. 用 write 覆盖 src/lib.rs，故意引入类型错误：let x: i32 = 等于一个字符串字面量。5. 等一下，让 LSP 后台 cargo check 跑完。6. 用 bash 跑 ls 看文件状态。7. 用 read 读 src/lib.rs 确认错误代码。8. 描述 LSP 注入的诊断信息（应该看到 mismatched types 错误代码）。9. 用 bash 跑 cargo build --lib 主动验证编译错误。10. 报告：诊断格式、错误代码、文件路径、行号。||05-M1,05-M3,05-M4,05-M6"
    "05-S3|EXT-05|错误到修复完整循环|按以下 10 步顺序执行：1. 用 write 创建 Cargo.toml。2. 用 write 创建 src/lib.rs，故意写错类型：let x: i32 等于一个字符串。3. 用 bash 跑 cargo build --lib 验证有错误。4. 等 LSP 注入诊断（应该看到 mismatched types）。5. 用 read 读 src/lib.rs 看当前错误代码。6. 用 write 修复 src/lib.rs，改为正确类型。7. 等 LSP 重新 check 应该看到错误减少。8. 用 bash 跑 cargo build --lib 确认干净。9. 用 read 读 src/lib.rs 确认修复后代码。10. 报告：错误前后对比、LSP 诊断次数、最终编译状态。||05-M1,05-M3"
)

# ── EXT-06 Hook 场景（10 步多事件触发）──
EXT06_SCENARIOS=(
    "06-S1|EXT-06|多 bash + write 触发 PostToolUse|按以下 10 步顺序执行：1. 用 bash 执行 echo step 1。2. 用 bash 执行 echo step 2。3. 用 write 创建 /tmp/hook_s1.txt 写 hello hook。4. 用 bash 执行 ls /tmp/hook_s1.txt。5. 用 write 覆盖 /tmp/hook_s1.txt 写 updated content。6. 用 bash 执行 cat /tmp/hook_s1.txt。7. 用 bash 执行 echo step 7。8. 用 read 读 /tmp/hook_s1.txt。9. 用 bash 执行 echo step 9。10. 报告执行了几个 bash 和几个 write（PostToolUse hook 应该触发多次）。|setup_hook_posttooluse|06-M1,06-M2,06-M8"
    "06-S2|EXT-06|完整工作流 + Stop hook|按以下 10 步顺序执行：1. 用 bash 执行 echo start。2. 用 write 创建 /tmp/hook_s2_a.txt。3. 用 write 创建 /tmp/hook_s2_b.txt。4. 用 bash 跑 ls /tmp/hook_s2_star。5. 用 read 读两个文件。6. 用 bash 跑 wc -l /tmp/hook_s2_star。7. 用 bash 执行 echo processing。8. 用 write 覆盖 /tmp/hook_s2_a.txt 加内容。9. 用 bash 跑 cat /tmp/hook_s2_a.txt。10. 报告完成（这步触发 Stop hook）。|setup_hook_stop|06-M1,06-M2,06-M8"
    "06-S3|EXT-06|SessionStart 后长流程|按以下 10 步顺序执行（SessionStart hook 在开头已触发）：1. 用 bash 执行 echo session started。2. 用 bash 跑 pwd。3. 用 bash 跑 date。4. 用 write 创建 /tmp/hook_s3_log.txt。5. 用 bash 执行 ls -la /tmp/hook_s3_log.txt。6. 用 write 覆盖加更多内容。7. 用 bash 跑 wc -c /tmp/hook_s3_log.txt。8. 用 read 读 /tmp/hook_s3_log.txt。9. 用 bash 执行 echo finalizing。10. 报告完整工作流总结。|setup_hook_sessionstart|06-M1,06-M2,06-M8"
)

# ── Hook 配置 setup 函数 ──
setup_hook_posttooluse() {
    local dir="$1"
    mkdir -p "$dir/.ion"
    rm -f "$dir/hook_log.txt"
    cat > "$dir/.ion/hooks.json" << 'EOF'
{
  "version": 1,
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "echo \"[HOOK-PostToolUse] bash called at $(date)\" >> $ION_PROJECT_DIR/hook_log.txt"
          }
        ]
      },
      {
        "matcher": "write",
        "hooks": [
          {
            "type": "command",
            "command": "echo \"[HOOK-PostToolUse] write called at $(date)\" >> $ION_PROJECT_DIR/hook_log.txt"
          }
        ]
      }
    ]
  }
}
EOF
}

setup_hook_stop() {
    local dir="$1"
    mkdir -p "$dir/.ion"
    rm -f "$dir/hook_log.txt"
    cat > "$dir/.ion/hooks.json" << 'EOF'
{
  "version": 1,
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo \"[HOOK-Stop] session ending at $(date)\" >> $ION_PROJECT_DIR/hook_log.txt"
          }
        ]
      }
    ]
  }
}
EOF
}

setup_hook_sessionstart() {
    local dir="$1"
    mkdir -p "$dir/.ion"
    rm -f "$dir/hook_log.txt"
    cat > "$dir/.ion/hooks.json" << 'EOF'
{
  "version": 1,
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo \"[HOOK-SessionStart] reason=$ION_REASON at $(date)\" >> $ION_PROJECT_DIR/hook_log.txt"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo \"[HOOK-PostToolUse] tool=$ION_TOOL_NAME at $(date)\" >> $ION_PROJECT_DIR/hook_log.txt"
          }
        ]
      }
    ]
  }
}
EOF
}

# ── Phase 2 setup functions for new modules ──
setup_monitor_basic() {
    local dir="$1"
    mkdir -p "$dir/.ion/monitors"
    rm -f "$dir/.ion/monitors/"*.json
}

setup_rules_basic() {
    local dir="$1"
    mkdir -p "$dir/.ion/rules"
    rm -f "$dir/.ion/rules/"*.md "$dir/.ion/rules/"*.mdc
}

setup_workflow_pass() {
    local dir="$1"
    mkdir -p "$dir/.ion"
    cat > "$dir/.ion/agent.md" << 'EOF'
---
workflow:
  gate_command: "test -f /tmp/wf_gate_s1.pass && echo PASS || echo FAIL"
  gate_expected: "PASS"
  max_retries: 5
---
Default agent with a workflow gate.
EOF
}

setup_workflow_fail() {
    local dir="$1"
    mkdir -p "$dir/.ion"
    cat > "$dir/.ion/agent.md" << 'EOF'
---
workflow:
  gate_command: "test -f /tmp/wf_gate_s2.pass && echo PASS || echo FAIL"
  gate_expected: "PASS"
  max_retries: 5
---
Default agent with a workflow gate that fails until /tmp/wf_gate_s2.pass exists.
EOF
}

setup_workflow_exhaust() {
    local dir="$1"
    mkdir -p "$dir/.ion"
    cat > "$dir/.ion/agent.md" << 'EOF'
---
workflow:
  gate_command: "echo FAIL"
  gate_expected: "PASS"
  max_retries: 2
---
Default agent with a workflow gate that always fails (max_retries=2).
EOF
}

# ── Phase 2 scenarios (EXT-07 ~ EXT-24) ──
EXT07_SCENARIOS=(
    "07-S1|EXT-07|正常闭环：设目标→修复→通过|按以下 10 步顺序执行：1. 用 goal_set 声明一个目标：objective='在 /tmp/goal_demo 目录下创建一个 add 函数（参数 a,b 返回 a+b）并通过 cargo build'，让系统自动生成 checks。2. 用 bash 跑 mkdir -p /tmp/goal_demo/src 创建目录。3. 用 write 创建 /tmp/goal_demo/Cargo.toml 写一个最小 lib 配置（[package] name=goal_demo [lib] path=src/lib.rs）。4. 用 write 创建 /tmp/goal_demo/src/lib.rs 写 pub fn add(a: i32, b: i32) -> i32 { a + b }。5. 用 bash 跑 cd /tmp/goal_demo && cargo build --lib 验证编译。6. 报告编译是否成功（这是 goal 的第一次 verification）。7. 用 read 读 /tmp/goal_demo/src/lib.rs 确认内容。8. 用 bash 跑 grep -n 'pub fn add' /tmp/goal_demo/src/lib.rs 确认函数存在。9. 用 goal_refine 增量更新 objective_patch='add 函数已实现并通过 cargo build'，保持 progress 不变。10. 报告：goal_set 返回的 goal_id、check_count、iterations.jsonl 记录的 verification 次数。||07-M1,07-M2,07-M3,07-M4"
    "07-S2|EXT-07|false-finish 拦截：声称完成但 check 失败|按以下 10 步顺序执行：1. 用 goal_set 声明 objective='让命令 false 退出 0'，显式带 checks=[{name:'must_pass_false',check_type:'ci',rationale:'test',command:'false',pass_criteria:{kind:'exit_code',expected:0},must_pass:true}]。2. 用 bash 跑 true 假装修复了问题。3. 用 read 读取 /tmp 下任意文件假装在工作。4. 用 bash 跑 echo 'I fixed it' 声称修复完成。5. 现在系统应触发 on_gate_check：false 必然 exit=1，check 失败，必须 RetryWith。6. 用 bash 跑 echo 'second attempt' 表示收到 retry。7. 用 bash 跑 echo 'try different approach'。8. 用 bash 跑 echo 'still trying'。9. 用 goal_refine 把 must_pass_false 改为 command='true'（relax check）。10. 报告：iterations.jsonl 里 failed_checks 历史、RetryWith 消息内容、progress 趋势（应被分类为 Stagnant 或 Oscillating）。||07-M1,07-M2,07-M3,07-M5"
    "07-S3|EXT-07|边界：错误调用 + 重复检测|按以下 10 步顺序执行：1. 用 goal_set 不传 objective 参数（应返回明确错误，不 panic）。2. 用 goal_set 传 objective='' 空字符串（验证非空校验）。3. 用 goal_set 传 objective='ok' 但 checks=[{name:'bad'}] 故意 malformed check（应反序列化失败报错）。4. 用 goal_refine 在没有 active goal 时调用（应返回 'no active goal' 错误）。5. 用 goal_set 正常设一个目标 objective='repeat test' checks=[{name:'ci_true',check_type:'ci',rationale:'r',command:'true',pass_criteria:{kind:'exit_code',expected:0},must_pass:true}]。6. 用 bash 跑 echo 'first attempt'（让 LLM 报告 done）。7. 用 bash 跑 echo 'first attempt'（重复相同 plan，触发 repetitive guard 的累计）。8. 用 bash 跑 echo 'first attempt'（第三次相同）。9. 用 bash 跑 echo 'fourth different attempt'。10. 报告：哪些调用返回错误、错误信息格式、repetitive 检测是否启动（看 last_action_plan 相似度）。||07-M1,07-M2,07-M3,07-M5"
)

EXT08_SCENARIOS=(
    "08-S1|EXT-08|正常添加+验证+查询|按以下 10 步顺序执行，全部通过 extension_rpc 调 monitor 方法：1. 用 extension_rpc monitor method='validate' params={name:'test-mon', interval_secs:60, script:'echo hello', agent:'developer', prompt_template:'got: {output}'} 验证配置合法。2. 用 extension_rpc monitor method='add' 添加同名 monitor（应返回 validated=true activated 取决于 registry）。3. 用 bash 跑 ls .ion/monitors/ 确认 test-mon.json 文件已写入。4. 用 extension_rpc monitor method='list' 列出所有 monitor，确认 test-mon 在列表里。5. 用 extension_rpc monitor method='test' params={script:'echo triggered-output', prompt_template:'got: {output}'} dry-run 脚本。6. 用 extension_rpc monitor method='status' 查看 monitor 状态。7. 用 bash 跑 cat .ion/monitors/test-mon.json 确认文件内容。8. 用 extension_rpc monitor method='disable' params={name:'test-mon'} 禁用。9. 用 extension_rpc monitor method='enable' params={name:'test-mon'} 重新启用。10. 报告：add 返回的 validated/activated 字段、status 里 trigger_count、test dry-run 的 would_trigger 字段。|setup_monitor_basic|08-M1,08-M2,08-M3,08-M4"
    "08-S2|EXT-08|错误处理：非法配置+重复名|按以下 10 步顺序执行：1. 用 extension_rpc monitor method='add' params={name:'', script:'echo x'} 添加空 name（应报 'missing name'）。2. 用 extension_rpc monitor method='add' params={name:'bad/name', script:'echo x'} 试路径穿越（应被 validate_name 拒绝）。3. 用 extension_rpc monitor method='add' params={name:'ok', interval_secs:0, script:'echo x'} interval=0（应报 'interval_secs must be >= 1'）。4. 用 extension_rpc monitor method='add' params={name:'ok', interval_secs:99999, script:'echo x'} interval 过大（应报 '> 86400'）。5. 用 extension_rpc monitor method='add' params={name:'ok', interval_secs:60, script:''} 空 script（应报 'script must not be empty'）。6. 用 extension_rpc monitor method='add' params={name:'ok', interval_secs:60, script:'echo x', prompt_template:'no placeholder'} 缺 {output}（应报 'missing placeholder'）。7. 用 extension_rpc monitor method='add' 正常添加 name='dup'。8. 用 extension_rpc monitor method='add' 再次添加 name='dup'（应报 'already exists'）。9. 用 extension_rpc monitor method='add' params={name:'unknown'} method='enable' 启用不存在的（应报 'not found'）。10. 报告：每个错误对应的 error message 模式、validate vs add 的差异。|setup_monitor_basic|08-M1,08-M4,08-M5"
    "08-S3|EXT-08|active pipeline 持久化|按以下 10 步顺序执行：1. 用 extension_rpc monitor method='list_active' 查看当前 active pipelines（应该为空或列表）。2. 用 extension_rpc monitor method='mark_active' params={monitor:'gh-issues', key:'issue-42', worker_id:'w-1', stage:'developer'} 标记一个 active。3. 用 extension_rpc monitor method='check_active' params={monitor:'gh-issues', key:'issue-42'} 确认 is_active=true。4. 用 bash 跑 cat ~/.ion/agent/active-pipelines.json 确认文件已持久化。5. 用 extension_rpc monitor method='list_active' 确认刚才标记的在列表里。6. 用 extension_rpc monitor method='mark_active' params={monitor:'gh-issues', key:'issue-42', stage:'reviewer'} 更新同一 pipeline 的 stage。7. 用 extension_rpc monitor method='check_active' 再次确认仍 active=true。8. 用 extension_rpc monitor method='release_active' params={monitor:'gh-issues', key:'issue-42'} 释放。9. 用 extension_rpc monitor method='check_active' 确认 is_active=false。10. 报告：mark_active 的 updated 字段、list_active 的 count 字段、active-pipelines.json 的内容变化。|setup_monitor_basic|08-M1,08-M3,08-M4"
)

EXT09_SCENARIOS=(
    "09-S1|EXT-09|前台+后台+管理完整流程|按以下 10 步顺序执行：1. 用 bash 执行 echo 'sync hello'（前台，看 exit=0 + output）。2. 用 bash background=true 启 python3 -m http.server 9091（后台，返回 bid）。3. 用 get_background_process 列所有进程，确认刚才的 bid 在运行。4. 用 bash 跑 curl -s http://localhost:9091/ 验证服务。5. 用 bash background=true 启 python3 -m http.server 9092。6. 用 get_background_process 查单个 bid 详情（head/tail 行）。7. 用 kill_process 杀掉 9091 的 bid。8. 用 get_background_process 确认 9091 已死、9092 还活。9. 用 bash 跑 sleep 0.1 && echo done 验证短时前台命令仍正常。10. 报告：bid 格式（6 字符 base36）、exit code 来源、processes.json 持久化路径。||09-M1,09-M2,09-M3,09-M4,09-M5"
    "09-S2|EXT-09|write_stdin 交互+timeoutBackground|按以下 10 步顺序执行：1. 用 bash background=true 启 cat（等待 stdin 输入）。2. 记下返回的 bid。3. 用 write_stdin 向该 bid 发 'hello from stdin'。4. 用 write_stdin 再次发 'second line'。5. 用 get_background_process 看 cat 进程的 output（应累积两行）。6. 用 kill_process 杀掉 cat。7. 用 bash timeoutBackground=true timeout=2 跑 sleep 10（应 2 秒后转后台 + 最终 exit=timeout）。8. 用 get_background_process 看 sleep 进程状态。9. 等几秒后再用 get_background_process 确认 sleep 已被 kill。10. 报告：write_stdin 是否正确投递、timeoutBackground 的 exit code、deliverAs 默认值。||09-M1,09-M2,09-M4,09-M5"
    "09-S3|EXT-09|错误处理+边界|按以下 10 步顺序执行：1. 用 bash 跑 false（应 exit=1，不 panic）。2. 用 bash 跑 exit 42（应 exit=42）。3. 用 bash 跑 nonexistent_cmd_xyz（应 exit=127 + 报 command not found）。4. 用 kill_process 杀一个不存在的 bid='zzzzzz'（应返回 not_found 错误但不崩溃）。5. 用 bash 跑 echo 1-divided-by-0-test 算术除零（看 shell 行为）。6. 用 bash timeout=2 跑 sleep 10（前台 2 秒超时，应被 kill）。7. 用 bash background=true 启一个会立即失败的命令如 ls /nonexistent-dir（后台 exit!=0）。8. 用 get_background_process 查看那个失败进程的 status + exit_code。9. 用 write_stdin 向不存在 bid 发数据（应报错）。10. 报告：每种错误对应的 exit code、错误消息是否可读、processes.json 里失败进程的 status 字段。||09-M1,09-M2,09-M3,09-M5"
)

EXT10_SCENARIOS=(
    "10-S1|EXT-10|save→输入→注入完整链路|按以下 10 步顺序执行：1. 用 memory_save 保存 content='用户喜欢 dark mode 界面' description='UI 偏好' category='ui-preference' tags=['ui','theme','dark']。2. 用 memory_save 保存 content='项目用 React + TypeScript' description='技术栈' category='tech-stack' tags=['react','typescript']。3. 用 bash 跑 ls <project_dir>/.ion/memory/outlines/ 确认 JSON 文件已写入。4. 用 bash 跑 cat <project_dir>/.ion/memory/index.json 看 outline 索引。5. 发一句话：'帮我看看 UI 怎么配置'（这会触发 on_input 检索 → 标记 pending）。6. 等下一轮 LLM 调用，on_context 应注入 <memory_context> 含 dark mode。7. 用 bash 跑 cat <session_dir>/memory/injected.json 看 inject 记录（应有 outline + file_hash + turn）。8. 用 bash 跑 cat <session_dir>/memory/transcript/input.jsonl 看 transcript 累积。9. 用 memory_search 搜 'ui' 应命中 dark mode 那条。10. 报告：memory_outline 在 system prompt 里、注入触发条件（hash 变化或 > 20 轮）、transcript 条目数。||10-M1,10-M2,10-M3,10-M4"
    "10-S2|EXT-10|consolidation + 多轮触发|按以下 10 步顺序执行：1. 用 memory_save 保存 5 条不同的记忆（不同 category/tags）。2. 发一句 'msg 1'（turn 1，transcript +1）。3. 发一句 'msg 2'（turn 2）。4. 发一句 'msg 3'（turn 3）。5. 发一句 'msg 4'（turn 4）。6. 发一句 'msg 5'（turn 5，应触发项目级 consolidation + emit memory_consolidated）。7. 用 bash 跑 cat <project_dir>/.ion/memory/index.json 看 entry_count 是否被重新统计。8. 用 memory_search 搜一个肯定命中的关键字。9. 发一句 'msg 6' 触发新一轮（on_input 应再次检索，但已注入的 outline 在窗口内不再重复注入）。10. 报告：consolidation 触发的 turn（5）、transcript 条目数（6+）、inject 窗口（20 轮）行为。||10-M1,10-M2,10-M3,10-M5"
    "10-S3|EXT-10|边界：空查询+超长+错误|按以下 10 步顺序执行：1. 用 memory_save 故意不传 content（应报 'missing content'）。2. 用 memory_save 传 content='' 空（应允许但生成空条目，看 behavior）。3. 用 memory_save 传 content='正常记忆用于后续测试'。4. 用 memory_search 查询一个肯定没有的关键字 'zzqqxxww'（应返回空数组 + emit memory_skipped reason=no_match）。5. 用 memory_search 查空 query（应返回所有条目）。6. 用 memory_save 传超长 content（重复 'abcdefghij' 100 次 = 1000 字符）。7. 用 memory_search 搜 'abcdefghij' 应命中超长那条。8. 用 memory_save 传 tags 为空数组。9. 用 memory_save 传 category 为空字符串。10. 报告：哪些操作报错、memory_skipped 事件、超长 content 是否完整保存。||10-M1,10-M3,10-M5"
)

EXT11_SCENARIOS=(
    "11-S1|EXT-11|正常加载+system prompt 注入|按以下 10 步顺序执行：1. 用 bash 跑 mkdir -p .ion/rules 创建规则目录。2. 用 write 创建 .ion/rules/global-coding.md 内容：开头 frontmatter '---\nglobs: \"**/*\"\n---' 然后正文 '所有代码必须有显式类型标注，禁止 use unwrap()。'。3. 用 write 创建 .ion/rules/rust-style.md 内容：frontmatter 'globs: \"**/*.rs\"' 正文 'Rust 代码必须用 thiserror 定义错误类型，禁止 anyhow 在 lib 里。'。4. 用 write 创建 .ion/rules/python-style.md 内容：frontmatter 'globs: \"**/*.py\"' 正文 'Python 代码用 type hints，必须 docstring。'。5. 用 extension_rpc rules-engine method='list' 列出所有加载的 rules，应看到 3 个。6. 用 extension_rpc rules-engine method='match' params={file:'src/main.rs'} 应匹配 rust-style（不匹配 python）。7. 用 extension_rpc rules-engine method='match' params={file:'scripts/run.py'} 应匹配 python-style。8. 用 bash 跑 cat .ion/rules/global-coding.md 确认文件内容。9. 用 read 读 .ion/rules/rust-style.md 触发 after_tool_call（应追加 rust rule 到 tool result）。10. 报告：global rule 在 system prompt 里（<project_rules>），path-specific rule 在 read 工具结果里（📌 [project rules]）。|setup_rules_basic|11-M1,11-M2,11-M3,11-M4"
    "11-S2|EXT-11|frontmatter 格式边界|按以下 10 步顺序执行：1. 用 write 创建 .ion/rules/comma-globs.md frontmatter 'globs: \"**/*.rs, **/*.toml\"'（逗号分隔）。2. 用 write 创建 .ion/rules/array-globs.md frontmatter 是 YAML inline array 'globs: [\"**/*.rs\", \"**/*.json\"]'。3. 用 write 创建 .ion/rules/block-globs.md frontmatter 是 YAML block array（globs: 后面换行 - \"**/*.ts\"）。4. 用 write 创建 .ion/rules/no-frontmatter.md 直接写正文（无 ---）。5. 用 write 创建 .ion/rules/empty-body.md frontmatter 'globs: \"**/*\"' 但正文空白（应被忽略）。6. 用 extension_rpc rules-engine method='list' 看哪些被加载。7. 用 extension_rpc rules-engine method='match' params={file:'Cargo.toml'} 看是否匹配 comma-globs。8. 用 extension_rpc rules-engine method='match' params={file:'tsconfig.json'} 看是否匹配 array-globs 或 block-globs。9. 用 extension_rpc rules-engine method='match' params={file:'README.md'} 看 no-frontmatter.md 是否全局匹配。10. 报告：每种 frontmatter 格式的解析结果、empty-body 是否被跳过。|setup_rules_basic|11-M1,11-M2,11-M3,11-M5"
    "11-S3|EXT-11|错误：路径穿越+无效 glob|按以下 10 步顺序执行：1. 用 extension_rpc rules-engine method='match' params={file:'../../../etc/passwd'} 测路径穿越（rules 匹配不应越权）。2. 用 write 创建 .ion/rules/invalid-glob.md frontmatter 'globs: \"[invalid\"'（未闭合字符类）。3. 用 extension_rpc rules-engine method='match' params={file:'any.txt'} 看无效 glob 是否优雅处理。4. 用 extension_rpc rules-engine method='list' 确认 invalid-globs.md 被加载（loading 不应失败）。5. 用 extension_rpc rules-engine method='match' params={file:''} 空字符串（应返回空 matches）。6. 用 extension_rpc rules-engine method='match' params={} 缺 file 参数（应返回 file='' empty matches）。7. 用 extension_rpc rules-engine method='unknown' 调不存在的方法（应报 'method not found'）。8. 用 bash 跑 rm .ion/rules/invalid-glob.md 清理。9. 用 extension_rpc rules-engine method='list' 确认 invalid-globs 已不在列表。10. 报告：glob 解析鲁棒性、错误消息格式、热加载行为。|setup_rules_basic|11-M1,11-M4,11-M5"
)

EXT12_SCENARIOS=(
    "12-S1|EXT-12|富技术会话触发 skill distillation|按以下 10 步顺序执行一个真实有内容的会话：1. 用 bash 跑 mkdir -p /tmp/learn_demo && cd /tmp/learn_demo 创建工作目录。2. 用 write 创建 /tmp/learn_demo/parser.rs 内容是一个简单的 Rust parser：pub fn parse(s: &str) -> Vec<&str> { s.split_whitespace().collect() }。3. 用 bash 跑 cd /tmp/learn_demo && rustc parser.rs 2>&1 | head -5 看编译。4. 用 write 在 /tmp/learn_demo/parser.rs 末尾加 #[cfg(test)] mod tests { use super::*; #[test] fn test_parse() { assert_eq!(parse(\"a b c\").len(), 3); } }。5. 用 bash 跑 cd /tmp/learn_demo && rustc --test parser.rs 2>&1 | tail -3 跑测试。6. 用 read 读 /tmp/learn_demo/parser.rs 看最终内容。7. 用 bash 跑 grep -n 'fn ' /tmp/learn_demo/parser.rs 列出所有函数。8. 用 write 创建 /tmp/learn_demo/README.md 描述这个 parser 用法。9. 用 bash 跑 wc -l /tmp/learn_demo/parser.rs 看代码量。10. 报告这个会话结束时 on_session_shutdown 会触发：会话 > 300 字、有 write 操作、有技术内容（fn/test/use），满足 should_extract + should_distill_skill 条件。||12-M1,12-M2,12-M3,12-M4"
    "12-S2|EXT-12|短会话被跳过（greeting-only）|按以下 10 步顺序执行一个内容极少的会话：1. 用 bash 跑 echo 'ok'。2. 用 bash 跑 echo 'thanks'。3. 用 bash 跑 echo 'done'。4. 用 bash 跑 echo '好的'。5. 用 bash 跑 echo 'test'。6. 用 bash 跑 echo 'ping'。7. 用 bash 跑 echo 'pong'。8. 用 bash 跑 echo 'hi'。9. 用 bash 跑 echo 'hello'。10. 报告：这个会话所有消息都是 SKIP_WORDS，内容 < 300 字符，无技术关键词（无 fn/use/struct/[code-block]），should_extract 应返回 false，on_session_shutdown 应跳过 skill distillation，distill 返回 None。||12-M1,12-M3"
    "12-S3|EXT-12|secret redaction + 边界|按以下 10 步顺序执行验证 secret 处理：1. 用 memory_save 保存 content='my API key is sk-1234567890abcdef1234567890abcdef don'\\''t share it' category='secrets' tags=['api','key']（含疑似 API key）。2. 用 bash 跑 echo 'ghp_abcdefghijklmnopqrstuvwxyz123456' 故意把 GitHub PAT 写到 stdout。3. 用 bash 跑 echo 'password=SuperSecret123!' 输出密码。4. 用 write 创建 /tmp/secret_test.txt 内容含 'token: eyJhbGc...' JWT 模式。5. 用 bash 跑 cat /tmp/secret_test.txt 读回。6. 用 bash 跑 echo 'AKIAABCDEFGHIJKLMNOPQRSTUVWXYZ' AWS key 模式。7. 用 bash 跑 echo 'normal log line no secret'。8. 用 bash 跑 echo 'another ok message'。9. 用 bash 跑 echo 'final message'。10. 报告：LearningExtension 的 secret_detector 在 on_session_shutdown 调 LLM 前应 redact 这些（akia/ghp_/sk-/password=/eyJ 都会被替换），secret_count > 0；但 should_extract 取决于是否有技术内容（这个会话 echo 太短可能被跳过）。||12-M1,12-M2,12-M4,12-M5"
)

EXT13_SCENARIOS=(
    "13-S1|EXT-13|规则拦截 + 放行正常流程|按以下 10 步顺序执行：1. 用 extension_rpc 调 permission 的 add_rule：subject=command.run，pattern=forbidden_*，decision=deny，scope=session，添加一条会话级拒绝规则。2. 用 extension_rpc 调 permission 的 list_rules 确认规则已加入（应看到刚才那条，provider=extension_rpc）。3. 用 bash 执行命令 forbidden_command_test（应该被 before_tool_call 拦截，看到错误信息含 denied by extension rule）。4. 用 bash 执行 echo allowed test（不在 pattern 范围，应放行）。5. 用 extension_rpc 调 permission 的 list_rules 再次确认规则还在。6. 用 extension_rpc 调 permission 的 add_rule 再加一条：subject=command.run，pattern=echo *，decision=allow，scope=session。7. 用 bash 执行 echo allow test（应放行）。8. 用 extension_rpc 调 permission 的 list_rules 看到两条规则。9. 用 bash 执行 forbidden_other（应再次被拦）。10. 报告：拦截了几次、放行了几次、规则总数。||13-M1,13-M2,13-M3,13-M5"
    "13-S2|EXT-13|stored decision 持久化 + 撤销|按以下 10 步顺序执行：1. 用 extension_rpc 调 permission 的 store_decision：subject=command.run，pattern=git status，decision=allow，scope=session，存一条 stored 决策（应返回 perm_stored_ 前缀的 id）。2. 用 extension_rpc 调 permission 的 list_stored 确认有 1 条 source=stored 的决策。3. 用 bash 执行 git status（应被 stored decision 自动放行）。4. 用 extension_rpc 调 permission 的 store_decision 再存一条：subject=file.read，pattern=*.env，decision=deny，scope=session。5. 用 read 读 /tmp/.env（应被拦，看到 denied by extension rule）。6. 用 extension_rpc 调 permission 的 list_stored 看到 2 条 stored。7. 取第 4 步 deny 决策的 id，用 extension_rpc 调 permission 的 remove_stored 删除它。8. 用 extension_rpc 调 permission 的 list_stored 确认只剩 1 条。9. 用 read 读 /tmp/.env（删了 deny 规则后应放行或返回正常错误，不应再是 Permission denied）。10. 报告：stored decision 几条、删除后剩几条、拦截行为。||13-M1,13-M4,13-M5"
    "13-S3|EXT-13|边界 + 错误处理|按以下 10 步顺序执行：1. 用 extension_rpc 调 permission 的 add_rule 传非法 decision=maybe（应返回错误 decision must be allow or deny）。2. 用 extension_rpc 调 permission 的 add_rule 传非法 scope=global（应返回错误 scope must be session or project）。3. 用 extension_rpc 调 permission 的 add_rule 传空 pattern（应成功添加通配规则）。4. 用 extension_rpc 调 permission 的 remove_stored 传不存在的 id perm_stored_nope（应返回错误 no stored decision with id）。5. 用 extension_rpc 调 permission 的 remove_stored 不传 id 参数（应返回错误 missing id parameter）。6. 用 extension_rpc 调 permission 的 store_decision 存一条 subject=* 通配 allow 决策 scope=session。7. 用 extension_rpc 调 permission 的 clear_stored 清空所有 stored 决策（应返回 removed 数量）。8. 用 extension_rpc 调 permission 的 list_stored 确认清空后为 0。9. 用 extension_rpc 调 permission 的 reload 触发热重载（应返回 reloaded 消息含规则计数）。10. 报告：哪些操作报错、哪些成功、错误信息格式。||13-M1,13-M6"
)

EXT14_SCENARIOS=(
    "14-S1|EXT-14|agent 改文件触发 pending + 审批事件|按以下 10 步顺序执行（在当前目录写真实文件）：1. 用 write 创建 baseline_file.txt 写入 initial content。2. 用 write 创建 another_file.txt 写入 another。3. 用 bash 跑 ls -la 看两个文件。4. 用 write 覆盖 baseline_file.txt 写入 modified v1（触发 snapshot + 让该文件进入 pending 状态）。5. 用 write 再次覆盖 baseline_file.txt 写入 modified v2（继续累积 diff）。6. 用 bash 跑 cat baseline_file.txt 确认当前内容。7. 用 read 读 baseline_file.txt 确认。8. 用 write 创建 third_file.txt（多一个 pending 文件）。9. 用 bash 跑 ls -la 看三个文件。10. 报告：改了哪几个文件、on_gate_check 应推 ApprovalRequest 事件含 pending 文件列表、列出 pending 路径。||14-M1,14-M2,14-M3,14-M5"
    "14-S2|EXT-14|多轮编辑 + re-approval 重置|按以下 10 步顺序执行：1. 用 write 创建 reapply_a.txt 写入 v0。2. 用 write 覆盖 reapply_a.txt 写入 v1。3. 用 bash 跑 cat reapply_a.txt 确认 v1。4. 用 write 覆盖 reapply_a.txt 写入 v2（再次修改触发 on_turn_end 的 check_re_approval，应推 ApprovalReset 事件）。5. 用 write 创建 reapply_b.txt。6. 用 write 覆盖 reapply_b.txt 写入 changed。7. 用 bash 跑 ls -la reapply_star.txt。8. 用 write 再次覆盖 reapply_a.txt 写入 v3（连续改动，diff baseline 应锚定在 approved 位置）。9. 用 read 读 reapply_a.txt 看 v3。10. 报告：re-approval 重置行为、diff baseline 锚定、ApprovalReset 事件触发次数。||14-M1,14-M2,14-M4"
    "14-S3|EXT-14|新增 + 删除文件 pending 边界|按以下 10 步顺序执行：1. 用 write 创建 stable_ref.txt 写入 baseline。2. 用 write 创建 del_target.txt 写入 to be deleted。3. 用 bash 跑 ls del_target.txt stable_ref.txt。4. 用 bash 跑 rm del_target.txt 删除该文件（on_turn_end 检测 deleted → 进入 pending）。5. 用 write 覆盖 stable_ref.txt 写入 modified。6. 用 write 创建 new_added.txt 写入 brand new。7. 用 bash 跑 ls -la 看当前文件清单。8. 用 read 读 stable_ref.txt 确认 modified。9. 用 bash 跑 ls new_added.txt 确认存在。10. 报告：pending 列表应含 added（new_added.txt）/modified（stable_ref.txt）/deleted（del_target.txt）三种状态。||14-M1,14-M2,14-M5"
)

EXT15_SCENARIOS=(
    "15-S1|EXT-15|read 注入 + write 触发 stale 折叠|按以下 10 步顺序执行（在当前目录写真实文件）：1. 用 write 创建 ctx_file.txt 写入 first version long content here。2. 用 read 读 ctx_file.txt（after_tool_call 记录 read，应进入索引）。3. 用 read 再读一次 ctx_file.txt（多次 read 都标 Current）。4. 用 write 覆盖 ctx_file.txt 写入 second version（record_write 把之前的 read 标 Stale）。5. 用 read 读 ctx_file.txt 看新内容（新 read 是 Current，旧 read 仍 Stale）。6. 用 write 再次覆盖 ctx_file.txt 写入 third version（所有之前的 read 都成 Stale）。7. 用 read 读 ctx_file.txt 看最新内容。8. 用 bash 跑 cat ctx_file.txt 交叉验证。9. 用 extension_rpc 调 context-index tree 看 ctx_file.txt 的状态（应是 current 最新的 + 历史 stale）。10. 报告：read 了几次、write 了几次、stale 折叠后旧 read 应变成 [ContextIndex: ...] 占位符。||15-M1,15-M2,15-M3,15-M4"
    "15-S2|EXT-15|grep 解析路径 + 多文件索引|按以下 10 步顺序执行：1. 用 write 创建 g_a.rs 写入 pub fn alpha() {}。2. 用 write 创建 g_b.rs 写入 pub fn beta() {}。3. 用 write 创建 g_c.rs 写入 pub fn gamma() {}。4. 用 grep 在当前目录搜 fn 关键字（after_tool_call 解析 ripgrep 输出，把 g_a.rs/g_b.rs/g_c.rs 三个路径加入索引）。5. 用 read 读 g_a.rs（再 record 一次 read）。6. 用 write 覆盖 g_b.rs 写入 pub fn beta_v2() {}（g_b.rs 的 grep read + 后续 read 都变 Stale）。7. 用 grep 再搜一次 fn 看 g_b.rs 新内容命中。8. 用 read 读 g_b.rs 看 beta_v2。9. 用 extension_rpc 调 context-index tree 应看到 3 个文件，g_b.rs 有 stale 痕迹。10. 报告：grep 解析了几个路径、哪些文件 stale、context_index tree 输出。||15-M1,15-M3,15-M5"
    "15-S3|EXT-15|边界：空索引 + 无操作文件|按以下 10 步顺序执行：1. 用 write 创建 untouched.txt 写入 never read by agent。2. 用 write 创建 only_write.txt 写入 only written。3. 用 bash 跑 ls -la（bash/find 标 untracked，不在索引里）。4. 用 read 读 only_write.txt（进入索引 Current）。5. 用 write 覆盖 only_write.txt（旧 read Stale）。6. 用 read 读 only_write.txt 看 new content。7. 用 extension_rpc 调 context-index ranges 传 path=only_write.txt 看历史 read 列表。8. 用 extension_rpc 调 context-index ranges 传 path=nonexistent.rs 看返回空 reads。9. 用 extension_rpc 调 context-index tree 确认 untouched.txt 不在索引（因为没 read 过）。10. 报告：哪些文件被索引、哪些 stale、untracked 来源列表。||15-M1,15-M5"
)

EXT16_SCENARIOS=(
    "16-S1|EXT-16|多轮工具调用触发 index 更新|按以下 10 步顺序执行：1. 用 bash 执行 echo step 1（第一轮工具调用，内核 increment_turn_stats +1）。2. 用 bash 执行 echo step 2。3. 用 write 创建 sess_idx_log.txt 写入 first。4. 用 read 读 sess_idx_log.txt。5. 用 bash 跑 wc -c sess_idx_log.txt。6. 用 write 覆盖 sess_idx_log.txt 加内容。7. 用 read 读 sess_idx_log.txt 看更新。8. 用 bash 执行 echo step 8。9. 用 bash 执行 echo step 9。10. 报告：执行了多少轮工具调用，session_index 里 turn_count 应累加（内核自动维护，无需 LLM 主动调）。||16-M1,16-M2,16-M3"
    "16-S2|EXT-16|跨 session 索引持久化|按以下 10 步顺序执行：1. 用 bash 跑 ls ~/.ion/agent/sessions.index.json 确认索引文件存在。2. 用 bash 跑 cat ~/.ion/agent/sessions.index.json 看 JSON 结构（应含 sessions 对象）。3. 用 bash 跑 python3 -c 读 sessions.index.json 统计当前 sessions 数量。4. 用 write 创建 marker_a.txt 写入 a。5. 用 write 创建 marker_b.txt 写入 b。6. 用 bash 跑 ls marker_star.txt。7. 用 read 读 marker_a.txt。8. 用 bash 执行 echo verifying index update。9. 用 bash 跑 cat ~/.ion/agent/sessions.index.json | python3 -c 看 updated_at 是否比第 2 步更新。10. 报告：sessions.index.json 大小、session 总数、updated_at 是否刷新。||16-M1,16-M2,16-M4"
    "16-S3|EXT-16|project 字段 + 统计字段|按以下 10 步顺序执行：1. 用 bash 跑 pwd 确认当前工作目录（应被记到 session meta.project）。2. 用 write 创建 proj_check.txt 写入 content。3. 用 read 读 proj_check.txt。4. 用 bash 执行 echo turn 4。5. 用 bash 跑 git rev-parse --abbrev-ref HEAD 看当前分支（应记到 meta.branch）。6. 用 bash 跑 ls -la 看目录。7. 用 write 覆盖 proj_check.txt 写入 v2。8. 用 read 读 proj_check.txt。9. 用 bash 跑 cat ~/.ion/agent/sessions.index.json 看 project/project_name/branch 字段是否正确。10. 报告：当前 session 的 project/branch/turn_count/token_input 字段值。||16-M1,16-M3,16-M5"
)

EXT17_SCENARIOS=(
    "17-S1|EXT-17|大量 bash 输出触发回收|按以下 10 步顺序执行（刻意产出长输出）：1. 用 bash 执行 yes line | head -300（产出 300 行）。2. 用 bash 执行 yes data | head -300。3. 用 bash 执行 yes more | head -300。4. 用 bash 执行 yes output | head -300。5. 用 bash 执行 yes again | head -300。6. 用 bash 执行 yes final | head -300。7. 用 bash 执行 echo summary。8. 用 bash 执行 echo more summary。9. 用 bash 执行 echo final step。10. 报告：执行了多少轮 bash、on_context 是否对早期长输出做 reclaim（应出现 [reclaimed: bash output was N chars] 占位符）。||17-M1,17-M2,17-M3"
    "17-S2|EXT-17|read 大文件 + write 触发 stale 回收|按以下 10 步顺序执行：1. 用 write 创建 big_read.txt 写入重复 100 次的 long content line（造一个大文件）。2. 用 read 读 big_read.txt（产成长 ToolResult）。3. 用 write 覆盖 big_read.txt 写入 short（旧 read 变 stale，可被 reclaim 即使在 heat window 内）。4. 用 read 读 big_read.txt 看 short。5. 用 bash 执行 yes noise | head -200。6. 用 bash 执行 yes more noise | head -200。7. 用 bash 执行 yes again | head -200。8. 用 bash 执行 echo step 8。9. 用 bash 执行 echo step 9。10. 报告：旧 read 在 write 后变 stale，on_context 应把它折叠成 [reclaimed: ... (stale)]。||17-M1,17-M4,17-M5"
    "17-S3|EXT-17|优先级 tier 顺序验证|按以下 10 步顺序执行：1. 用 write 创建 tier_read.txt 写入 read me long content。2. 用 read 读 tier_read.txt。3. 用 grep 在当前目录搜 tier 关键字（grep 是 tier2）。4. 用 bash 执行 yes bash output | head -200（bash 是 tier1，最低价值先回收）。5. 用 bash 执行 yes more bash | head -200。6. 用 bash 执行 yes even more | head -200。7. 用 read 读 tier_read.txt 再读一次。8. 用 bash 执行 echo priority check。9. 用 bash 执行 echo final。10. 报告：reclaim 优先级应是 bash(tier1) > grep(tier2) > read(tier3)，bash 先被折叠。||17-M1,17-M2,17-M3"
)

EXT18_SCENARIOS=(
    "18-S1|EXT-18|read 记录 + 外部修改触发 stale|按以下 10 步顺序执行：1. 用 write 创建 ftg_test.txt 写入 original content。2. 用 read 读 ftg_test.txt（after_tool_call 记录 mtime+size 到 snapshots）。3. 用 extension_rpc 调 file-time-guard status 看 tracked_files 应 ≥ 1。4. 用 extension_rpc 调 file-time-guard check 传 path=ftg_test.txt，应 stale=false（刚读过没改）。5. 用 bash 执行 sleep 2（确保 mtime 改变）。6. 用 bash 执行 bash -c 'echo externally modified > ftg_test.txt'（模拟外部 IDE 改文件，绕过 agent 的 record）。7. 用 extension_rpc 调 file-time-guard check 传 path=ftg_test.txt，应 stale=true 含 mtime 或 size changed 原因。8. 用 read 重新读 ftg_test.txt（record 更新 snapshot）。9. 用 extension_rpc 调 file-time-guard check 传 path=ftg_test.txt，应 stale=false（重新读后 fresh）。10. 报告：tracked_files 数量、stale 检测的 reason 内容。||18-M1,18-M2,18-M3,18-M4"
    "18-S2|EXT-18|write 在 stale 文件上的 Warn 行为|按以下 10 步顺序执行：1. 用 write 创建 ftg_warn.txt 写入 v1。2. 用 read 读 ftg_warn.txt（record snapshot）。3. 用 bash 执行 sleep 2。4. 用 bash 执行 bash -c 'echo externally changed v2 > ftg_warn.txt'（外部改）。5. 用 extension_rpc 调 file-time-guard check 传 path=ftg_warn.txt 确认 stale=true。6. 用 write 覆盖 ftg_warn.txt 写入 agent v3（默认 Warn 模式：应放行，但 stderr 有 WARNING）。7. 用 read 读 ftg_warn.txt 看最终内容（应是 agent v3）。8. 用 extension_rpc 调 file-time-guard status 看 tracked_files。9. 用 extension_rpc 调 file-time-guard check 传 path=ftg_warn.txt 应 stale=false（write 后 mtime 已更新但 record 没刷，需 read 才刷——验这个语义）。10. 报告：Warn 模式下 write 是否放行、stale 检测前后对比。||18-M1,18-M2,18-M5"
    "18-S3|EXT-18|ignore paths + 边界|按以下 10 步顺序执行：1. 用 bash 执行 mkdir -p target。2. 用 bash 在 target 下创建 dummy.rs 写入 a。3. 用 read 读 target/dummy.rs（record snapshot）。4. 用 bash 执行 sleep 2。5. 用 bash 执行 bash -c 'echo longer content > target/dummy.rs'（外部改 target 下文件）。6. 用 extension_rpc 调 file-time-guard check 传 path=target/dummy.rs，应 stale=false（target/ 在 ignore_paths 里，永远不报 stale）。7. 用 read 读 target/dummy.rs 看新内容。8. 用 extension_rpc 调 file-time-guard check 传 path=/nonexistent/file.txt（文件不存在，应 fail-open stale=false）。9. 用 extension_rpc 调 file-time-guard status 看 mode 应是 warn。10. 报告：ignore 路径行为、不存在文件 fail-open、当前 mode。||18-M1,18-M4,18-M5"
)

EXT19_SCENARIOS=(
    "19-S1|EXT-19|完整 plan lifecycle：enter → add → list → exit → 执行|按以下 10 步顺序执行：1. 用 plan_enter 进入 plan mode，参数 plan_path='/tmp/plan_s1.md'。2. 用 read 读当前目录的 Cargo.toml（plan mode 允许 read）。3. 用 plan_add 加步骤 '搭建项目骨架'。4. 用 plan_add 加步骤 '写 src/lib.rs'。5. 用 plan_add 加步骤 '跑 cargo build 验证'。6. 用 plan_list 看当前所有步骤。7. 用 plan_done 标记步骤 0 为完成。8. 用 plan_list 确认步骤 0 显示 [x]。9. 用 plan_exit 退出 plan mode（应该把 plan 写到 /tmp/plan_s1.md）。10. 报告：plan 文件内容、各步骤状态、退出后 write/bash 是否恢复可用。||19-M1,19-M2,19-M3,19-M4,19-M7"
    "19-S2|EXT-19|strict_mode 审批门 + plan_approve|按以下 10 步顺序执行：1. 用 plan_enter 进入 plan mode，参数 plan_path='/tmp/plan_s2.md'，strict_mode=true。2. 用 plan_add 加步骤 '分析需求'。3. 用 plan_add 加步骤 '设计 API'。4. 用 plan_add 加步骤 '写测试'。5. 用 plan_approve 审批步骤 0。6. 用 plan_approve 审批步骤 1。7. 用 plan_approve 审批步骤 2。8. 用 plan_list 确认所有步骤都 approved。9. 用 plan_exit 退出（strict_mode 要求全部 approved，现在应该成功）。10. 报告：strict_mode 是否生效、未审批时 plan_exit 是否被拒、审批后是否放行。||19-M1,19-M2,19-M5,19-M6"
    "19-S3|EXT-19|plan mode 工具隔离 + 持久化|按以下 10 步顺序执行：1. 用 plan_enter 进入 plan mode，plan_path='/tmp/plan_s3.md'。2. 用 read 读 Cargo.toml（plan mode 允许）。3. 尝试用 bash 跑 echo hello（plan mode 应该拒绝 bash）。4. 尝试用 write 写文件（plan mode 应该拒绝 write）。5. 用 plan_add 加 3 条步骤。6. 用 plan_list 确认步骤数。7. 用 plan_done 标记步骤 0。8. 用 plan_exit 退出。9. 用 read 读 /tmp/plan_s3.md 看 plan 是否持久化。10. 报告：plan mode 下哪些工具被禁用、持久化文件内容、状态转换链路。||19-M1,19-M2,19-M3,19-M4"
)

EXT20_SCENARIOS=(
    "20-S1|EXT-20|read 重复触发 loop warning|按以下 10 步顺序执行：1. 用 write 创建 /tmp/loop_s1.txt 写入 'loop target'。2. 用 read 读 /tmp/loop_s1.txt（第 1 次）。3. 用 read 读 /tmp/loop_s1.txt（第 2 次，完全相同的 file_path）。4. 用 read 读 /tmp/loop_s1.txt（第 3 次，应该触发 WARN_THRESHOLD=3 警告）。5. 用 read 读 /tmp/loop_s1.txt（第 4 次）。6. 用 read 读 /tmp/loop_s1.txt（第 5 次，应该触发 ABORT_THRESHOLD=5 被强制中断）。7. 如果上一步被中断，改用 bash 跑 echo recovered。8. 用 read 读 /tmp/loop_s1.txt 但带不同 offset（不同签名，应该不触发）。9. 用 bash 跑 ls /tmp/loop_s1.txt 确认文件还在。10. 报告：第几次 read 触发警告、第几次被 abort、不同签名是否豁免、abort 错误信息。||20-M1,20-M2,20-M3,20-M5"
    "20-S2|EXT-20|bash echo 归一化检测|按以下 10 步顺序执行：1. 用 bash 执行 echo hello（第 1 次）。2. 用 bash 执行 echo world（第 2 次，内容不同但 echo 被归一化成相同签名）。3. 用 bash 执行 echo foo（第 3 次，应该触发警告）。4. 用 bash 执行 printf test（printf 也归一化成 echo 签名，第 4 次）。5. 用 bash 执行 echo bar（第 5 次，应该触发 ABORT）。6. 如果被 abort，改用 bash 跑 pwd（不同签名，应该成功）。7. 用 bash 跑 ls（不同签名）。8. 用 bash 跑 date（不同签名）。9. 用 bash 跑 whoami（不同签名）。10. 报告：echo/printf 是否被归一化、归一化后是否仍计数、abort 阈值是否准确。||20-M1,20-M2,20-M3,20-M4"
    "20-S3|EXT-20|豁免工具不触发 + UTF-8 安全|按以下 10 步顺序执行：1. 用 memory_save 保存一条 memory（内容含中文字符 '循环检测测试'）。2. 用 memory_search 搜 '循环'（第 1 次，memory_search 在 LOOP_EXEMPT_TOOLS 里）。3. 用 memory_search 搜 '循环'（第 2 次，应该豁免不警告）。4. 用 memory_search 搜 '循环'（第 3 次，豁免）。5. 用 memory_search 搜 '循环'（第 4 次，仍豁免）。6. 用 memory_search 搜 '循环'（第 5 次，仍豁免，证明豁免生效）。7. 用 plan_list（也在豁免列表）连调 3 次。8. 用 bash 跑一个含多字节中文字符的长命令（如 python3 -c 后接 60 个中文字符），验证不 panic。9. 用 read 读一个含 emoji 的文件。10. 报告：豁免工具是否真的不计数、UTF-8 多字节命令是否正常处理无 panic。||20-M1,20-M6"
)


EXT22_SCENARIOS=(
    "22-S1|EXT-22|首轮后标题生成 + 索引更新|按以下 10 步顺序执行：1. 用 bash 跑 echo 'session title test start'（这是首轮，on_turn_end 后应触发标题生成）。2. 用 bash 跑 pwd。3. 用 bash 跑 cat ~/.ion/agent/session-titles.json 看是否生成了标题。4. 用 bash 跑 ion sessions 看会话列表里是否有标题。5. 用 bash 跑 ls -la ~/.ion/sessions/ 找最新 session 文件。6. 用 bash 跑 head -5 <最新 session.jsonl> 看是否有 session_name entry。7. 用 write 创建 /tmp/title_s1.txt 写当前时间戳。8. 用 bash 跑 echo 'second turn content'（这轮不应再触发标题生成，done flag 已置位）。9. 用 read 读 /tmp/title_s1.txt。10. 报告：标题内容、标题长度（应 ≤50-80 字符）、session-titles.json 结构、是否只生成一次。||22-M1,22-M2,22-M3,22-M4"
    "22-S2|EXT-22|启发式 fallback + 中文 prompt|按以下 10 步顺序执行（关掉网络或用无效 fast tier 让 LLM 失败，触发启发式）：1. 用 bash 跑 export ION_FAST_TIER_BYPASS=1（或直接发中文 prompt 让 LLM 可能超时）。2. 用 bash 跑 echo '修复解析器的 bug'（中文 prompt，启发式 fallback 会用首句切分）。3. 用 bash 跑 cat ~/.ion/agent/session-titles.json 看标题（应该是 '修复解析器的 bug' 而非 'Untitled'）。4. 用 bash 跑 ion sessions 看标题显示。5. 用 bash 跑 echo '按以下 10 步顺序执行：1. 做 A'（这种 prompt 启发式应该在冒号处切分）。6. 用 bash 跑 cat ~/.ion/agent/session-titles.json 看新标题（应该是 '按以下 10 步顺序执行'）。7. 用 write 创建 /tmp/title_s2.txt。8. 用 bash 跑 ls /tmp/title_s2.txt。9. 用 read 读 /tmp/title_s2.txt。10. 报告：启发式切分规则、中文冒号是否正确处理、空 prompt 是否回退 Untitled。||22-M1,22-M2,22-M5"
    "22-S3|EXT-22|长 prompt 截断 + 多行 prompt|按以下 10 步顺序执行：1. 用 bash 跑一个超长 prompt（重复 'A' 字符 200 次）作为首轮消息（启发式应截断到 77 字符 + '...'）。2. 用 bash 跑 cat ~/.ion/agent/session-titles.json 看标题长度（应 ≤80 字符）。3. 用 bash 跑 ion sessions 确认标题不溢出。4. 用 bash 跑一个多行 prompt（第一行 'First line' 第二行 'Second line'）。5. 用 bash 跑 cat ~/.ion/agent/session-titles.json 看标题（应该只取第一行）。6. 用 write 创建 /tmp/title_s3.txt。7. 用 bash 跑 echo step 7。8. 用 bash 跑 echo step 8。9. 用 bash 跑 echo step 9。10. 报告：长 prompt 截断行为、多行 prompt 取首行、标题长度上限。||22-M1,22-M2,22-M5"
)

EXT23_SCENARIOS=(
    "23-S1|EXT-23|gate 通过路径（gate_command 输出 PASS）|按以下 10 步顺序执行（agent .md 已配 workflow gate_command='test -f /tmp/wf_gate_s1.pass && echo PASS || echo FAIL'）：1. 用 bash 跑 touch /tmp/wf_gate_s1.pass（让 gate_command 输出 PASS）。2. 用 bash 跑 echo step 2。3. 用 write 创建 /tmp/wf_s1.txt 写 'workflow gate test'。4. 用 bash 跑 cat /tmp/wf_s1.txt。5. 用 bash 跑 echo step 5。6. 用 bash 跑 ls /tmp/wf_gate_s1.pass。7. 用 bash 跑 echo step 7。8. 用 read 读 /tmp/wf_s1.txt。9. 用 bash 跑 echo step 9。10. 报告完成（此时 LLM Stop → gate 跑 → 输出 PASS → Allow 放行，不应触发 RetryWith）。|setup_workflow_pass|23-M1,23-M2,23-M5"
    "23-S2|EXT-23|gate 失败触发 RetryWith 强制继续|按以下 10 步顺序执行（agent .md 配 gate_command='test -f /tmp/wf_gate_s2.pass && echo PASS || echo FAIL'，但文件不存在，gate 应失败）：1. 用 bash 跑 rm -f /tmp/wf_gate_s2.pass（确保 gate 会失败）。2. 用 bash 跑 echo step 2。3. 用 write 创建 /tmp/wf_s2.txt 写 'incomplete'。4. 用 bash 跑 echo step 4。5. 用 bash 跑 echo step 5。6. 报告 'done'（此时 LLM Stop → gate 失败 → 注入 'GATE CHECK FAILED' 强制继续）。7. 被强制继续后，用 bash 跑 touch /tmp/wf_gate_s2.pass（修复 gate）。8. 用 bash 跑 echo step 8。9. 用 read 读 /tmp/wf_s2.txt。10. 再次报告完成（这次 gate 通过 → Allow）。|setup_workflow_fail|23-M1,23-M3,23-M4,23-M5"
    "23-S3|EXT-23|max_retries 耗尽放行|按以下 10 步顺序执行（agent .md 配 max_retries=2 且 gate_command 永远输出 FAIL）：1. 用 bash 跑 echo step 1。2. 用 write 创建 /tmp/wf_s3.txt。3. 用 bash 跑 echo step 3。4. 用 bash 跑 echo step 4。5. 报告完成（gate 失败 → RetryWith 第 1 次）。6. 被强制继续，用 bash 跑 echo step 6。7. 报告完成（gate 失败 → RetryWith 第 2 次）。8. 被强制继续，用 bash 跑 echo step 8。9. 报告完成（第 3 次 gate 检查，已超 max_retries=2 → Allow 放行避免无限循环）。10. 报告：gate 失败次数、RetryWith 注入内容、max_retries 耗尽后的行为。|setup_workflow_exhaust|23-M1,23-M3,23-M6"
)

EXT24_SCENARIOS=(
    "24-S1|EXT-24|基础流式会话 + 事件可见|按以下 10 步顺序执行：1. 用 bash 跑 echo 'streaming test step 1'。2. 用 bash 跑 pwd。3. 用 write 创建 /tmp/stream_s1.txt 写 'streaming output'。4. 用 bash 跑 cat /tmp/stream_s1.txt。5. 用 bash 跑 echo step 5。6. 用 read 读 /tmp/stream_s1.txt。7. 用 bash 跑 echo step 7。8. 用 bash 跑 ls -la /tmp/stream_s1.txt。9. 用 bash 跑 echo step 9。10. 报告完整工作流（观察流式输出：每个 bash/write/read 应触发 tool_execution_start 事件，assistant 回复应有 text_delta 流）。||24-M1,24-M2,24-M3"
    "24-S2|EXT-24|长输出流式 + 工具调用 delta|按以下 10 步顺序执行：1. 用 bash 跑 seq 1 100（长输出，测试 text_delta 是否分段流）。2. 用 bash 跑 cat /tmp/stream_s1.txt || echo 'no file'。3. 用 write 创建 /tmp/stream_s2.txt 写一段 500 字符的内容。4. 用 bash 跑 wc -c /tmp/stream_s2.txt。5. 用 bash 跑 yes | head -50（长输出）。6. 用 read 读 /tmp/stream_s2.txt。7. 用 bash 跑 grep -c . /tmp/stream_s2.txt。8. 用 bash 跑 echo step 8。9. 用 bash 跑 echo step 9。10. 报告：长输出是否分段流、tool_call_delta 是否触发、流式事件频率。||24-M1,24-M2,24-M4"
    "24-S3|EXT-24|多工具混合 + agent 生命周期事件|按以下 10 步顺序执行：1. 用 bash 跑 echo 'agent lifecycle test'。2. 用 write 创建 /tmp/stream_s3_a.txt。3. 用 write 创建 /tmp/stream_s3_b.txt。4. 用 bash 跑 ls /tmp/stream_s3_*.txt。5. 用 read 读两个文件。6. 用 bash 跑 wc -l /tmp/stream_s3_*.txt。7. 用 bash 跑 echo processing。8. 用 write 覆盖 /tmp/stream_s3_a.txt 加内容。9. 用 bash 跑 cat /tmp/stream_s3_a.txt。10. 报告完整工作流（每个 user turn 应有 agent_start → 多个 message_start/end → tool_execution_start → agent_end 事件序列）。||24-M1,24-M2,24-M5"
)

# ── 所有场景集合 ──
ALL_SCENARIOS=(
    "${EXT02_SCENARIOS[@]}"
    "${EXT03_SCENARIOS[@]}"
    "${EXT04_SCENARIOS[@]}"
    "${EXT05_SCENARIOS[@]}"
    "${EXT06_SCENARIOS[@]}"
    "${EXT07_SCENARIOS[@]}"
    "${EXT08_SCENARIOS[@]}"
    "${EXT09_SCENARIOS[@]}"
    "${EXT10_SCENARIOS[@]}"
    "${EXT11_SCENARIOS[@]}"
    "${EXT12_SCENARIOS[@]}"
    "${EXT13_SCENARIOS[@]}"
    "${EXT14_SCENARIOS[@]}"
    "${EXT15_SCENARIOS[@]}"
    "${EXT16_SCENARIOS[@]}"
    "${EXT17_SCENARIOS[@]}"
    "${EXT18_SCENARIOS[@]}"
    "${EXT19_SCENARIOS[@]}"
    "${EXT20_SCENARIOS[@]}"
    "${EXT22_SCENARIOS[@]}"
    "${EXT23_SCENARIOS[@]}"
    "${EXT24_SCENARIOS[@]}"
)

# 按 EXT 过滤
get_scenarios_for_ext() {
    local ext_id="$1"
    case "$ext_id" in
        EXT-02) printf '%s\n' "${EXT02_SCENARIOS[@]}" ;;
        EXT-03) printf '%s\n' "${EXT03_SCENARIOS[@]}" ;;
        EXT-04) printf '%s\n' "${EXT04_SCENARIOS[@]}" ;;
        EXT-05) printf '%s\n' "${EXT05_SCENARIOS[@]}" ;;
        EXT-06) printf '%s\n' "${EXT06_SCENARIOS[@]}" ;;
        EXT-07) printf '%s\n' "${EXT07_SCENARIOS[@]}" ;;
        EXT-08) printf '%s\n' "${EXT08_SCENARIOS[@]}" ;;
        EXT-09) printf '%s\n' "${EXT09_SCENARIOS[@]}" ;;
        EXT-10) printf '%s\n' "${EXT10_SCENARIOS[@]}" ;;
        EXT-11) printf '%s\n' "${EXT11_SCENARIOS[@]}" ;;
        EXT-12) printf '%s\n' "${EXT12_SCENARIOS[@]}" ;;
        EXT-13) printf '%s\n' "${EXT13_SCENARIOS[@]}" ;;
        EXT-14) printf '%s\n' "${EXT14_SCENARIOS[@]}" ;;
        EXT-15) printf '%s\n' "${EXT15_SCENARIOS[@]}" ;;
        EXT-16) printf '%s\n' "${EXT16_SCENARIOS[@]}" ;;
        EXT-17) printf '%s\n' "${EXT17_SCENARIOS[@]}" ;;
        EXT-18) printf '%s\n' "${EXT18_SCENARIOS[@]}" ;;
        EXT-19) printf '%s\n' "${EXT19_SCENARIOS[@]}" ;;
        EXT-20) printf '%s\n' "${EXT20_SCENARIOS[@]}" ;;
        EXT-22) printf '%s\n' "${EXT22_SCENARIOS[@]}" ;;
        EXT-23) printf '%s\n' "${EXT23_SCENARIOS[@]}" ;;
        EXT-24) printf '%s\n' "${EXT24_SCENARIOS[@]}" ;;
        *) printf '%s\n' "${ALL_SCENARIOS[@]}" ;;
    esac
}
