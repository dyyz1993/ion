#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Skill Tool CI — LLM 按需调用 skill 验证
#
# 验证：skill 工具 list / inject / fork（spawn_worker 起子任务）/ get_skills RPC
# 方式：FauxProvider 驱动（host + call_tool RPC 直调，不依赖真实 LLM）
# 隔离：HOME=临时目录（socket 隔离）+ ION_AGENT_DIR（全局 skill 隔离）
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

echo "════════════════════════════════════════════════════"
echo "  Skill Tool CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ── 准备隔离测试目录 ──
# HOME=临时目录 → socket 路径 $HOME/.ion/host.sock 隔离，不污染用户真实 ~/.ion/
TEST_TMP="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$TEST_TMP"' EXIT

FAKE_HOME="$TEST_TMP/home"
mkdir -p "$FAKE_HOME"

# 项目级 skill（在测试项目 .ion/skills/ 下）
mkdir -p "$TEST_TMP/proj/.ion/skills"
cat > "$TEST_TMP/proj/.ion/skills/code-review.md" <<'EOF'
---
name: code-review
description: Perform a thorough code review
trigger: when user asks to review code
---
# Code Review Skill
## Steps
1. Read the changed files
2. Check for common issues (security, performance, style)
3. Provide structured feedback
EOF

cat > "$TEST_TMP/proj/.ion/skills/testing.md" <<'EOF'
---
name: testing
description: Write and run tests
---
# Testing Skill
Write unit tests and integration tests.
EOF

# 全局 skill（ION_AGENT_DIR 指向隔离目录）
AGENT_DIR="$TEST_TMP/agent"
mkdir -p "$AGENT_DIR/skills"
cat > "$AGENT_DIR/skills/deployment.md" <<'EOF'
---
name: deployment
description: Deploy to production
---
# Deployment Skill
Deploy the application safely.
EOF

# 公共环境变量（所有子命令继承）
export HOME="$FAKE_HOME"
export ION_AGENT_DIR="$AGENT_DIR"

# ── 启动 host（FauxProvider，在项目目录下运行）──
cd "$TEST_TMP/proj"
ION_FAUX_REPLY="skill test" $ION_BIN serve >"$TEST_TMP/host.log" 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 $HOST_PID 2>/dev/null; then
    fail "S0: host 启动失败"
    cat "$TEST_TMP/host.log" | tail -5
    exit 1
fi
pass "S0: host 启动成功（FauxProvider + 隔离 skill 目录）"

CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')

if [ -z "$SID" ]; then
    fail "S0: create_session 失败"
    kill $HOST_PID 2>/dev/null; exit 1
fi
pass "S0: create_session 成功 (sid=${SID:0:12}...)"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group S: Skill 工具（list / inject / fork / get_skills）"

# S1: call_tool skill list — 应列出全局 + 项目级 skill
LIST_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"list"}}' 2>&1)
if echo "$LIST_OUT" | grep -q "code-review" && echo "$LIST_OUT" | grep -q "deployment" && echo "$LIST_OUT" | grep -q "testing"; then
    pass "S1: skill list 列出全局 + 项目级 skill（code-review + testing + deployment）"
else
    fail "S1: skill list 未列出预期 skill"
    echo "  输出: $(echo "$LIST_OUT" | head -5)"
fi

# S2: skill list 包含 description（frontmatter 解析）
if echo "$LIST_OUT" | grep -q "thorough code review"; then
    pass "S2: skill list 包含 description（frontmatter 解析）"
else
    fail "S2: skill list 缺 description"
fi

# S3: inject 模式 — 加载 code-review skill，返回正文
LOAD_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review"}}' 2>&1)
if echo "$LOAD_OUT" | grep -q "Skill 'code-review' loaded" && echo "$LOAD_OUT" | grep -q "Code Review Skill"; then
    pass "S3: skill inject 返回正文（code-review）"
else
    fail "S3: skill inject 未返回正文"
    echo "  输出: $(echo "$LOAD_OUT" | head -5)"
fi

# S4: inject 模式默认（不传 context 参数）— 等价于 inject
LOAD_DEFAULT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"testing"}}' 2>&1)
if echo "$LOAD_DEFAULT" | grep -q "Skill 'testing' loaded" && echo "$LOAD_DEFAULT" | grep -q "Testing Skill"; then
    pass "S4: skill inject 默认模式（不传 context）加载 testing"
else
    fail "S4: skill inject 默认模式失败"
fi

# S5: fork 模式 — spawn_worker 起子任务，skill 注入 system prompt，返回执行结果
FORK_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review","context":"fork"}}' 2>&1)
if echo "$FORK_OUT" | grep -qi "executed in fork mode" && echo "$FORK_OUT" | grep -qi '"success": true\|"success":true'; then
    pass "S5: skill fork 执行成功（spawn_worker 起子任务，返回结果）"
else
    fail "S5: skill fork 执行失败"
    echo "  输出: $(echo "$FORK_OUT" | head -5)"
fi

# S6: 加载不存在的 skill — 应报错
GHOST_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"ghost-skill"}}' 2>&1)
if echo "$GHOST_OUT" | grep -qi "not found\|error"; then
    pass "S6: 加载不存在的 skill 正确报错"
else
    fail "S6: 加载不存在 skill 未报错"
    echo "  输出: $(echo "$GHOST_OUT" | head -5)"
fi

# S7: 缺 skill_name 参数 — 应报错
MISSING_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{}}' 2>&1)
if echo "$MISSING_OUT" | grep -qi "missing.*skill_name\|error"; then
    pass "S7: 缺 skill_name 参数正确报错"
else
    fail "S7: 缺参数未报错"
fi

# S8: get_skills RPC — 已有 RPC 不受 skill 工具影响
GETSKILLS_OUT=$($ION_BIN rpc --session "$SID" --method get_skills 2>&1)
if echo "$GETSKILLS_OUT" | grep -q "code-review" && echo "$GETSKILLS_OUT" | grep -q "count"; then
    pass "S8: get_skills RPC 正常（已有 RPC 不受 skill 工具影响）"
else
    fail "S8: get_skills RPC 异常"
fi

# S9: 加载全局 skill（deployment，在 ION_AGENT_DIR 下）
DEPLOY_OUT=$($ION_BIN rpc --session "$SID" --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"deployment"}}' 2>&1)
if echo "$DEPLOY_OUT" | grep -q "Skill 'deployment' loaded" && echo "$DEPLOY_OUT" | grep -q "Deploy the application"; then
    pass "S9: 加载全局 skill（deployment，ION_AGENT_DIR 目录）"
else
    fail "S9: 加载全局 skill 失败"
    echo "  输出: $(echo "$DEPLOY_OUT" | head -5)"
fi

# ──────────────────────────────────────────────────────────
# Group E: 边界 / 空 skill
# ──────────────────────────────────────────────────────────
echo ""
echo "Group E: 边界场景"

# E1: 无 skill 时 skill list 返回提示
# 先杀掉当前 host
kill $HOST_PID 2>/dev/null
wait $HOST_PID 2>/dev/null

EMPTY_AGENT="$TEST_TMP/empty_agent"
mkdir -p "$EMPTY_AGENT"
mkdir -p "$TEST_TMP/emptyproj"

cd "$TEST_TMP/emptyproj"
ION_AGENT_DIR="$EMPTY_AGENT" \
ION_FAUX_REPLY="empty" \
$ION_BIN serve >"$TEST_TMP/empty_host.log" 2>&1 &
EMPTY_PID=$!
sleep 2

if kill -0 $EMPTY_PID 2>/dev/null; then
    EMPTY_CREATE=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
    EMPTY_SID=$(echo "$EMPTY_CREATE" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')
    if [ -n "$EMPTY_SID" ]; then
        EMPTY_LIST=$($ION_BIN rpc --session "$EMPTY_SID" --method call_tool \
          --params '{"tool":"skill","args":{"skill_name":"list"}}' 2>&1)
        if echo "$EMPTY_LIST" | grep -qi "No skills available"; then
            pass "E1: 无 skill 时 skill list 返回 'No skills available'"
        else
            fail "E1: 无 skill 时未返回正确提示"
            echo "  输出: $(echo "$EMPTY_LIST" | head -3)"
        fi
    else
        skip "E1: 空 host create_session 失败"
    fi
    kill $EMPTY_PID 2>/dev/null
    wait $EMPTY_PID 2>/dev/null
else
    skip "E1: 空 host 启动失败"
fi

# ──────────────────────────────────────────────────────────
# Group R: SkillTool 注册到 build_tools（LLM 可见性）
#
# 这个 Group 验证一个之前的 bug：SkillTool struct 定义了但没在
# build_tools() 里注册，导致 LLM 看不到 skill 工具，只能用 --skill
# flag 注入 system_prompt。修复后 skill 工具应该出现在工具列表里。
# ──────────────────────────────────────────────────────────
echo ""
echo "Group R: skill 工具注册到 build_tools（LLM 可见性）"

# 用 export-after-run 拿到工具列表（FauxProvider 驱动，无需真实 LLM）
REG_TMP="$TEST_TMP/reg"
mkdir -p "$REG_TMP"
REG_HTML="$REG_TMP/out.html"

# 准备一个 skill（让 skill_dirs 非空，避免条件分支误判）
mkdir -p "$FAKE_HOME/.ion/agent/skills"
echo "# test skill" > "$FAKE_HOME/.ion/agent/skills/test.md"

cd "$REG_TMP"
HOME="$FAKE_HOME" ION_FAUX_REPLY="ok" \
    "$ION_BIN" --export "$REG_HTML" -p "hi" 2>&1 >/dev/null || true

if [ -f "$REG_HTML" ]; then
    # 解析 HTML 里的 tools 数组
    TOOLS_JSON=$(python3 - "$REG_HTML" <<'PYEOF'
import re, base64, json, sys
html = open(sys.argv[1]).read()
m = re.search(r'<script id="session-data"[^>]*>([^<]+)</script>', html)
if not m:
    print("[]")
else:
    data = json.loads(base64.b64decode(m.group(1).strip()).decode("utf-8"))
    tools = data.get("tools", [])
    print(json.dumps([t.get("name") for t in tools]))
PYEOF
)
    if [ -n "$TOOLS_JSON" ]; then
        # 检查 skill 在工具列表里
        if echo "$TOOLS_JSON" | grep -q '"skill"'; then
            pass "R1: skill 工具在工具列表里（LLM 可见）"
        else
            fail "R1: skill 工具不在列表里（LLM 看不到）"
            echo "  实际工具: $TOOLS_JSON"
        fi

        # 检查基本工具也在（确保注册路径没破坏其他工具）
        for t in read write bash edit; do
            if echo "$TOOLS_JSON" | grep -q "\"$t\""; then
                pass "R2-$t: $t 工具仍在列表里"
            else
                fail "R2-$t: $t 工具丢失"
            fi
        done

        # 检查 --no-skills 时 skill 工具被排除
        NO_SKILL_HTML="$REG_TMP/no_skill.html"
        HOME="$FAKE_HOME" ION_FAUX_REPLY="ok" \
            "$ION_BIN" --no-skills --export "$NO_SKILL_HTML" -p "hi" 2>&1 >/dev/null || true
        if [ -f "$NO_SKILL_HTML" ]; then
            NO_SKILL_TOOLS=$(python3 - "$NO_SKILL_HTML" <<'PYEOF'
import re, base64, json, sys
html = open(sys.argv[1]).read()
m = re.search(r'<script id="session-data"[^>]*>([^<]+)</script>', html)
if not m:
    print("[]")
else:
    data = json.loads(base64.b64decode(m.group(1).strip()).decode("utf-8"))
    print(json.dumps([t.get("name") for t in data.get("tools", [])]))
PYEOF
)
            if echo "$NO_SKILL_TOOLS" | grep -q '"skill"'; then
                fail "R3: --no-skills 后 skill 工具仍在（应该排除）"
            else
                pass "R3: --no-skills 正确排除 skill 工具"
            fi
        else
            skip "R3: --no-skills 测试 HTML 未生成"
        fi
    else
        fail "R1: 工具列表解析失败"
    fi
else
    fail "R1: 注册测试 HTML 未生成"
fi

# ──────────────────────────────────────────────────────────
# Group F: fork 模式 — 独立 session 文件验证
#
# 这个 Group 验证一个之前的 bug：fork spawn 出来的子 Worker 跟主 Worker
# 用同一 cwd，session_path(cwd) 算出同一文件，导致数据混乱 + 子 Worker
# 对话历史丢失。修复后 fork 子 Worker 用 <session_id>.jsonl 独立文件。
#
# 测试链路：
#   1. 起 host（隔离 HOME）
#   2. call_tool skill(context=fork) → spawn 子 Worker
#   3. 检查 <session_id>.jsonl 文件存在（独立 session）
#   4. 检查文件内容有 message（不是空的）
#   5. export HTML 验证可导出
# ──────────────────────────────────────────────────────────
echo ""
echo "Group F: fork 模式独立 session 文件"

# Group E 已杀掉前面的 host，Group F 必须自己重启

# 准备一个 fork 用的 skill（必须在 host 启动前创建，Worker 启动时扫描）
# 全局 skill 走 ION_AGENT_DIR（跟 Group S 一致）
mkdir -p "$AGENT_DIR/skills"
cat > "$AGENT_DIR/skills/fork-test.md" << 'EOF'
# Fork Test Skill
This skill is for testing fork mode isolation.
Use the `echo` tool to echo "fork-mode-test-ok".
EOF

HOME="$FAKE_HOME" ION_FAUX_REPLY="ok" "$ION_BIN" serve &
FORK_HOST_PID=$!
sleep 4

FORK_RAW=$(HOME="$FAKE_HOME" "$ION_BIN" rpc --method create_session --params '{"agent":"build"}' 2>/dev/null)
FORK_SID=$(echo "$FORK_RAW" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('data',{}).get('session_id',''))" 2>/dev/null)

if [ -n "$FORK_SID" ]; then
    # F1: call skill fork → spawn 子 Worker
    FORK_OUT=$(HOME="$FAKE_HOME" "$ION_BIN" rpc --session "$FORK_SID" --method call_tool \
        --params '{"tool":"skill","args":{"skill_name":"fork-test","context":"fork"}}' \
        2>&1)
    FORK_SUCCESS=$(echo "$FORK_OUT" | python3 -c "
import json,sys
try:
    d = json.loads(sys.stdin.read())
    print('yes' if d.get('success') else 'no')
except: print('parse_error')
" 2>/dev/null)

    if [ "$FORK_SUCCESS" = "yes" ]; then
        pass "F1: skill fork spawn 子 Worker 成功"

        # F2: 检查 <session_id>.jsonl 文件存在（排除 memory-agent + input）
        sleep 2  # 给子 Worker 时间写 session
        # sessions 目录可能在 FAKE_HOME/.ion 或 AGENT_DIR；排除 session.jsonl / input.jsonl / memory_agent
        FORK_SESSIONS=$(find "$FAKE_HOME/.ion" "$AGENT_DIR" -name "*.jsonl" ! -name "session.jsonl" ! -name "input.jsonl" 2>/dev/null | grep -v memory_agent | head -3)
        if [ -n "$FORK_SESSIONS" ]; then
            pass "F2: fork 子 Worker 独立 session 文件存在"
            FORK_SF=$(echo "$FORK_SESSIONS" | head -1)
            FORK_SUBSID=$(head -1 "$FORK_SF" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('id','?'))" 2>/dev/null)
            FORK_LINES=$(wc -l < "$FORK_SF")

            # F3: 文件内容验证（有 session header + parentSession + spawnMeta）
            HEAD_TYPE=$(head -1 "$FORK_SF" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('type','?'))" 2>/dev/null)
            if [ "$HEAD_TYPE" = "session" ]; then
                pass "F3: fork session header 正确（sid=$FORK_SUBSID, $FORK_LINES lines）"
            else
                fail "F3: fork session header 错误（type=$HEAD_TYPE）"
            fi

            # F3b: header 含 parentSession（关联父 Worker）
            PARENT_SESSION=$(head -1 "$FORK_SF" | python3 -c "
import json, sys
e = json.loads(sys.stdin.read())
print(e.get('parentSession') or '')
" 2>/dev/null)
            if [ -n "$PARENT_SESSION" ]; then
                pass "F3b: fork header.parentSession = $PARENT_SESSION（关联父 Worker）"
            else
                fail "F3b: fork header 缺 parentSession（无血缘关联）"
            fi

            # F3c: header 含 spawnMeta（relation + spawnedBy）
            SPAWN_META=$(head -1 "$FORK_SF" | python3 -c "
import json, sys
e = json.loads(sys.stdin.read())
m = e.get('spawnMeta') or {}
rel = m.get('relation', '')
sb = m.get('spawnedBy', '')
if rel and sb:
    print(f'{rel}|{sb}')
else:
    print('')
" 2>/dev/null)
            if [ -n "$SPAWN_META" ]; then
                pass "F3c: fork header.spawnMeta 含 relation + spawnedBy（$SPAWN_META）"
            else
                fail "F3c: fork header 缺 spawnMeta"
            fi

            # F4: export HTML 可导出
            FORK_HTML="$TEST_TMP/fork_export.html"
            HOME="$FAKE_HOME" "$ION_BIN" --export "$FORK_HTML" --session "$FORK_SUBSID" 2>&1 | grep -q "Exported" && \
                pass "F4: fork 子 Worker session 可导出 HTML" || \
                fail "F4: fork 子 Worker session 导出失败"

            # F5: HTML 里有内容（不是空 entries）
            if [ -f "$FORK_HTML" ]; then
                ENTRY_COUNT=$(python3 - "$FORK_HTML" <<'PYEOF'
import re, base64, json, sys
html = open(sys.argv[1]).read()
m = re.search(r'<script id="session-data"[^>]*>([^<]+)</script>', html)
if m:
    data = json.loads(base64.b64decode(m.group(1).strip()).decode("utf-8"))
    print(len(data.get("entries", [])))
else:
    print(0)
PYEOF
)
                [ "$ENTRY_COUNT" -gt 0 ] && \
                    pass "F5: fork HTML 有 $ENTRY_COUNT 个 entries" || \
                    fail "F5: fork HTML 是空的（$ENTRY_COUNT entries）"

                # F6: HTML 里 systemPrompt 字段含 skill 内容（fork 关键证据）
                SP_VALUE=$(python3 - "$FORK_HTML" <<'PYEOF'
import re, base64, json, sys
html = open(sys.argv[1]).read()
m = re.search(r'<script id="session-data"[^>]*>([^<]+)</script>', html)
if m:
    data = json.loads(base64.b64decode(m.group(1).strip()).decode("utf-8"))
    sp = data.get("systemPrompt", "")
    # 应该包含 skill 名字和 "executing a skill" 标记
    if "skill" in sp.lower() and len(sp) > 50:
        print("ok")
    elif sp:
        print(f"short:{sp[:50]}")
    else:
        print("missing")
else:
    print("no_data")
PYEOF
)
                case "$SP_VALUE" in
                    ok) pass "F6: HTML systemPrompt 含 skill 内容（fork 注入证据）" ;;
                    missing) fail "F6: HTML 缺 systemPrompt 字段（看不到 skill 注入）" ;;
                    *) fail "F6: HTML systemPrompt 异常: $SP_VALUE" ;;
                esac
            fi
        else
            fail "F2: fork 子 Worker 独立 session 文件不存在"
            echo "  实际 sessions dir 内容:"
            ls -la "$FAKE_HOME/.ion/agent/sessions/" 2>&1 | head -10
        fi
    else
        fail "F1: skill fork 失败"
        echo "  输出: $(echo "$FORK_OUT" | head -3)"
    fi
else
    fail "F1: host create_session 失败"
fi

# 清理 fork host
kill $FORK_HOST_PID 2>/dev/null
wait $FORK_HOST_PID 2>/dev/null

# ──────────────────────────────────────────────────────────
cd "$PROJECT_DIR"
echo ""
echo "══════════════════════════════════════════"
echo "  PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "══════════════════════════════════════════"

if [ "$FAIL" -eq 0 ]; then
    echo "🎉 全部通过"
    exit 0
else
    echo "⚠️ 有 $FAIL 个失败"
    exit 1
fi
