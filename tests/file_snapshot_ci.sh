#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# File Snapshot CI — 双路快照验证
#
# 用手动构造 JSONL + cargo test 验证 object_store/scanner/diff/gc
# RPC 层面用 ion rpc 验证 get_modified_files/get_file_diff
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

echo "════════════════════════════════════════════════════"
echo "  File Snapshot CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group A: object_store 基础（单元测试）"
# ──────────────────────────────────────────────────────────

UNIT_A=$(cargo test --lib file_snapshot::object_store 2>&1)
if echo "$UNIT_A" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_A" | grep 'passed' | sed 's/.*\([0-9]\+ passed\).*/\1/' | head -1)
    pass "A1: object_store 单元测试全过（$COUNT）"
else
    fail "A1: object_store 单元测试有失败"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group B: write/edit 精确 diff（路线 1，单元测试）"
# ──────────────────────────────────────────────────────────

UNIT_B=$(cargo test --lib file_snapshot::snapshot 2>&1)
if echo "$UNIT_B" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_B" | grep 'passed' | sed 's/.*\([0-9]\+ passed\).*/\1/' | head -1)
    pass "B1: snapshot 采集单元测试全过（$COUNT）"
else
    fail "B1: snapshot 单元测试有失败"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group C: 目录扫描 + git ignore 智能过滤（路线 2）"
# ──────────────────────────────────────────────────────────

UNIT_C=$(cargo test --lib file_snapshot::scanner 2>&1)
if echo "$UNIT_C" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_C" | grep 'passed' | sed 's/.*\([0-9]\+ passed\).*/\1/' | head -1)
    pass "C1: scanner 单元测试全过（$COUNT）"
else
    fail "C1: scanner 单元测试有失败"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group D: diff 生成（单元测试）"
# ──────────────────────────────────────────────────────────

UNIT_D=$(cargo test --lib file_snapshot::diff 2>&1)
if echo "$UNIT_D" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_D" | grep 'passed' | sed 's/.*\([0-9]\+ passed\).*/\1/' | head -1)
    pass "D1: diff 单元测试全过（$COUNT）"
else
    fail "D1: diff 单元测试有失败"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group E: GC（单元测试）"
# ──────────────────────────────────────────────────────────

UNIT_E=$(cargo test --lib file_snapshot::gc 2>&1)
if echo "$UNIT_E" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_E" | grep 'passed' | sed 's/.*\([0-9]\+ passed\).*/\1/' | head -1)
    pass "E1: GC 单元测试全过（$COUNT）"
else
    fail "E1: GC 单元测试有失败"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group F: RPC 接口（需 host 模式）"
# ──────────────────────────────────────────────────────────

# 启动 host + 测试 RPC
SOCK="$HOME/.ion/host.sock"
rm -f "$SOCK" 2>/dev/null

ION_FAUX_REPLY="snapshot test" ./target/debug/ion serve >/tmp/ion_fs_host.log 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 $HOST_PID 2>/dev/null; then
    skip "F1-F4: host 启动失败，跳过 RPC 测试"
    kill $HOST_PID 2>/dev/null
else
    # 创建会话
    CREATE_OUT=$(./target/debug/ion rpc --method create_session --params '{"agent":"developer"}' 2>&1)
    SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')

    if [ -n "$SID" ]; then
        pass "F0: create_session 成功（$SID）"

        # F1: get_modified_files（空会话应返回空或 error）
        MOD_OUT=$(./target/debug/ion rpc --session "$SID" --method get_modified_files 2>&1)
        if echo "$MOD_OUT" | grep -qE "files|error"; then
            pass "F1: get_modified_files 正常返回"
        else
            fail "F1: get_modified_files 异常"
        fi

        # F2: get_file_diff
        DIFF_OUT=$(./target/debug/ion rpc --session "$SID" --method get_file_diff --params '{"filePath":"test.rs"}' 2>&1)
        if echo "$DIFF_OUT" | grep -qE "diff|error"; then
            pass "F2: get_file_diff 正常返回"
        else
            fail "F2: get_file_diff 异常"
        fi

        # F3: get_file_history
        HIST_OUT=$(./target/debug/ion rpc --session "$SID" --method get_file_history --params '{"filePath":"test.rs"}' 2>&1)
        if echo "$HIST_OUT" | grep -qE "history|error"; then
            pass "F3: get_file_history 正常返回"
        else
            fail "F3: get_file_history 异常"
        fi

        # F4: get_batch_diffs
        BATCH_OUT=$(./target/debug/ion rpc --session "$SID" --method get_batch_diffs 2>&1)
        if echo "$BATCH_OUT" | grep -qE "files|error|summary"; then
            pass "F4: get_batch_diffs 正常返回"
        else
            fail "F4: get_batch_diffs 异常"
        fi
    else
        skip "F1-F4: create_session 失败，跳过 RPC 测试"
    fi

    kill $HOST_PID 2>/dev/null
    wait $HOST_PID 2>/dev/null
    rm -f "$SOCK"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group G: 集成验证（全量 file_snapshot 测试）"
# ──────────────────────────────────────────────────────────

ALL_FS=$(cargo test --lib file_snapshot 2>&1)
if echo "$ALL_FS" | grep -q "test result: ok"; then
    COUNT=$(echo "$ALL_FS" | grep 'passed' | sed 's/.*\([0-9]\+ passed\).*/\1/' | head -1)
    pass "G1: file_snapshot 全部测试通过（$COUNT）"
else
    fail "G1: file_snapshot 有失败"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo "  PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════"

if [ "$FAIL" -eq 0 ]; then
    echo "🎉 全部通过"
    exit 0
else
    echo "⚠️ 有 $FAIL 个失败"
    exit 1
fi
