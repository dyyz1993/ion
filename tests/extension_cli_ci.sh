#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Extension CLI CI — 验证 ion extension install/remove/list
#
# 命令行验证扩展管理：装/卸/列 WASM 扩展。
# 对齐 AGENTS.md「命令行可验证原则」。
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0

# ── Session isolation (issue #30) ──────────────────────────────────────────
# ION_SESSION_DIR isolation (issue #30)
# Use a subdirectory UNDER $HOME/.ion/agent/sessions so that scripts which
# `find $HOME/.ion/agent/sessions` still work. Each test gets a unique subdir.
if [ -z "${ION_SESSION_DIR:-}" ]; then
    export ION_SESSION_DIR="$HOME/.ion/agent/sessions/_ci_$(basename "$0" .sh)_$$"
    mkdir -p "$ION_SESSION_DIR"
    CREATED_SESSION_DIR=1
fi
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
EXT_DIR="$HOME/.ion/agent/extensions"

echo "════════════════════════════════════════════════════"
echo "  Extension CLI CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# 清理测试扩展（避免污染）
for f in ci-test-ext.wasm ci-another.wasm; do
    rm -f "$EXT_DIR/$f" 2>/dev/null
done
BASE_COUNT=$(find "$EXT_DIR" -maxdepth 1 -type f -name '*.wasm' 2>/dev/null | wc -l | tr -d ' ')

TMPDIR=$(mktemp -d)
cleanup() {
    rm -rf "$TMPDIR"
    for f in ci-test-ext.wasm ci-another.wasm; do
        rm -f "$EXT_DIR/$f" 2>/dev/null
    done
    if [ "${CREATED_SESSION_DIR:-0}" = "1" ]; then
        rm -rf "$ION_SESSION_DIR"
    fi
}
trap cleanup EXIT

# ════════════════════════════════════════════════════════
# Group A：install
# ════════════════════════════════════════════════════════
echo ""
echo "── Group A：install ──"

# 造测试 wasm 文件
echo "fake wasm content" > "$TMPDIR/ci-test-ext.wasm"

# A1：install 成功
OUT=$("$ION_BIN" extension install "$TMPDIR/ci-test-ext.wasm" 2>&1)
if echo "$OUT" | grep -q "installed extension: ci-test-ext.wasm"; then
    pass "A1: install 成功（输出含文件名）"
else
    fail "A1: install 失败"
    echo "    OUT: $OUT"
fi

# A2：文件确实拷到扩展目录
if [ -f "$EXT_DIR/ci-test-ext.wasm" ]; then
    pass "A2: 文件拷到 $EXT_DIR/ci-test-ext.wasm"
else
    fail "A2: 文件未拷到扩展目录"
fi

# A3：install 不存在的文件 → 报错
OUT=$("$ION_BIN" extension install "$TMPDIR/nonexistent.wasm" 2>&1)
if echo "$OUT" | grep -qi "not found"; then
    pass "A3: install 不存在的文件 → 报错"
else
    fail "A3: 不存在文件未报错"
    echo "    OUT: $OUT"
fi

# A4：install 非 wasm 文件 → 报错
echo "not a wasm" > "$TMPDIR/readme.txt"
OUT=$("$ION_BIN" extension install "$TMPDIR/readme.txt" 2>&1)
if echo "$OUT" | grep -qi "only .wasm"; then
    pass "A4: install 非 .wasm 文件 → 拒绝"
else
    fail "A4: 非 wasm 未拒绝"
    echo "    OUT: $OUT"
fi

# ════════════════════════════════════════════════════════
# Group B：list
# ════════════════════════════════════════════════════════
echo ""
echo "── Group B：list ──"

# 装第二个扩展
echo "another" > "$TMPDIR/ci-another.wasm"
"$ION_BIN" extension install "$TMPDIR/ci-another.wasm" >/dev/null 2>&1

# B1：list 列出两个扩展
OUT=$("$ION_BIN" extension list 2>&1)
if echo "$OUT" | grep -q "ci-test-ext.wasm" && echo "$OUT" | grep -q "ci-another.wasm"; then
    pass "B1: list 列出两个已装扩展"
else
    fail "B1: list 未列出预期扩展"
    echo "    OUT: $OUT"
fi

# B2：list 显示总数（保留用户已安装的其他 Extension）
EXPECTED_COUNT=$((BASE_COUNT + 2))
if echo "$OUT" | grep -qiE "Total:.*${EXPECTED_COUNT}"; then
    pass "B2: list 显示总数（基线 $BASE_COUNT + 测试 2）"
else
    fail "B2: list 未显示正确总数"
    echo "    OUT: $(echo "$OUT" | grep -i total)"
fi

# ════════════════════════════════════════════════════════
# Group C：remove
# ════════════════════════════════════════════════════════
echo ""
echo "── Group C：remove ──"

# C1：remove 成功（不带 .wasm 后缀）
OUT=$("$ION_BIN" extension remove ci-test-ext 2>&1)
if echo "$OUT" | grep -q "removed extension" && [ ! -f "$EXT_DIR/ci-test-ext.wasm" ]; then
    pass "C1: remove 成功（不带后缀也能删）"
else
    fail "C1: remove 失败"
    echo "    OUT: $OUT"
fi

# C2：remove 不存在的扩展 → 报错
OUT=$("$ION_BIN" extension remove nonexistent-ext 2>&1)
if echo "$OUT" | grep -qi "not found"; then
    pass "C2: remove 不存在的扩展 → 报错"
else
    fail "C2: 不存在的扩展未报错"
    echo "    OUT: $OUT"
fi

# C3：remove 带后缀也能用
OUT=$("$ION_BIN" extension remove ci-another.wasm 2>&1)
if echo "$OUT" | grep -q "removed extension"; then
    pass "C3: remove 带 .wasm 后缀也能删"
else
    fail "C3: 带后缀 remove 失败"
    echo "    OUT: $OUT"
fi

# ════════════════════════════════════════════════════════
# Group D：install 覆盖（更新）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group D：install 覆盖（更新）──"

# 装一个，改内容，再装一次（覆盖）
echo "v1" > "$TMPDIR/ci-test-ext.wasm"
"$ION_BIN" extension install "$TMPDIR/ci-test-ext.wasm" >/dev/null 2>&1
SIZE1=$(stat -f%z "$EXT_DIR/ci-test-ext.wasm" 2>/dev/null || stat -c%s "$EXT_DIR/ci-test-ext.wasm" 2>/dev/null)
echo "v2 with more content to change size" > "$TMPDIR/ci-test-ext.wasm"
"$ION_BIN" extension install "$TMPDIR/ci-test-ext.wasm" >/dev/null 2>&1
SIZE2=$(stat -f%z "$EXT_DIR/ci-test-ext.wasm" 2>/dev/null || stat -c%s "$EXT_DIR/ci-test-ext.wasm" 2>/dev/null)

if [ "$SIZE1" != "$SIZE2" ]; then
    pass "D1: install 覆盖（同名文件更新，size $SIZE1 -> $SIZE2）"
else
    fail "D1: install 未覆盖旧文件"
fi

# 清理
"$ION_BIN" extension remove ci-test-ext >/dev/null 2>&1

# ════════════════════════════════════════════════════════
# Group E：create 生成可构建的 ABI v1 Extension
# ════════════════════════════════════════════════════════
echo ""
echo "── Group E：create scaffold ──"

CREATE_DIR="$TMPDIR/create"
mkdir -p "$CREATE_DIR"
OUT=$(cd "$CREATE_DIR" && "$ION_BIN" extension create ci-hello 2>&1)

if echo "$OUT" | grep -q "scaffolded extension: ci-hello" \
   && [ -f "$CREATE_DIR/ci-hello/Cargo.toml" ] \
   && [ -f "$CREATE_DIR/ci-hello/src/lib.rs" ] \
   && [ -f "$CREATE_DIR/ci-hello/MANUAL.md" ]; then
    pass "E1: create 生成 Cargo.toml、src/lib.rs 和 MANUAL.md"
else
    fail "E1: create 脚手架不完整"
    echo "    OUT: $OUT"
fi

if grep -q "wasm32-wasip1" "$CREATE_DIR/ci-hello/src/lib.rs" \
   && grep -q "extension_version() -> u32" "$CREATE_DIR/ci-hello/src/lib.rs" \
   && grep -q "extension_execute_tool" "$CREATE_DIR/ci-hello/src/lib.rs" \
   && ! grep -q "plugin_" "$CREATE_DIR/ci-hello/src/lib.rs"; then
    pass "E2: scaffold 使用当前 ABI 命名与 wasm32-wasip1 target"
else
    fail "E2: scaffold ABI 内容不正确"
fi

if (cd "$CREATE_DIR/ci-hello" && cargo build --target wasm32-wasip1 --release -q); then
    pass "E3: scaffold 可编译为 wasm32-wasip1"
else
    fail "E3: scaffold 编译失败"
fi

WASM_PATH="$CREATE_DIR/ci-hello/target/wasm32-wasip1/release/ci_hello.wasm"
if [ -f "$WASM_PATH" ]; then
    pass "E4: scaffold 产物名称与安装提示一致"
else
    fail "E4: 未找到 scaffold WASM 产物"
fi

OUT=$(cd "$CREATE_DIR" && "$ION_BIN" extension create ../escape 2>&1)
if echo "$OUT" | grep -q "invalid extension name"; then
    pass "E5: create 拒绝路径逃逸名称"
else
    fail "E5: create 未拒绝路径逃逸名称"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then echo "❌ extension_cli_ci 有失败"; exit 1; fi
echo "✅ extension_cli_ci 全部通过"
exit 0
