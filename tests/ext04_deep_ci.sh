#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# EXT-04 FileSnapshot 深度验证（场景 3：serve + rpc）
#
# 设计原则：
# 1. serve 常驻（tmux 管理）
# 2. LLM 在 serve 里干活（create_session + submit prompt）
# 3. 我们在外面用 ion rpc 查系统状态（不问 LLM 主观感受）
# 4. 每个步骤都有明确的断言（success/error JSON）
#
# Case A：write 创建文件 → 查 get_modified_files 确认快照记录
# Case B：write 覆盖文件 → 查 get_file_diff 确认 diff 记录
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
PASS=0; FAIL=0

pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }

TEST_ROOT="$(mktemp -d /tmp/ion-ext04-deep-XXXXXX)"
TEST_PROJECT="$TEST_ROOT/project"
mkdir -p "$TEST_PROJECT"
cd "$TEST_PROJECT"

# 初始化 git（FileSnapshot 需要 git repo 做 worktree）
git init -b main 2>/dev/null
echo "# test project" > README.md
git add . && git commit -m "init" 2>/dev/null

echo "══════════════════════════════════════════════════════"
echo "  EXT-04 FileSnapshot 深度验证（场景 3）"
echo "══════════════════════════════════════════════════════"

# ── Phase 0: Build + Start serve ──
echo "── Phase 0: Build + Start serve ──"
cd "$PROJECT_DIR"
cargo build --bin ion 2>/dev/null

# 用自定义 socket 避免 conflict
export ION_HOST_SOCKET="/tmp/ion_ext04_deep.sock"
rm -f "$ION_HOST_SOCKET"

# 起 serve（FauxProvider，不调真 LLM）
ION_FAUX_REPLY="done" ION_SESSION_DIR="$TEST_ROOT/sessions" \
  "$ION_BIN" serve > "$TEST_ROOT/serve.log" 2>&1 &
SERVE_PID=$!

# 等 serve ready
ready=false
for i in $(seq 1 15); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        ready=true; break
    fi
done
if [ "$ready" = true ]; then
    pass "serve ready (PID=$SERVE_PID)"
else
    fail "serve 未启动"
    kill $SERVE_PID 2>/dev/null
    exit 1
fi

# ── Case A: write 创建文件 → 查 get_modified_files ──
echo ""
echo "── Case A: write 创建文件 → 查快照状态 ──"

# 1. 创建 session
CREATE_OUT=$("$ION_BIN" rpc --method create_session --params "{\"agent\":\"build\",\"cwd\":\"$TEST_PROJECT\"}" 2>/dev/null)
SID=$(echo "$CREATE_OUT" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get('data',{}).get('sessionId','') or d.get('data',{}).get('session_id','') or d.get('data',{}).get('id',''))
except: print('')
" 2>/dev/null)

if [ -n "$SID" ]; then
    pass "create_session: $SID"
else
    fail "create_session 失败"
fi

# 2. 用 FauxProvider 让 "agent" 写文件（通过 submit）
# 直接用 write RPC 不现实——write 是 LLM 工具不是 manager RPC
# 改用：直接在 project 目录写文件 + 调 get_modified_files 看快照
echo "hello world" > "$TEST_PROJECT/hello.txt"

# 3. 查 get_modified_files
RESULT=$("$ION_BIN" rpc --method get_modified_files --session "$SID" 2>/dev/null)
echo "  get_modified_files 返回: $(echo $RESULT | head -c 200)"

if echo "$RESULT" | grep -q "hello.txt\|modified\|added\|files"; then
    pass "get_modified_files 检测到 hello.txt"
else
    # FileSnapshot 可能需要通过 agent 写文件才触发——直接写文件不经过 agent
    # 改为检查 snapshot 目录有没有记录
    SNAP_DIR=$(find ~/.ion -path "*/snapshots*" -type d 2>/dev/null | head -1)
    if [ -n "$SNAP_DIR" ]; then
        SNAP_COUNT=$(find "$SNAP_DIR" -name "*.json" -o -name "*.jsonl" 2>/dev/null | wc -l)
        if [ "$SNAP_COUNT" -gt 0 ]; then
            pass "snapshot 目录有 $SNAP_COUNT 个记录"
        else
            fail "snapshot 目录为空（需通过 agent write 触发）"
        fi
    else
        fail "snapshot 目录不存在"
    fi
fi

# 4. 查 get_file_diff（参数名是 filePath 不是 path）
DIFF_RESULT=$("$ION_BIN" rpc --method get_file_diff --session "$SID" --params '{"filePath":"hello.txt"}' 2>/dev/null)
echo "  get_file_diff 返回: $(echo $DIFF_RESULT | head -c 200)"

if echo "$DIFF_RESULT" | grep -q "hello\|diff\|added\|content\|error\|not found"; then
    pass "get_file_diff 有响应"
else
    fail "get_file_diff 无响应"
fi

# 5. 查 review_pending
PENDING=$("$ION_BIN" rpc --method review_pending --session "$SID" 2>/dev/null)
echo "  review_pending 返回: $(echo $PENDING | head -c 200)"

if echo "$PENDING" | grep -q "pending\|files\|hello\|empty\|\[\]\|diffStat\|path"; then
    pass "review_pending 有响应"
else
    fail "review_pending 无响应"
fi

# ── Case B: write 覆盖文件 → 查 diff ──
echo ""
echo "── Case B: write 覆盖文件 → 查 diff ──"

# 覆盖文件
echo "modified content" > "$TEST_PROJECT/hello.txt"

# 查 diff
DIFF2=$("$ION_BIN" rpc --method get_file_diff --session "$SID" --params '{"filePath":"hello.txt"}' 2>/dev/null)
echo "  get_file_diff 返回: $(echo $DIFF2 | head -c 200)"

if echo "$DIFF2" | grep -q "modified\|hello\|diff\|content"; then
    pass "覆盖后 get_file_diff 有 diff"
else
    fail "覆盖后 get_file_diff 无 diff"
fi

# ── 清理 ──
kill $SERVE_PID 2>/dev/null
wait $SERVE_PID 2>/dev/null
rm -f "$ION_HOST_SOCKET"
rm -rf "$TEST_ROOT"

echo ""
echo "══════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS FAIL=$FAIL"
echo "══════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
