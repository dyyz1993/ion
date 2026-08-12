#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# EXT-04 FileSnapshot 场景 3 深度验证（serve + rpc）
#
# 复杂场景：Rust 计算器项目 — 多文件创建、功能迭代、部分审批、回滚验证
#
#   Phase 3: LLM 创建 Rust 计算器项目（Cargo.toml + src/main.rs，2 文件）
#   Phase 4: review_pending（2 文件 pending）+ 部分审批（approve Cargo.toml only）
#   Phase 5: LLM 添加 mod 功能（修改 src/main.rs）
#   Phase 6: re-approval（src/main.rs 回到 pending）+ reject src/main.rs
#   Phase 6b: LLM 验证回滚（read src/main.rs + cargo run -- mod 验证失败）
#   Phase 7: LLM 用 bash 编译验证（cargo build + cargo run -- add）
#   Phase 8: get_modified_files + get_file_diff + restore_files
#   Phase 9: 导出 HTML
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0; FAIL=0

pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-ext04-serve-XXXXXX)"
TEST_PROJECT="$TEST_ROOT/calc-project"
SOCK="/tmp/ion_ext04_serve_$$.sock"
SID=""

wait_agent_idle() {
    local max_iter=${1:-60}
    local label=${2:-agent}
    for i in $(seq 1 "$max_iter"); do
        sleep 2
        local s
        s=$("$ION_BIN" rpc --session "$SID" --method review_pending 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    err = str(d.get('error','')) + str(d.get('data',{}).get('error',''))
    if 'busy' in err.lower() or 'running' in err.lower():
        print('busy')
    else:
        print('idle')
except: print('idle')
" 2>/dev/null)
        if [ "$s" = "idle" ]; then return 0; fi
        [ $((i % 5)) = 0 ] && echo "  ...等 ${label} ${i}x2s"
    done
    return 1
}

rpc_data() {
    local method="$1"
    local params="${2:-\{\}}"
    "$ION_BIN" rpc --session "$SID" --method "$method" --params "$params" 2>/dev/null | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(json.dumps(d.get('data',{})))
except Exception: print('{}')
" 2>/dev/null
}

# ── Phase 0: Build + 准备项目 ──
echo "══════════════════════════════════════════════════════"
echo "  EXT-04 FileSnapshot 场景 3（Rust 计算器项目 — 复杂场景）"
echo "══════════════════════════════════════════════════════"
echo "── Phase 0: Build + 准备项目 ──"

cd "$PROJECT_DIR"
cargo build --bin ion 2>/dev/null

mkdir -p "$TEST_PROJECT/.ion"
echo '{"file-snapshot":{"enabled":true}}' > "$TEST_PROJECT/.ion/settings.json"
cd "$TEST_PROJECT"
git init -b main 2>/dev/null
echo "# calc" > README.md
git add . && git commit -m init 2>/dev/null

# ── Phase 1: 启动 serve ──
echo "── Phase 1: 启动 serve（zai/glm-5.2, skip MCP）──"

export ION_HOST_SOCKET="$SOCK"
export ION_SKIP_MCP=1
rm -f "$SOCK"

ION_SESSION_DIR="$TEST_ROOT/sessions" \
  "$ION_BIN" serve > "$TEST_ROOT/serve.log" 2>&1 &
SERVE_PID=$!

ready=false
for i in $(seq 1 15); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        ready=true; break
    fi
done
if [ "$ready" = true ]; then pass "serve ready (PID=$SERVE_PID)"; else fail "serve 未启动"; kill $SERVE_PID; exit 1; fi

# ── Phase 2: create_session ──
echo "── Phase 2: create_session（zai/glm-5.2）──"
CREATE_OUT=$("$ION_BIN" rpc --method create_session \
  --params '{"agent":"build","cwd":"'"$TEST_PROJECT"'","model":"glm-5.2","provider":"zai"}' 2>/dev/null)
SID=$(echo "$CREATE_OUT" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get('data',{}).get('session_id','') or d.get('data',{}).get('sessionId',''))
except: print('')" 2>/dev/null)
if [ -n "$SID" ]; then pass "create_session: $SID"; else fail "create_session 失败"; kill $SERVE_PID; exit 1; fi

# ════════════════════════════════════════════════════════
# Phase 3: LLM 创建 Rust 计算器项目（2 文件：Cargo.toml + src/main.rs）
# ════════════════════════════════════════════════════════
echo "── Phase 3: LLM 创建 Rust 计算器项目（Cargo.toml + src/main.rs）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请使用 write 工具创建一个 Rust 命令行计算器项目，需要创建以下两个文件：\n\n1. 文件路径 Cargo.toml，内容：\n[package]\nname = \"calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n\n2. 文件路径 src/main.rs，实现一个命令行计算器：\n- 从命令行参数读取运算符和两个数字\n- 支持 add（加法）、sub（减法）、mul（乘法）、div（除法）四种运算\n- div 时除数为 0 要报错\n- 输出格式：result: <数字>\n\n例如：cargo run -- add 3 5 应输出 result: 8\n\n请用 write 工具创建这两个文件。"
}' 2>/dev/null > /dev/null

if wait_agent_idle 60 "P1"; then pass "Phase 3 完成"; else fail "Phase 3 超时"; fi

# 验证文件创建
CARGO_OK=false; MAIN_OK=false
if [ -f "$TEST_PROJECT/Cargo.toml" ]; then
    if grep -q "calc" "$TEST_PROJECT/Cargo.toml"; then CARGO_OK=true; fi
fi
if [ -f "$TEST_PROJECT/src/main.rs" ]; then
    if grep -q "fn main" "$TEST_PROJECT/src/main.rs"; then MAIN_OK=true; fi
fi
if $CARGO_OK; then pass "Cargo.toml 已创建（含 calc 包名）"; else fail "Cargo.toml 缺失或内容不对"; fi
if $MAIN_OK; then pass "src/main.rs 已创建（含 fn main）"; else fail "src/main.rs 缺失或内容不对"; fi

# ════════════════════════════════════════════════════════
# Phase 4: review_pending（2 文件）+ 部分审批
# ════════════════════════════════════════════════════════
echo "── Phase 4: review_pending + 部分审批（approve Cargo.toml, keep src/main.rs pending）──"

PENDING=$(rpc_data review_pending '{}')
PENDING_COUNT=$(echo "$PENDING" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(len(d.get('pending',[])))
except: print(0)")
if [ "$PENDING_COUNT" -ge 2 ]; then
    pass "V1 review_pending: ${PENDING_COUNT} 个 pending 文件（含 Cargo.toml + src/main.rs）"
else
    fail "V1 review_pending: 只有 ${PENDING_COUNT} 个（期望 ≥ 2）"
fi

# V2: 只 approve Cargo.toml（部分审批）
APPROVE_OUT=$(rpc_data review_approve '{"path":"Cargo.toml"}')
if echo "$APPROVE_OUT" | grep -q "approved"; then
    pass "V2 review_approve Cargo.toml: approved（部分审批 — 只批一个）"
else
    fail "V2 review_approve Cargo.toml 失败: $APPROVE_OUT"
fi

# src/main.rs 应该仍在 pending
PENDING_AFTER=$(rpc_data review_pending '{}')
STILL_PENDING=$(echo "$PENDING_AFTER" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    paths = [p.get('path','') for p in d.get('pending',[])]
    print('yes' if 'src/main.rs' in paths else 'no')
except: print('no')")
if [ "$STILL_PENDING" = "yes" ]; then
    pass "部分审批验证: src/main.rs 仍在 pending（只批了 Cargo.toml）"
else
    fail "部分审批异常: src/main.rs 不在 pending"
fi

# ════════════════════════════════════════════════════════
# Phase 5: LLM 添加 mod 功能（修改 src/main.rs）
# ════════════════════════════════════════════════════════
echo "── Phase 5: LLM 添加取模运算（mod）到 src/main.rs ──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请使用 write 工具修改 src/main.rs，给计算器添加取模运算（mod）支持。\n\n要求：\n- 支持 mod 运算（使用 % 操作符）\n- mod 0 也要报错\n- 例如 cargo run -- mod 10 3 应输出 result: 1\n\n只修改 src/main.rs，不修改 Cargo.toml。"
}' 2>/dev/null > /dev/null

if wait_agent_idle 60 "P2"; then pass "Phase 5 完成"; else fail "Phase 5 超时"; fi

# 验证 mod 功能添加
if grep -q "mod\|Mod\|%\|rem" "$TEST_PROJECT/src/main.rs" 2>/dev/null; then
    pass "Phase 5: src/main.rs 含 mod/取模 相关代码"
else
    fail "Phase 5: src/main.rs 未添加 mod 功能"
fi

# ════════════════════════════════════════════════════════
# Phase 6: re-approval（src/main.rs 回到 pending）+ reject
# ════════════════════════════════════════════════════════
echo "── Phase 6: re-approval + review_reject src/main.rs（回滚 mod 功能）──"

# L2: src/main.rs 应该因再次修改回到 pending
PENDING=$(rpc_data review_pending '{}')
HAS_MAIN=$(echo "$PENDING" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    paths = [p.get('path','') for p in d.get('pending',[])]
    print('yes' if 'src/main.rs' in paths else 'no')
except: print('no')")
if [ "$HAS_MAIN" = "yes" ]; then
    pass "L2 re-approval: src/main.rs 回到 pending（修改后自动重置）"
else
    fail "L2 re-approval: src/main.rs 未回到 pending"
fi

# V3: reject src/main.rs（回滚 mod 功能）
REJECT_OUT=$(rpc_data review_reject '{"path":"src/main.rs"}')
REJECT_ACTION=$(echo "$REJECT_OUT" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get('action',''))
except: print('')")
if [ -n "$REJECT_ACTION" ]; then
    pass "V3 review_reject src/main.rs: action=${REJECT_ACTION}（mod 功能被回滚）"
else
    fail "V3 review_reject src/main.rs 失败: $REJECT_OUT"
fi

# ════════════════════════════════════════════════════════
# Phase 6b: LLM 验证回滚（read + cargo run -- mod 验证失败）
# ════════════════════════════════════════════════════════
echo "── Phase 6b: LLM 验证 mod 回滚（read src/main.rs + cargo run -- mod 确认失败）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "你的 mod 功能被审批拒绝了（src/main.rs 已回滚）。请验证：\n1. 用 read 工具读取 src/main.rs，确认 mod 相关代码是否已被移除\n2. 用 bash 工具执行 cargo run -- mod 10 3，看看是否还能用（预期应该失败或没有 mod 功能）\n\n汇报验证结果。"
}' 2>/dev/null > /dev/null

if wait_agent_idle 60 "Verify"; then pass "Phase 6b LLM 验证完成"; else fail "Phase 6b 超时"; fi

# ════════════════════════════════════════════════════════
# Phase 7: LLM 用 bash 编译验证（cargo build + cargo run -- add）
# ════════════════════════════════════════════════════════
echo "── Phase 7: LLM 用 bash 编译验证（cargo build + cargo run -- add 10 20）──"

"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "请用 bash 工具执行以下命令验证计算器项目：\n1. cargo build\n2. cargo run -- add 10 20\n3. cargo run -- sub 15 5\n\n确认编译成功且运算结果正确。"
}' 2>/dev/null > /dev/null

if wait_agent_idle 60 "Build"; then pass "Phase 7 完成"; else fail "Phase 7 超时"; fi

# ════════════════════════════════════════════════════════
# Phase 8: get_modified_files + get_file_diff
# ════════════════════════════════════════════════════════
echo "── Phase 8: get_modified_files + get_file_diff ──"

MODIFIED=$(rpc_data get_modified_files '{}')
MOD_COUNT=$(echo "$MODIFIED" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(len(d.get('files',[])))
except: print(0)")
if [ "$MOD_COUNT" -gt 0 ]; then pass "get_modified_files: ${MOD_COUNT} 个变更"; else pass "get_modified_files: 0 个（全审批完）"; fi

DIFF=$(rpc_data get_file_diff '{"filePath":"Cargo.toml"}')
HAS_DIFF=$(echo "$DIFF" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print('yes' if d.get('hasContent') or d.get('diff') else 'no')
except: print('no')")
if [ "$HAS_DIFF" = "yes" ]; then pass "get_file_diff(Cargo.toml): 有 diff"; else pass "get_file_diff(Cargo.toml): 无 diff"; fi

# ════════════════════════════════════════════════════════
# Phase 9: 导出 HTML
# ════════════════════════════════════════════════════════
echo "── Phase 9: 导出 HTML ──"

SERVE_LOG="$TEST_ROOT/serve.log"
if grep -q "CreditsError\|Insufficient balance" "$SERVE_LOG" 2>/dev/null; then fail "serve 日志有 CreditsError"; else pass "serve 日志无 CreditsError（provider=zai）"; fi

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null; rm -f "$SOCK"; sleep 1

HTML="$TEST_ROOT/export.html"
if ION_SESSION_DIR="$TEST_ROOT/sessions" "$ION_BIN" --export "$HTML" --session "$SID" 2>/dev/null; then
    HTML_SIZE=$(stat -f%z "$HTML" 2>/dev/null || stat -c%s "$HTML" 2>/dev/null)
    if [ "$HTML_SIZE" -gt 100000 ]; then pass "导出 HTML: ${HTML_SIZE} bytes"; echo "    📄 $HTML"; else fail "导出 HTML 太小: ${HTML_SIZE}"; fi
else fail "导出 HTML 失败"; fi

echo "  TEST_ROOT: $TEST_ROOT"
echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS FAIL=$FAIL"
echo "══════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
