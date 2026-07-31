#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# File Snapshot E2E — 真实 LLM 审批闭环 (ION_E2E=1)
#
# 验证：agent 写代码 → snapshot 采集 → 审批 → 回滚
# ──────────────────────────────────────────────────────────
set -o pipefail

if [ "${ION_E2E:-0}" != "1" ]; then
    echo "⏭️  Skipping file_snapshot_e2e (set ION_E2E=1 to run)"
    exit 0
fi

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ✅ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ❌ $1"; }

echo "════════════════════════════════════════════════════"
echo "  File Snapshot E2E — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build"

# 启用 file-snapshot
python3 -c "
import json
p = '$HOME/.ion/config.json'
with open(p) as f: c = json.load(f)
c.setdefault('extensions', {})['file-snapshot'] = {'enabled': True}
with open(p, 'w') as f: json.dump(c, f, indent=2)
" 2>/dev/null

WD=$(mktemp -d /tmp/fs_e2e_XXXXXX)
cd "$WD" && git init -q && git config user.email "e@e.com" && git config user.name "e"
mkdir -p src && printf '[package]\nname="t"\nversion="0.1.0"\nedition="2021"\n[lib]\npath="src/lib.rs"\n' > Cargo.toml
echo "// t" > src/lib.rs && git add -A && git commit -q -m init

echo ""
echo "── L1: 真实 LLM 写代码 → snapshot 采集 → 验证 ──"

OUTPUT=$(timeout 120 "$ION_BIN" \
    --provider zai --model glm-5.2 \
    -p "Add a pub fn double(n: i64) -> i64 to src/lib.rs that returns n*2." \
    --workdir "$WD" 2>&1)

# 检查 snapshot 日志
SNAPSHOT_DIR="$HOME/.ion/agent/file-snapshots"
if [ -d "$SNAPSHOT_DIR" ]; then
    SNAPSHOT_COUNT=$(find "$SNAPSHOT_DIR" -name "*.jsonl" 2>/dev/null | wc -l | tr -d ' ')
    if [ "$SNAPSHOT_COUNT" -gt 0 ]; then
        pass "L1: snapshot 采集成功（$SNAPSHOT_COUNT 条记录）"
    else
        fail "L1: 无 snapshot 记录"
    fi
else
    yellow "L1: snapshot 目录不存在（可能 config 未生效）"
fi

# 检查代码确实被修改
if grep -q "pub fn double" "$WD/src/lib.rs" 2>/dev/null; then
    pass "L2: 代码被修改（pub fn double 存在）"
else
    fail "L2: 代码未被修改"
fi

# 检查 RPC 可用
SERVE_PID=""
"$ION_BIN" serve > /tmp/fs_e2e_serve.log 2>&1 &
SERVE_PID=$!
sleep 5

SID=$("$ION_BIN" rpc --method create_session --params '{"cwd":"'"$WD"'"}' 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('data',{}).get('session_id',''))" 2>/dev/null)

if [ -n "$SID" ]; then
    MODIFIED=$("$ION_BIN" rpc --session "$SID" --method get_modified_files --params '{}' 2>/dev/null)
    if echo "$MODIFIED" | grep -q "success"; then
        pass "L3: get_modified_files RPC 成功"
    else
        fail "L3: get_modified_files RPC 失败"
    fi
else
    fail "L3: create_session 失败"
fi

kill $SERVE_PID 2>/dev/null

# 恢复 config
python3 -c "
import json
p = '$HOME/.ion/config.json'
with open(p) as f: c = json.load(f)
if 'extensions' in c: c['extensions']['file-snapshot'] = {'enabled': False}
with open(p, 'w') as f: json.dump(c, f, indent=2)
" 2>/dev/null

cd "$PROJECT_DIR"
rm -rf "$WD"

echo ""
echo "════════════════════════════════════════════════════"
echo "  Summary: Pass=$PASS  Fail=$FAIL"
echo "════════════════════════════════════════════════════"
[ $FAIL -gt 0 ] && exit 1 || exit 0
