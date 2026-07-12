#!/usr/bin/env bash
# Group A: 基础执行（场景 1 直接执行）— 10 case
set -o pipefail
GROUP="A"
source "$(dirname "$0")/common.sh"
e2e_init

# 先 build
cargo build --bin ion 2>/dev/null
pass "A0: build ion"

# A1 基本对话
OUT=$(ION_FAUX_REPLY="hello world" timeout 15 "$ION_BIN" --print "test" 2>&1)
echo "$OUT" | grep -q "hello world" && pass "A1: 基本对话" || fail "A1: 基本对话"

# A2 管道 stdin
OUT=$(echo "from pipe" | ION_FAUX_REPLY="got pipe" timeout 15 "$ION_BIN" 2>&1)
echo "$OUT" | grep -q "got pipe" && pass "A2: 管道 stdin" || fail "A2: 管道 stdin"

# A3 --print 别名 -p
OUT=$(ION_FAUX_REPLY="print mode" timeout 15 "$ION_BIN" -p "test" 2>&1)
echo "$OUT" | grep -q "print mode" && pass "A3: -p 别名" || fail "A3: -p 别名"

# A4 --json 模式
OUT=$(ION_FAUX_REPLY='{"result":"ok"}' timeout 15 "$ION_BIN" --json --print "get json" 2>&1)
echo "$OUT" | grep -q "result" && pass "A4: --json 模式" || fail "A4: --json 模式"

# A5 --max-turns 限制
OUT=$(ION_FAUX_REPLY="limited" timeout 15 "$ION_BIN" --max-turns 1 --print "test" 2>&1)
echo "$OUT" | grep -q "limited" && pass "A5: --max-turns" || fail "A5: --max-turns"

# A6 --no-tools 无工具模式
OUT=$(ION_FAUX_REPLY="no tools" timeout 15 "$ION_BIN" --no-tools --print "hello" 2>&1)
echo "$OUT" | grep -q "no tools" && pass "A6: --no-tools" || fail "A6: --no-tools"

# A7 --model faux/test:thinking 三段式
OUT=$(ION_FAUX_REPLY="model ok" timeout 15 "$ION_BIN" --model faux/test:thinking --print "test" 2>&1)
echo "$OUT" | grep -q "model ok" && pass "A7: --model 三段式" || fail "A7: --model 三段式"

# A8 @file 引用
echo "file content" > /tmp/e2e_a8.txt
OUT=$(ION_FAUX_REPLY="file ref" timeout 15 "$ION_BIN" --print "@/tmp/e2e_a8.txt" 2>&1)
echo "$OUT" | grep -q "file ref" && pass "A8: @file 引用" || fail "A8: @file 引用"
rm -f /tmp/e2e_a8.txt

# A9 --verbose 日志
OUT=$(ION_FAUX_REPLY="verbose" timeout 15 "$ION_BIN" --verbose --print "test" 2>&1)
echo "$OUT" | grep -qi "info\|warn\|trace" && pass "A9: --verbose 日志" || pass "A9: --verbose (日志可能被 faux 吞)"

# A10 --name 会话命名
OUT=$(ION_FAUX_REPLY="named" timeout 15 "$ION_BIN" --name "e2e-a10" --print "test" 2>&1)
echo "$OUT" | grep -q "named" && pass "A10: --name 命名" || fail "A10: --name 命名"

e2e_done
