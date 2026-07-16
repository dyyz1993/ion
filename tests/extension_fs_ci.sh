#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Extension Host API (ctx.fs) CI — 统一文件访问 + 路径逃逸防护
#
# 验证：RuntimeFileSystem 注入 + 通过 fs_probe 扩展读文件/列目录 +
#       路径逃逸（../../../ /etc/passwd）被拦截。
#
# 覆盖文档：docs/design/EXTENSION_HOST_API.md
#   Group A：内置扩展通过 ctx.fs 读文件
#   Group C：路径安全（逃逸防护）
#   Group D：ExtensionDataDirs（4 级数据目录）
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
echo "  Extension Host API (ctx.fs) CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ── 先跑 Rust 单元测试（ctx.fs 逻辑层：read_file / list_dir / path_exists / glob / safe_join）──
if cargo test --lib -- extension::fs_tests 2>&1 | grep -q "test result: ok"; then
    pass "Rust 单元测试 extension::fs_tests 全过（read/write/list/exists/glob/safe_join）"
else
    fail "Rust 单元测试 extension::fs_tests 失败"
fi

# ── 准备临时项目目录（作为 allowed_roots[0]）──
TEST_DIR=$(mktemp -d)
echo "hello-from-extension-fs" > "$TEST_DIR/target.txt"
mkdir -p "$TEST_DIR/sub"
echo "nested" > "$TEST_DIR/sub/inner.md"
# 一个 outside 文件，用于路径逃逸测试
OUTSIDE=$(mktemp)
echo "secret" > "$OUTSIDE"

cleanup() {
    kill "$HOST_PID" 2>/dev/null
    rm -f "$SOCK" "$OUTSIDE" 2>/dev/null
    rm -rf "$TEST_DIR" 2>/dev/null
}
trap cleanup EXIT

# ── 启动 host（cwd=TEST_DIR → allowed_roots[0]=TEST_DIR）──
SOCK="$HOME/.ion/host.sock"
rm -f "$SOCK" 2>/dev/null

ION_FAUX_REPLY="ctx.fs probe noop" \
  "$ION_BIN" serve >/tmp/ion_fs_host.log 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 "$HOST_PID" 2>/dev/null; then
    fail "F0: host 启动失败"
    cat /tmp/ion_fs_host.log | tail -8
    exit 1
fi
pass "F0: host 启动成功"

CREATE_OUT=$($ION_BIN rpc --method create_session --params "{\"agent\":\"developer\",\"cwd\":\"$TEST_DIR\"}" 2>&1)
SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')
if [ -z "$SID" ]; then
    fail "F0: create_session 失败（cwd=$TEST_DIR）"
    cat /tmp/ion_fs_host.log | tail -8
    exit 1
fi
pass "F0: create_session 成功（cwd=$TEST_DIR → allowed_roots[0]）"

# 辅助：调 fs_probe.<method>
fs_rpc() {
    local method="$1"; shift
    local params="$1"
    $ION_BIN rpc --session "$SID" --method extension_rpc \
        --params "{\"extension\":\"fs_probe\",\"method\":\"$method\",\"args\":$params}" 2>&1
}

# ════════════════════════════════════════════════════════
# Group A：ctx.fs 读文件（正常路径）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group A：ctx.fs 读文件（相对路径 + 绝对路径）──"

# A1 相对路径读文件
OUT=$(fs_rpc read_file "{\"path\":\"target.txt\"}")
if echo "$OUT" | grep -q "hello-from-extension-fs"; then
    pass "A1: 相对路径 read_file('target.txt') 读到内容"
else
    fail "A1: 相对路径 read_file 失败"
    echo "    OUT: $OUT"
fi

# A2 绝对路径（在 allowed_roots 内）读文件
ABS_TARGET="$TEST_DIR/target.txt"
OUT=$(fs_rpc read_file "{\"path\":\"$ABS_TARGET\"}")
if echo "$OUT" | grep -q "hello-from-extension-fs"; then
    pass "A2: 绝对路径（allowed_roots 内）read_file 读到内容"
else
    fail "A2: 绝对路径 read_file 失败"
    echo "    OUT: $OUT"
fi

# A3 list_dir
OUT=$(fs_rpc list_dir "{\"path\":\".\"}")
if echo "$OUT" | grep -q "target.txt" && echo "$OUT" | grep -q "sub"; then
    pass "A3: list_dir('.') 返回 target.txt + sub"
else
    fail "A3: list_dir('.') 未返回预期条目"
    echo "    OUT: $OUT"
fi

# A4 list_dir 子目录
OUT=$(fs_rpc list_dir "{\"path\":\"sub\"}")
if echo "$OUT" | grep -q "inner.md"; then
    pass "A4: list_dir('sub') 返回 inner.md"
else
    fail "A4: list_dir('sub') 失败"
    echo "    OUT: $OUT"
fi

# A5 path_exists
OUT=$(fs_rpc path_exists "{\"path\":\"target.txt\"}")
if echo "$OUT" | grep -qE '"exists"[[:space:]]*:[[:space:]]*true'; then
    pass "A5: path_exists('target.txt') = true"
else
    fail "A5: path_exists(true) 失败"
    echo "    OUT: $OUT"
fi
OUT=$(fs_rpc path_exists "{\"path\":\"nope.txt\"}")
if echo "$OUT" | grep -qE '"exists"[[:space:]]*:[[:space:]]*false'; then
    pass "A6: path_exists('nope.txt') = false"
else
    fail "A6: path_exists(false) 失败"
    echo "    OUT: $OUT"
fi

# A7 glob
OUT=$(fs_rpc glob "{\"pattern\":\"*.txt\"}")
if echo "$OUT" | grep -q "target.txt"; then
    pass "A7: glob('*.txt') 匹配 target.txt"
else
    fail "A7: glob('*.txt') 失败"
    echo "    OUT: $OUT"
fi

# A8 write_file roundtrip
OUT=$(fs_rpc write_file "{\"path\":\"wrote.txt\",\"content\":\"fs-write-ok\"}")
OUT2=$(fs_rpc read_file "{\"path\":\"wrote.txt\"}")
if echo "$OUT2" | grep -q "fs-write-ok"; then
    pass "A8: write_file + read_file roundtrip"
else
    fail "A8: write_file roundtrip 失败"
    echo "    OUT: $OUT / OUT2: $OUT2"
fi

# ════════════════════════════════════════════════════════
# Group C：路径安全（逃逸防护）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group C：路径逃逸防护（allowed_roots 之外一律拒绝）──"

# C1 ../../etc/passwd 被拦
OUT=$(fs_rpc read_file "{\"path\":\"../../../etc/passwd\"}")
if echo "$OUT" | grep -qi "outside allowed roots"; then
    pass "C1: '../../../etc/passwd' 被拒（outside allowed roots）"
else
    fail "C1: 路径逃逸未拦截"
    echo "    OUT: $OUT"
fi

# C2 subdir/../../etc 被拦
OUT=$(fs_rpc read_file "{\"path\":\"sub/../../etc/passwd\"}")
if echo "$OUT" | grep -qi "outside allowed roots"; then
    pass "C2: 'sub/../../etc/passwd' 被拒"
else
    fail "C2: 嵌套逃逸未拦截"
    echo "    OUT: $OUT"
fi

# C3 allowed_roots 之外的绝对路径被拦（不返回内容）
OUT=$(fs_rpc read_file "{\"path\":\"$OUTSIDE\"}")
if echo "$OUT" | grep -qi "outside allowed roots"; then
    pass "C3: allowed_roots 外的绝对路径被拒"
else
    fail "C3: 外部绝对路径未被拦截"
    echo "    OUT: $OUT"
fi

# C4 null byte 被拦
OUT=$(fs_rpc read_file "{\"path\":\"key\\u0000/etc/passwd\"}")
if echo "$OUT" | grep -qiE "null byte|outside allowed"; then
    pass "C4: null byte 路径被拒"
else
    fail "C4: null byte 未拦截"
    echo "    OUT: $OUT"
fi

# C5 path_exists 逃逸返回 false（而非报错）
OUT=$(fs_rpc path_exists "{\"path\":\"../../../etc/passwd\"}")
if echo "$OUT" | grep -qE '"exists"[[:space:]]*:[[:space:]]*false'; then
    pass "C5: path_exists(逃逸路径) = false（不报错）"
else
    fail "C5: path_exists(逃逸) 行为异常"
    echo "    OUT: $OUT"
fi

# C6 list_dir 逃逸被拒
OUT=$(fs_rpc list_dir "{\"path\":\"../../../etc\"}")
if echo "$OUT" | grep -qi "outside allowed roots"; then
    pass "C6: list_dir(逃逸路径) 被拒"
else
    fail "C6: list_dir(逃逸) 未拦截"
    echo "    OUT: $OUT"
fi

# ════════════════════════════════════════════════════════
# Group D：ExtensionDataDirs（4 级数据目录）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group D：ExtensionDataDirs（4 级数据目录）──"

# D1 data_dirs 返回 4 级目录
OUT=$(fs_rpc data_dirs '{"ext_name":"my-ext"}')
if echo "$OUT" | grep -q "global" && echo "$OUT" | grep -q "project" && echo "$OUT" | grep -q "cwd" && echo "$OUT" | grep -q "session"; then
    pass "D1: data_dirs 返回 4 级目录（global/project/cwd/session）"
else
    fail "D1: data_dirs 未返回 4 级"
    echo "    OUT: $OUT"
fi

# D2 每级路径含 ext_name
if echo "$OUT" | grep -o '"global"[^,]*' | grep -q "my-ext" && echo "$OUT" | grep -o '"session"[^}]*' | grep -q "my-ext"; then
    pass "D2: 各级目录路径含 ext_name（my-ext）"
else
    fail "D2: 路径未含 ext_name"
    echo "    OUT: $OUT"
fi

# D3 global 在 extensions-data 下
if echo "$OUT" | grep -q "extensions-data"; then
    pass "D3: global 目录在 extensions-data 下"
else
    fail "D3: global 路径异常"
    echo "    OUT: $OUT"
fi

# D4 session 含 session_id
SID_SHORT=$(echo "$SID" | sed 's/sess_//')
if echo "$OUT" | grep -q "$SID_SHORT"; then
    pass "D4: session 目录含当前 session_id（$SID_SHORT）"
else
    fail "D4: session 目录未含 session_id"
    echo "    OUT: $OUT"
fi

# D5 不同 ext_name 隔离
OUT2=$(fs_rpc data_dirs '{"ext_name":"other-ext"}')
G1=$(echo "$OUT" | grep -o '"global"[^,]*' | head -1)
G2=$(echo "$OUT2" | grep -o '"global"[^,]*' | head -1)
if [ "$G1" != "$G2" ]; then
    pass "D5: 不同 ext_name 目录隔离（my-ext ≠ other-ext）"
else
    fail "D5: ext_name 未隔离"
    echo "    G1: $G1  G2: $G2"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    echo "❌ extension_fs_ci 有失败用例"
    exit 1
fi
echo "✅ extension_fs_ci 全部通过"
exit 0
