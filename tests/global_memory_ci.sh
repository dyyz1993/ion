#!/usr/bin/env bash
# Memory V0.2 跨项目记忆 Agent 验证
# 验证：单例初始化 + extension_rpc（save/search/list/forget）+ 跨项目检索
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0

# ── Session isolation (issue #30) ──────────────────────────────────────────
# ION_SESSION_DIR isolation (issue #30)
# Use a subdirectory UNDER $HOME/.ion/agent/sessions so that scripts which
# `find $HOME/.ion/agent/sessions` still work. Each test gets a unique subdir.
if [ -z "${ION_SESSION_DIR:-}" ]; then
    export ION_SESSION_DIR="$HOME/.ion/agent/sessions/_ci_$(basename "$0" .sh)_$$"
    mkdir -p "$ION_SESSION_DIR"
    trap 'rm -rf "$ION_SESSION_DIR"' EXIT
fi
green(){ printf "\033[32m%s\033[0m\n" "$1"; }
red(){ printf "\033[31m%s\033[0m\n" "$1"; }
pass(){ green "✅ PASS: $1"; ((PASS++)); }
fail(){ red "❌ FAIL: $1"; ((FAIL++)); }

ION_BIN="$PROJECT_DIR/target/debug/ion"

# ── 记忆库隔离（2026-08-27）：真实 LLM/蒸馏流程会把夹具写进全局记忆库，
# 曾致用户真实库 663 条测试垃圾被迫全量归档。ION_HOME 重定向到临时目录，
# db_path() 已支持该覆盖；独立 socket 确保起的是本脚本的隔离 host。
export ION_HOME="${ION_HOME:-$(mktemp -d /tmp/ion-mem-home-XXXXXX)}"
export ION_HOST_SOCKET="${ION_HOST_SOCKET:-/tmp/ion_mem_ci_$$.sock}"

echo "── Phase 0: Build ──"
cargo build --bin ion 2>&1 | tail -2

# 清理旧数据（隔离 ION_HOME 路径，禁碰真实库——曾硬编码真实路径把用户库 rm 成 0 字节）
MEM_DB="$ION_HOME/agent/global-memory.db"
rm -f "$MEM_DB"

# 隔离 socket 独立 → 必然无 host，自起隔离 host（不能复用外面的：它连的是真实库）
lsof -ti "$ION_HOST_SOCKET" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
timeout 60 "$ION_BIN" serve >/tmp/mem-serve.log 2>&1 &
SERVE_PID=$!
sleep 4
echo "  (started isolated host PID=$SERVE_PID, ION_HOME=$ION_HOME)"

echo ""
echo "── Group A: 单例生命周期 ──"

# A1: DB 创建（on_singleton_init 触发；隔离路径）
if [ -f "$MEM_DB" ]; then
    pass "A1 global-memory.db 创建（on_singleton_init）"
else
    fail "A1 global-memory.db 创建"
fi

# A2: extension_rpc 可用（单例已初始化）
OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"list","args":{}}' 2>&1)
if grep -q "entries" <<< "$OUTPUT"; then
    pass "A2 extension_rpc 可用（单例已初始化）"
else
    fail "A2 extension_rpc 可用 (output: $(echo "$OUTPUT" | head -5))"
fi

echo ""
echo "── Group B: 记忆检索 ──"

# B1: save
OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"save","args":{"content":"user prefers rust async","category":"preference","tags":"rust,async","project":"project-x","importance":8}}' 2>&1)
if echo "$OUTPUT" | grep -q "gmem_"; then
    pass "B1 save 返回 gmem ID"
else
    fail "B1 save (output: $OUTPUT)"
fi

# B2: FTS5 搜索
OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"search","args":{"query":"rust"}}' 2>&1)
if echo "$OUTPUT" | grep -q "user prefers rust async"; then
    pass "B2 FTS5 搜索命中"
else
    fail "B2 FTS5 搜索 (output: $OUTPUT)"
fi

# B3: 跨项目检索
timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"save","args":{"content":"project uses typescript","category":"preference","tags":"ts","project":"project-y","importance":5}}' >/dev/null 2>&1

OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"search","args":{"query":"project"}}' 2>&1)
COUNT=$(echo "$OUTPUT" | grep -o '"id"' | wc -l | tr -d ' ')
if [ "$COUNT" -ge 2 ]; then
    pass "B3 跨项目检索（$COUNT 条结果）"
else
    fail "B3 跨项目检索（$COUNT 条，期望 >=2）"
fi

echo ""
echo "── Group C: 软删除 + 边界 ──"

# C1: forget（用 grep 提取 ID，更健壮）
SAVE_OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"save","args":{"content":"entry-to-delete","category":"note","tags":"test","project":"test","importance":3}}' 2>&1)
MEM_ID=$(echo "$SAVE_OUTPUT" | grep -o '"id": *"gmem_[a-f0-9-]*"' | grep -o 'gmem_[a-f0-9-]*' | head -1)
OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params "{\"extension\":\"global-memory\",\"method\":\"forget\",\"args\":{\"id\":\"$MEM_ID\"}}" 2>&1)
if echo "$OUTPUT" | grep -q "true"; then
    pass "C1 forget 软删除"
else
    fail "C1 forget (output: $OUTPUT)"
fi

# C2: 验证 list 不含被删条目（forget 生效检查）
if [ -n "$MEM_ID" ]; then
    OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"list","args":{}}' 2>&1)
    if echo "$OUTPUT" | grep -q "$MEM_ID"; then
        fail "C2 软删除后 list 仍含此条目（forget 未生效）"
    else
        pass "C2 软删除后 list 不含此条目"
    fi
else
    fail "C2 无法获取 MEM_ID（save 失败？）"
fi

# C3: 未知方法报错
OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"nonexistent","args":{}}' 2>&1)
if echo "$OUTPUT" | grep -qi "unknown\|error"; then
    pass "C3 未知方法报错"
else
    fail "C3 未知方法报错"
fi

# 关闭 serve
kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null

echo ""
echo "── 结果 ──"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && green "全部通过" || red "有失败"

# 清理
rm -f ~/.ion/agent/global-memory.db
exit $FAIL
