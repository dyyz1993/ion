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
    "03-S2|EXT-03|端口占用 + 失败处理|按以下 10 步顺序执行：1. 用 bash 占用端口 9999（前台启 python3 -m http.server 9999）。2. 用 bash background=true 再启 python3 -m http.server 9999（应该失败）。3. 用 get_background_process 看失败原因。4. 用 bash 杀掉第 1 步前台进程。5. 用 bash background=true 启 python3 -m http.server 9999（现在应该成功）。6. 用 bash background=true 启 python3 -m http.server 8888。7. 用 bash curl http://localhost:9999/ 和 http://localhost:8888/ 验证。8. 用 get_background_process 列出活跃 server。9. 用 kill_process 杀 8888。10. 报告：端口冲突如何检测、错误信息、最终存活 server。||03-M1,03-M2,03-M4"
    "03-S3|EXT-03|多语言 dev server|按以下 10 步顺序执行：1. 用 bash background=true 启 python3 -m http.server 8000。2. 用 bash background=true 启 python3 -m http.server 8001。3. 用 bash background=true 启 python3 -m http.server 8002。4. 用 get_background_process 列出所有 server。5. 用 bash curl http://localhost:8000/ 验证。6. 用 bash curl http://localhost:8001/ 验证。7. 用 bash curl http://localhost:8002/ 验证。8. 用 kill_process 杀 8001。9. 用 get_background_process 确认 8001 死了，8000/8002 活着。10. 报告：3 个 server 同时检测能力、并发性能、清理策略。||03-M1,03-M2,03-M4"
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

# ── 所有场景集合 ──
ALL_SCENARIOS=(
    "${EXT02_SCENARIOS[@]}"
    "${EXT03_SCENARIOS[@]}"
    "${EXT04_SCENARIOS[@]}"
    "${EXT05_SCENARIOS[@]}"
    "${EXT06_SCENARIOS[@]}"
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
        *) printf '%s\n' "${ALL_SCENARIOS[@]}" ;;
    esac
}
