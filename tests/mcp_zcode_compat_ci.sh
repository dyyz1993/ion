#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# MCP zcode 配置兼容 CI
#
# 验证 ion 能 100% 兼容 zcode 的 MCP 配置格式：
#   Group A:  嵌套格式 mcp.servers 加载（disabled，快速确定性）
#   Group B:  enabled/disabled 字段兼容
#   Group C:  嵌套 + 平铺混用
#   Group D:  每个 server CLI 实调工具（核心验证，真实连接）
#
# 用法：bash tests/mcp_zcode_compat_ci.sh [D]
#   不带参数 → 跑 Group A/B/C（快速，不连真实 server）
#   带 D     → 额外跑 Group D（真实连接 4 个 zcode MCP server）
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
RUN_GROUP_D=false
[[ "${1:-}" == "D" ]] && RUN_GROUP_D=true

echo "════════════════════════════════════════════════════"
echo "  MCP zcode 兼容 CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ── 隔离的 TEST_HOME（不影响真实 ~/.ion/config.json）──
TEST_HOME="/tmp/ion_mcp_zcode_ci_home_$$"
rm -rf "$TEST_HOME" 2>/dev/null
mkdir -p "$TEST_HOME/.ion"
ln -sf "/Users/xuyingzhou/.rustup" "$TEST_HOME/.rustup" 2>/dev/null
ln -sf "/Users/xuyingzhou/.cargo" "$TEST_HOME/.cargo" 2>/dev/null
export HOME="$TEST_HOME"

# ── 启动 host（用 ION_FAUX_REPLY 避免 LLM 调用）──
SOCK=""
HOST_PID=""
start_host() {
    SOCK="$TEST_HOME/.ion/host.sock"
    rm -f "$SOCK" 2>/dev/null
    ION_FAUX_REPLY="mcp test" ION_FAUX_REPEAT=1 $ION_BIN serve >/tmp/ion_mcp_zcode_host.log 2>&1 &
    HOST_PID=$!
    for i in $(seq 1 30); do
        sleep 1
        if $ION_BIN rpc --method list_sessions >/dev/null 2>&1; then
            break
        fi
        if ! kill -0 $HOST_PID 2>/dev/null; then
            echo "❌ host 启动失败"; cat /tmp/ion_mcp_zcode_host.log | tail -5; exit 1
        fi
    done
    CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"build"}' 2>&1)
    SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')
    sleep 1
}

# 精确 PID 清理（遵循 AGENTS.md 的 pkill 禁令）
stop_host() {
    if [[ -n "$HOST_PID" ]]; then
        kill "$HOST_PID" 2>/dev/null
        wait "$HOST_PID" 2>/dev/null
    fi
}

rpc() {
    $ION_BIN rpc --session "$SID" --method "$1" ${2:+--params "$2"} 2>&1
}

# 写 config.json 到 TEST_HOME/.ion/
write_config() {
    cat > "$TEST_HOME/.ion/config.json"
}

# ════════════════════════════════════════════════════════
echo ""
echo "Group A: 嵌套格式 mcp.servers 加载（disabled，不触发真实连接）"
# ════════════════════════════════════════════════════════

# A1: 嵌套 stdio disabled
write_config <<'EOF'
{
  "mcp": {
    "servers": {
      "test-kb": {
        "type": "stdio",
        "command": "echo",
        "args": ["hello"],
        "disabled": true
      }
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q '"test-kb"' && echo "$OUT" | grep -q '"stdio"'; then
    pass "A1: 嵌套 mcp.servers stdio 格式被 ion 正确加载"
else
    fail "A1: 嵌套 stdio 未加载 — $OUT"
fi
stop_host

# A2: 嵌套 http disabled
write_config <<'EOF'
{
  "mcp": {
    "servers": {
      "remote-api": {
        "type": "http",
        "url": "http://localhost:9999/mcp",
        "disabled": true
      }
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q '"remote-api"' && echo "$OUT" | grep -q '"streamable-http"'; then
    pass "A2: 嵌套 mcp.servers http 格式被 ion 正确加载（type:http 也接受）"
else
    fail "A2: 嵌套 http 未加载 — $OUT"
fi
stop_host

# ════════════════════════════════════════════════════════
echo ""
echo "Group B: enabled/disabled 字段兼容"
# ════════════════════════════════════════════════════════

# B1: enabled:false → disabled
write_config <<'EOF'
{
  "mcp": {
    "servers": {
      "off-srv": {
        "type": "stdio",
        "command": "echo",
        "enabled": false
      }
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q '"off-sbr"' || echo "$OUT" | grep -q '"disabled":true'; then
    pass "B1: enabled:false → server 被禁用"
else
    # get_mcp_servers 可能不返回 disabled server，检查是否不连接
    if echo "$OUT" | grep -q '"off-srv"' && echo "$OUT" | grep -q '"disabled"'; then
        pass "B1: enabled:false → server 存在且 disabled"
    else
        fail "B1: enabled:false 未生效 — $OUT"
    fi
fi
stop_host

# B2: enabled:true → 启用
write_config <<'EOF'
{
  "mcp": {
    "servers": {
      "on-srv": {
        "type": "stdio",
        "command": "echo",
        "enabled": true,
        "disabled": true
      }
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
# enabled:true 应优先于 disabled:true → server 启用（会尝试连接，echo 不是有效 MCP server 会报错但不 disabled）
if echo "$OUT" | grep -q '"on-srv"' && ! echo "$OUT" | grep -q '"on-srv".*"disabled":true'; then
    pass "B2: enabled:true 优先于 disabled:true → server 启用"
else
    fail "B2: enabled 优先级未生效 — $OUT"
fi
stop_host

# ════════════════════════════════════════════════════════
echo ""
echo "Group C: 嵌套 + 平铺混用"
# ════════════════════════════════════════════════════════

# C1: 两组不同名 → 都出现
write_config <<'EOF'
{
  "mcp_servers": {
    "flat-srv": {
      "command": "echo",
      "disabled": true
    }
  },
  "mcp": {
    "servers": {
      "nested-srv": {
        "type": "http",
        "url": "http://localhost:8888/mcp",
        "disabled": true
      }
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q '"flat-srv"' && echo "$OUT" | grep -q '"nested-srv"'; then
    pass "C1: 嵌套 + 平铺不同名 → 两组都加载"
else
    fail "C1: 混用加载失败 — flat:$(echo $OUT | grep -c flat-srv) nested:$(echo $OUT | grep -c nested-srv)"
fi
stop_host

# C2: 同名 server → 平铺优先（平铺=stdio，嵌套=http，若平铺优先→transport=stdio）
write_config <<'EOF'
{
  "mcp_servers": {
    "same-srv": {
      "command": "echo",
      "disabled": true
    }
  },
  "mcp": {
    "servers": {
      "same-srv": {
        "type": "http",
        "url": "http://localhost:9999/mcp",
        "disabled": true
      }
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
# 平铺是 stdio，嵌套是 http。若平铺优先 → transport=stdio
# 用 python 精确提取 same-srv 的 transport 字段
TRANSPORT=$(echo "$OUT" | python3 -c "
import json,sys
data = json.load(sys.stdin)
srvs = data.get('data', data) if isinstance(data, dict) else data
for s in srvs:
    if s.get('name') == 'same-srv':
        print(s.get('transport',''))
        break
" 2>/dev/null)
if [[ "$TRANSPORT" == "stdio" ]]; then
    pass "C2: 同名 server → 平铺 mcp_servers 优先（transport=stdio 胜出）"
else
    fail "C2: 平铺优先未生效 — transport='$TRANSPORT'（期望 stdio）"
fi
stop_host

# ════════════════════════════════════════════════════════
# Group D: 真实连接 + CLI 实调每个 server 工具
# ════════════════════════════════════════════════════════
if [[ "$RUN_GROUP_D" != "true" ]]; then
    echo ""
    echo "Group D: 跳过（用 'bash $0 D' 启用真实连接测试）"
    skip "Group D（4 个真实 server 连接 + 实调）"
else
    echo ""
    echo "Group D: 每个 server CLI 实调工具（真实连接）"

    # 用真实的 zcode server 配置（含 zai-mcp-server 图片识别）
    write_config <<'EOF'
{
  "mcp_servers": {
    "knowledge-base": {
      "command": "npx",
      "args": ["-y", "@dyyz1993/kb-mcp", "--stdio"],
      "env": {
        "KB_DIR": "/Users/xuyingzhou/.knowledge",
        "KB_DATA_DIR": "/Users/xuyingzhou/.kb-chat"
      }
    },
    "zai-mcp-server": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@z_ai/mcp-server"],
      "env": {
        "Z_AI_API_KEY": "23d448c18e4c4a46979771e440eda666.tWJA3pEUJakumguh",
        "Z_AI_MODE": "ZHIPU"
      }
    },
    "zread": {
      "type": "http",
      "url": "https://p.19930810.xyz:8443/g/primary/https://open.bigmodel.cn/api/mcp/zread/mcp",
      "headers": {"Authorization": "Bearer 111"}
    },
    "web-search-prime": {
      "type": "http",
      "url": "https://p.19930810.xyz:8443/g/primary/https://open.bigmodel.cn/api/mcp/web_search_prime/mcp",
      "headers": {"Authorization": "Bearer 111"}
    },
    "web-reader": {
      "type": "http",
      "url": "https://p.19930810.xyz:8443/g/primary/https://open.bigmodel.cn/api/mcp/web_reader/mcp",
      "headers": {"Authorization": "Bearer 111"}
    }
  }
}
EOF
    start_host
    # 等真实 MCP server 连接（stdio 首次 npm 下载 + http 连接）
    echo "  ⏳ 等待 MCP server 连接（15s）..."
    sleep 15

    # D1: knowledge-base → kb_list
    OUT=$(rpc call_tool '{"tool":"mcp__knowledge-base__kb_list","args":{}}')
    if echo "$OUT" | grep -q '"success"[[:space:]]*:[[:space:]]*true'; then
        pass "D1: knowledge-base kb_list 调用成功"
    else
        fail "D1: knowledge-base kb_list 失败 — $(echo "$OUT" | head -c 200)"
    fi

    # D2: web-reader → webReader
    OUT=$(rpc call_tool '{"tool":"mcp__web-reader__webReader","args":{"url":"https://example.com"}}')
    if echo "$OUT" | grep -q '"success"[[:space:]]*:[[:space:]]*true'; then
        pass "D2: web-reader webReader 调用成功"
    else
        fail "D2: web-reader webReader 失败 — $(echo "$OUT" | head -c 200)"
    fi

    # D3: web-search-prime → web_search_prime
    OUT=$(rpc call_tool '{"tool":"mcp__web-search-prime__web_search_prime","args":{"search_query":"test"}}')
    if echo "$OUT" | grep -q '"success"[[:space:]]*:[[:space:]]*true'; then
        pass "D3: web-search-prime 调用成功"
    else
        fail "D3: web-search-prime 失败 — $(echo "$OUT" | head -c 200)"
    fi

    # D4: zread → get_repo_structure
    OUT=$(rpc call_tool '{"tool":"mcp__zread__get_repo_structure","args":{"repo_name":"octocat/Hello-World"}}')
    if echo "$OUT" | grep -q '"success"[[:space:]]*:[[:space:]]*true'; then
        pass "D4: zread get_repo_structure 调用成功"
    else
        fail "D4: zread get_repo_structure 失败 — $(echo "$OUT" | head -c 200)"
    fi

    # D5: zai-mcp-server → analyze_image（图片识别，核心能力验证）
    # 用本地小图文件路径（避免网络下载延迟，在 30s RPC 超时内完成）
    TEST_IMG="/tmp/ion_mcp_test_img_$$.png"
    # 下载一个小截图用于图片识别测试（如果已有本地图就复用）
    if [[ -f "$PROJECT_DIR/output/playwright/export-refactor-mobile.png" ]]; then
        cp "$PROJECT_DIR/output/playwright/export-refactor-mobile.png" "$TEST_IMG"
    else
        curl -sL "https://upload.wikimedia.org/wikipedia/commons/thumb/4/47/PNG_transparency_demonstration_1.png/280px-PNG_transparency_demonstration_1.png" -o "$TEST_IMG" 2>/dev/null
    fi
    if [[ -f "$TEST_IMG" ]]; then
        OUT=$(rpc call_tool "{\"tool\":\"mcp__zai-mcp-server__analyze_image\",\"args\":{\"image_source\":\"$TEST_IMG\",\"prompt\":\"Describe this image in one sentence.\"}}")
        if echo "$OUT" | grep -q '"success"[[:space:]]*:[[:space:]]*true'; then
            # 进一步验证返回了描述文本（不是错误消息）
            OUTPUT_TEXT=$(echo "$OUT" | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    o=str(d.get('data',{}).get('output',''))
    print(o[:300])
except: print('')  " 2>/dev/null)
            if echo "$OUTPUT_TEXT" | grep -qi "MCP error\|not found\|error"; then
                fail "D5: analyze_image 返回错误 — $OUTPUT_TEXT"
            else
                pass "D5: zai-mcp-server analyze_image 图片识别成功（返回描述）"
            fi
        else
            fail "D5: analyze_image 调用失败 — $(echo "$OUT" | head -c 200)"
        fi
        rm -f "$TEST_IMG"
    else
        skip "D5: analyze_image（无可用测试图片）"
    fi

    stop_host
fi

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果：✅ $PASS  ❌ $FAIL  ⏭️  $SKIP"
echo "════════════════════════════════════════════════════"

rm -rf "$TEST_HOME" 2>/dev/null

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
