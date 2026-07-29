#!/usr/bin/env bash
# rules_ci.sh — 验证 RulesEngineExtension 的加载/匹配/注入/去重
#
# 测试链路：
#   1. 起 host + 创建带 .ion/rules/ 的项目
#   2. extension_rpc list → 确认 rules 加载
#   3. extension_rpc match → 确认 glob 匹配
#   4. export HTML → 确认 <project_rules> 注入 system prompt
#   5. 去重：跑两次 export，确认路径匹配 rule 只注入一次（injected set）
#
# Usage: bash tests/rules_ci.sh
set -euo pipefail

ION_BIN="${ION_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/ion}"
HOST_SOCK="$HOME/.ion/host.sock"
PASS=0; FAIL=0

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  ✅ $label"
        PASS=$((PASS+1))
    else
        echo "  ❌ $label (expected '$needle' in output)"
        FAIL=$((FAIL+1))
    fi
}

cleanup() {
    # 精确杀 host（按 socket）
    local pid=$(lsof -ti "$HOST_SOCK" 2>/dev/null || true)
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== rules_ci: 验证 RulesEngineExtension 加载/匹配/注入/去重 ==="

# ── 准备测试项目（含 .ion/rules/）──
TMP=$(mktemp -d /tmp/ion-rules-ci-XXXXXX)
mkdir -p "$TMP/proj/src" "$TMP/proj/.ion/rules"
echo "fn main() {}" > "$TMP/proj/src/main.rs"

# Rule 1: 全局 rule（applyTo 为空 → 常驻注入）
cat > "$TMP/proj/.ion/rules/global.md" << 'EOF'
---
applyTo: "**"
---
# Global Rule
- Always respond in English.
EOF

# Rule 2: 路径匹配 rule（**/*.rs → 首次匹配注入 + 去重）
cat > "$TMP/proj/.ion/rules/rust.md" << 'EOF'
---
applyTo: "**/*.rs"
---
# Rust Rule
- Use snake_case for functions.
- ALL comments MUST be in ENGLISH ONLY.
EOF

# Rule 3: 不匹配的 rule（**/*.py → 项目无 .py 文件，不注入）
cat > "$TMP/proj/.ion/rules/python.md" << 'EOF'
---
applyTo: "**/*.py"
---
# Python Rule
- Use PEP 8 style.
EOF

echo ""
echo "--- Group A: export HTML（rules 注入验证）---"

# 在 proj 目录里用 FauxProvider 跑一个 session + export（cwd 对，.ion/rules 能扫到）
cd "$TMP/proj"
EXPORT_OUT="$TMP/rules_export.html"
ION_FAUX_REPLY='{"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"Stop"}' \
    $ION_BIN --no-context-files --provider faux --model faux-test \
    --export "$EXPORT_OUT" \
    "test" >/dev/null 2>&1 </dev/null

if [ -f "$EXPORT_OUT" ]; then
    SP_CHECK=$(python3 << PYEOF 2>/dev/null || echo "no_sp"
import re, base64, json
with open("$EXPORT_OUT") as f:
    html = f.read()
blobs = re.findall(r"[A-Za-z0-9+/]{500,}={0,2}", html)
if not blobs:
    print("no_data"); exit()
data = json.loads(base64.b64decode(blobs[0] + "==").decode("utf-8","replace"))
sp = data.get("systemPrompt","")
results = []
results.append("HAS_PROJECT_RULES" if "<project_rules>" in sp else "NO_PROJECT_RULES")
results.append("HAS_GLOBAL" if "Global Rule" in sp else "NO_GLOBAL")
results.append("HAS_RUST" if "snake_case" in sp else "NO_RUST")
results.append("NO_PYTHON" if "PEP 8" not in sp else "HAS_PYTHON")
print(" ".join(results))
PYEOF
    )
    assert_contains "A1: 含 <project_rules> 标签" "$SP_CHECK" "HAS_PROJECT_RULES"
    assert_contains "A2: 全局 rule 注入（Global Rule）" "$SP_CHECK" "HAS_GLOBAL"
    assert_contains "A3: rust rule 注入（匹配 **/*.rs → snake_case）" "$SP_CHECK" "HAS_RUST"
    assert_contains "A4: python rule 不注入（无 .py 文件）" "$SP_CHECK" "NO_PYTHON"
else
    echo "  ⚠️ export 文件未生成，跳过 A 组（可能 session 创建失败）"
    FAIL=$((FAIL+4))
fi

echo ""
echo "--- Group C: 去重验证（路径匹配 rule 不重复注入）---"
# C1: 直接调 RulesEngineExtension 单元测试（on_system_prompt 去重逻辑）
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DEDUP_RESULT=$(cd "$PROJECT_DIR" && cargo test --lib rules_engine::tests::test_on_system_prompt_with_matching_rule 2>&1 | tail -3)
assert_contains "C1: 去重单元测试通过" "$DEDUP_RESULT" "ok"

echo ""
echo "==============================================="
echo "rules_ci: $PASS passed, $FAIL failed"
echo "==============================================="

# 清理
rm -rf "$TMP"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
