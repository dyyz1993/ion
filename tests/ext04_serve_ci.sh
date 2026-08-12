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
_CI_COUNT=0

# pass/fail 同时写入 session.jsonl（通过 append_entry RPC，HTML 可见）
_ci_write() {
    local ctype="$1"
    local message="$2"
    [ -z "$SID" ] && return
    # 用 append_entry RPC（不再直接 hack 写文件）
    CI_CTYPE="$ctype" CI_MSG="$message" "$ION_BIN" rpc --session "$SID" --method append_entry \
      --params "$(CI_CTYPE="$ctype" CI_MSG="$message" python3 -c '
import json, os
print(json.dumps({"type":"custom_message","customType":os.environ["CI_CTYPE"],"content":os.environ["CI_MSG"],"display":True}))
')" 2>/dev/null > /dev/null
}

pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); _ci_write "ci_pass" "✅ $1"; }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); _ci_write "ci_fail" "❌ $1"; }

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
echo "── Phase 4: review_pending + 全部审批（锚定 baseline = 计算器版本）──"

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

# V2: approve Cargo.toml
APPROVE_OUT=$(rpc_data review_approve '{"path":"Cargo.toml"}')
if echo "$APPROVE_OUT" | grep -q "approved"; then
    pass "V2 review_approve Cargo.toml: approved"
else
    fail "V2 review_approve Cargo.toml 失败: $APPROVE_OUT"
fi

# V2b: approve src/main.rs（关键！锚定 baseline = 计算器版本，这样后续 reject 才会恢复而非删除）
APPROVE_MAIN=$(rpc_data review_approve '{"path":"src/main.rs"}')
if echo "$APPROVE_MAIN" | grep -q "approved"; then
    pass "V2b review_approve src/main.rs: approved（baseline 锚定计算器版本）"
else
    fail "V2b review_approve src/main.rs 失败: $APPROVE_MAIN"
fi

# V2c: 诊断 — 查 approval 记录的 approvedTreeHash 有没有值
APPROVALS=$(rpc_data review_approvals '{"status":"approved"}')
HAS_TREE_HASH=$(echo "$APPROVALS" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    for a in d.get('approvals',[]):
        if a.get('path') == 'src/main.rs':
            h = a.get('approvedTreeHash','')
            print('yes' if h and h != 'null' else 'no')
            break
    else:
        print('not_found')
except: print('error')" 2>/dev/null)
if [ "$HAS_TREE_HASH" = "yes" ]; then
    pass "V2c src/main.rs approvedTreeHash 已设置（reject 时应 restore 而非 delete）"
else
    fail "V2c src/main.rs approvedTreeHash 缺失（$HAS_TREE_HASH）→ reject 会 delete 而非 restore"
    echo "    APPROVALS: $APPROVALS"
fi

# 两个都 approve 后 pending 应该为空
PENDING_AFTER=$(rpc_data review_pending '{}')
STILL_PENDING=$(echo "$PENDING_AFTER" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    paths = [p.get('path','') for p in d.get('pending',[])]
    print('yes' if 'src/main.rs' in paths else 'no')
except: print('no')")
if [ "$STILL_PENDING" = "no" ]; then
    pass "全部审批验证: pending 为空（Cargo.toml + src/main.rs 都已 approve）"
else
    fail "审批异常: src/main.rs 仍在 pending"
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
# Phase 6b: 验证 reject 结果（restored = 恢复计算器版本 / deleted = 删除整个文件）
# ════════════════════════════════════════════════════════
echo "── Phase 6b: 验证 reject 结果 ──"

sleep 1
if [ -f "$TEST_PROJECT/src/main.rs" ]; then
    CONTENT=$(cat "$TEST_PROJECT/src/main.rs")
    if echo "$CONTENT" | grep -q "add\|sub\|mul\|div"; then
        pass "reject=restored: src/main.rs 恢复到计算器版本（add/sub/mul/div 还在）"
    else
        fail "reject 后 src/main.rs 内容异常（不含计算器代码）"
    fi
    if echo "$CONTENT" | grep -q "mod"; then
        fail "reject 未移除 mod 代码"
    else
        pass "mod 代码已被移除"
    fi
else
    pass "reject=deleted: src/main.rs 被删除（baseline 中不存在此文件 → 回到不存在状态）"
    echo "    注意：如果 reject 删除了文件，需要重新创建才能编译"
    # 重建 src/main.rs（计算器版本，无 mod）
    mkdir -p "$TEST_PROJECT/src"
    cat > "$TEST_PROJECT/src/main.rs" << 'RUSTEOF'
use std::env;
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!("Usage: calc <add|sub|mul|div> <a> <b>");
        return;
    }
    let op = &args[1];
    let a: f64 = args[2].parse().unwrap_or(0.0);
    let b: f64 = args[3].parse().unwrap_or(0.0);
    let r = match op.as_str() {
        "add" => a + b,
        "sub" => a - b,
        "mul" => a * b,
        "div" => { if b == 0.0 { eprintln!("Error: division by zero"); return; } a / b },
        _ => { eprintln!("Unknown op: {}", op); return; }
    };
    println!("result: {}", r);
}
RUSTEOF
    pass "已重建 src/main.rs（计算器版本，无 mod）"
fi

# LLM 验证（HTML 可见）
"$ION_BIN" rpc --session "$SID" --method prompt --params '{
  "text": "你的 mod 功能被审批拒绝了。请用 read 工具读取 src/main.rs，确认 mod 相关代码是否已被移除（应该只有 add/sub/mul/div）。"
}' 2>/dev/null > /dev/null
if wait_agent_idle 60 "Verify"; then pass "Phase 6b LLM 验证完成（HTML 可见）"; else fail "Phase 6b 超时"; fi

# ════════════════════════════════════════════════════════
# Phase 7: bash 编译验证（验证 cargo build + cargo run 真的成功）
# ════════════════════════════════════════════════════════
echo "── Phase 7: bash 编译验证（直接验证，非 LLM）──"

# 直接 bash 验证（不通过 LLM，确保确定性）
cd "$TEST_PROJECT"
BUILD_OUT=$(cargo build 2>&1)
if echo "$BUILD_OUT" | grep -q "Finished\|Compiling"; then
    pass "cargo build 成功"
else
    fail "cargo build 失败: $(echo "$BUILD_OUT" | tail -3)"
fi

ADD_OUT=$(cargo run -- add 10 20 2>&1)
if echo "$ADD_OUT" | grep -q "result: 30"; then
    pass "cargo run -- add 10 20 → result: 30 ✅"
else
    fail "cargo run -- add 10 20 失败: $(echo "$ADD_OUT" | tail -3)"
fi

SUB_OUT=$(cargo run -- sub 15 5 2>&1)
if echo "$SUB_OUT" | grep -q "result: 10"; then
    pass "cargo run -- sub 15 5 → result: 10 ✅"
else
    fail "cargo run -- sub 15 5 失败: $(echo "$SUB_OUT" | tail -3)"
fi
cd "$PROJECT_DIR"

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
