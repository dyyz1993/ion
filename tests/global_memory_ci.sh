#!/usr/bin/env bash
# Memory V0.2 跨项目记忆 Agent 验证
# 验证：单例初始化 + extension_rpc（save/search/list/forget）+ 跨项目检索
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green(){ printf "\033[32m%s\033[0m\n" "$1"; }
red(){ printf "\033[31m%s\033[0m\n" "$1"; }
pass(){ green "✅ PASS: $1"; ((PASS++)); }
fail(){ red "❌ FAIL: $1"; ((FAIL++)); }

ION_BIN="$PROJECT_DIR/target/debug/ion"

echo "── Phase 0: Build ──"
cargo build --bin ion 2>&1 | tail -2

# 清理旧数据
rm -f ~/.ion/agent/global-memory.db
# 确保 serve 没在跑
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null; sleep 1

echo ""
echo "── Group A: 单例生命周期 ──"

# 启动 serve
timeout 30 "$ION_BIN" serve start >/tmp/mem-serve.log 2>&1 &
SERVE_PID=$!
sleep 4

# A1: DB 创建（on_singleton_init 触发）
if [ -f ~/.ion/agent/global-memory.db ]; then
    pass "A1 global-memory.db 创建（on_singleton_init）"
else
    fail "A1 global-memory.db 创建"
fi

# A2: extension_rpc 可用（单例已初始化）
OUTPUT=$(timeout 5 "$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"list","args":{}}' 2>&1)
if echo "$OUTPUT" | grep -q "entries"; then
    pass "A2 extension_rpc 可用（单例已初始化）"
else
    fail "A2 extension_rpc 可用 (output: $(echo "$OUTPUT" | head -3))"
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
MEM_ID=$(echo "$SAVE_OUTPUT" | grep -o '"id": *"gmem_[a-f0-9]*"' | grep -o 'gmem_[a-f0-9]*' | head -1)
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
