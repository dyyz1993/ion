#!/usr/bin/env bash
# ext_scenarios.sh — 扩展多场景测试清单（可 source）
#
# 每个场景格式：
#   SCENARIO_ID|EXT_ID|NAME|PROMPT|PRE_SETUP|EXPECTED_METRICS
#
# PRE_SETUP：bash 函数名（在跑 prompt 前调用，准备 hooks.json / 文件系统等）
# EXPECTED_METRICS：跑 validate_html.py --ext 时期望全过的指标 ID 列表（逗号分隔）

# ── EXT-02 GlobalMemory 场景 ──
EXT02_SCENARIOS=(
    "02-S1|EXT-02|save+search round-trip|请记住：我喜欢 Rust 语言。然后搜索记忆里关于语言偏好的内容。||02-M1,02-M2,02-M3,02-M4"
    "02-S2|EXT-02|空查询不报错|搜索一个肯定没有的关键字：zzqqxxww。然后告诉我搜到了什么。||02-M1,02-M3,02-M8"
    "02-S3|EXT-02|tags 多关键字检索|请记住这条经验：用 tokio 跑异步任务时要注意生命周期，tags 加上 async/tokio/rust。然后搜 'tokio'。||02-M1,02-M2,02-M3,02-M4"
)

# ── EXT-03 DevServerDetector 场景 ──
EXT03_SCENARIOS=(
    "03-S1|EXT-03|Python http.server|请用 bash 工具 background=true 后台运行命令：python3 -m http.server 8765。然后告诉我检测到了什么 dev server。||03-M2,03-M4,03-M5"
    "03-S2|EXT-03|不同端口|请用 bash background=true 后台运行 python3 -m http.server 3000。然后报告检测到的端口。||03-M2,03-M4,03-M5"
    "03-S3|EXT-03|多 server 并发|用 bash background=true 同时后台启动 python3 -m http.server 8765 和 python3 -m http.server 8766。报告所有检测到的 server。||03-M2,03-M4"
)

# ── EXT-04 FileSnapshot 场景 ──
EXT04_SCENARIOS=(
    "04-S1|EXT-04|单文件创建|请用 write 工具创建 /tmp/snap_test_1.txt 内容为 hello world。然后告诉我 snapshot 状态。||04-M1,04-M4"
    "04-S2|EXT-04|多文件创建|请用 write 工具创建 3 个文件：/tmp/snap_a.txt /tmp/snap_b.txt /tmp/snap_c.txt，内容分别为 a/b/c。||04-M1,04-M4"
    "04-S3|EXT-04|编辑后 diff|请用 write 创建 /tmp/snap_diff.txt 内容 hello。然后再用 write 覆盖为 world。||04-M1,04-M4,04-M5"
)

# ── EXT-05 Lsp 场景 ──
EXT05_SCENARIOS=(
    "05-S1|EXT-05|干净代码无错|请用 write 工具在 /tmp/lsp_test/ 创建一个简单的 Rust lib（Cargo.toml + src/lib.rs），写一个正确的 add 函数。然后调 lsp_check 工具。||05-M1,05-M6"
    "05-S2|EXT-05|捕获类型错误|先创建一个 Rust lib，然后故意写错：let x: i32 = \"string\";。然后调 lsp_check 看错误。||05-M1,05-M3,05-M6"
    "05-S3|EXT-05|修复后清零|先引入错误（同 S2），调 lsp_check 看到错误。然后修复，再调 lsp_check，确认错误减少。||05-M1,05-M4"
)

# ── EXT-06 Hook 场景（每个场景配不同的 hooks.json）──
EXT06_SCENARIOS=(
    "06-S1|EXT-06|PostToolUse command|请用 bash 执行 echo hello-hook。触发 PostToolUse hook。|setup_hook_posttooluse|06-M1,06-M2,06-M8"
    "06-S2|EXT-06|Stop event|请回复 'done' 即可，触发 Stop hook。|setup_hook_stop|06-M1,06-M2"
    "06-S3|EXT-06|SessionStart|直接回复 hi。触发 SessionStart hook。|setup_hook_sessionstart|06-M1,06-M2"
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
            "command": "echo \"[HOOK-PostToolUse] bash called\" >> $ION_PROJECT_DIR/hook_log.txt"
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
            "command": "echo \"[HOOK-Stop] session ending\" >> $ION_PROJECT_DIR/hook_log.txt"
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
            "command": "echo \"[HOOK-SessionStart] reason=$ION_REASON\" >> $ION_PROJECT_DIR/hook_log.txt"
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
