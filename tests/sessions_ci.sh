#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Sessions CI — ion sessions（按主仓库维度查询会话）验证
#
# 验证：
#   Group A: 基础过滤（当前项目 vs --all）
#   Group B: JSON 输出字段完整性（含 tokenCacheRead/Write）
#   Group C: worktree 聚合正确性（worktree 会话归入主仓库）
#   Group D: 表格格式 + 边界情况
#
# 自包含策略：备份真实索引 → 造测试数据 → 跑测试 → 还原
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
INDEX_PATH="$HOME/.ion/agent/sessions.index.json"
BACKUP_PATH="/tmp/ion_sessions_ci_backup.json"

echo "════════════════════════════════════════════════════"
echo "  Sessions CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ── 备份真实索引，换上测试索引 ──
if [ -f "$INDEX_PATH" ]; then
    cp "$INDEX_PATH" "$BACKUP_PATH"
fi
cleanup() {
    if [ -f "$BACKUP_PATH" ]; then
        mv "$BACKUP_PATH" "$INDEX_PATH"
    else
        rm -f "$INDEX_PATH"
    fi
}
trap cleanup EXIT

# 造测试索引：3 条会话
#   - sess_main_001: 主仓库会话（/tmp/sessions_ci_repo）
#   - sess_wt_001:   worktree 会话（/tmp/sessions_ci_wt）—— 同主仓库
#   - sess_other_001: 其他项目（/tmp/sessions_ci_other）—— 不同主仓库
NOW_MS=$(python3 -c "import time; print(int(time.time()*1000))")
mkdir -p "$HOME/.ion/agent"

python3 -c "
import json, os
now = $NOW_MS
idx = {'sessions': {
    'sess_main_001': {
        'name': 'main session', 'first_name': 'main session',
        'project': '/tmp/sessions_ci_repo', 'project_name': 'sessions_ci_repo',
        'worktree': False, 'branch': 'main',
        'model': 'glm-4.7', 'agent': 'build', 'provider': 'zhipuai',
        'token_input': 1000, 'token_output': 200,
        'token_cache_read': 500, 'token_cache_write': 100,
        'compress_count': 0, 'message_count': 10, 'turn_count': 5,
        'created_at': now - 3600000, 'updated_at': now - 1800000,
        'error_count': 0
    },
    'sess_wt_001': {
        'name': 'wt session', 'first_name': 'wt session',
        'project': '/tmp/sessions_ci_wt', 'project_name': 'sessions_ci_wt',
        'worktree': True, 'branch': 'feat/test',
        'model': 'glm-4.7', 'agent': 'developer', 'provider': 'zhipuai',
        'token_input': 500, 'token_output': 50,
        'token_cache_read': 0, 'token_cache_write': 0,
        'compress_count': 0, 'message_count': 4, 'turn_count': 2,
        'created_at': now - 7200000, 'updated_at': now - 5400000,
        'error_count': 0
    },
    'sess_other_001': {
        'name': 'other', 'first_name': 'other',
        'project': '/tmp/sessions_ci_other', 'project_name': 'sessions_ci_other',
        'worktree': False, 'branch': 'main',
        'model': 'deepseek-v4', 'agent': 'build', 'provider': 'opencode',
        'token_input': 9999, 'token_output': 0,
        'token_cache_read': 0, 'token_cache_write': 0,
        'compress_count': 0, 'message_count': 0, 'turn_count': 0,
        'created_at': now - 86400000, 'updated_at': now - 86400000,
        'error_count': 0
    }
}}
os.makedirs(os.path.dirname('$INDEX_PATH'), exist_ok=True)
with open('$INDEX_PATH', 'w') as f:
    json.dump(idx, f)
"

# 造真实 git 仓库 + worktree（让 project_key_git 能算出相同 key）
REPO_DIR="/tmp/sessions_ci_repo"
WT_DIR="/tmp/sessions_ci_wt"
OTHER_DIR="/tmp/sessions_ci_other"
rm -rf "$REPO_DIR" "$WT_DIR" "$OTHER_DIR"
mkdir -p "$REPO_DIR" && cd "$REPO_DIR" && git init -q && git config user.email "t@t.t" && git config user.name "t"
git commit -q --allow-empty -m "init"
git worktree add -q "$WT_DIR" -b feat/test 2>/dev/null
mkdir -p "$OTHER_DIR" && cd "$OTHER_DIR" && git init -q && git config user.email "t@t.t" && git config user.name "t" && git commit -q --allow-empty -m "init"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group A: 基础过滤（当前项目 vs --all）"

# A1: ion sessions --all 应列出全部 3 条
ALL_OUT=$(cd "$REPO_DIR" && $ION_BIN sessions --all 2>&1)
ALL_COUNT=$(echo "$ALL_OUT" | grep -oE "Total: [0-9]+ sessions" | grep -oE "[0-9]+")
if [ "$ALL_COUNT" = "3" ]; then
    pass "A1: --all 列出全部 3 条会话"
else
    fail "A1: --all 应为 3 条，实际 $ALL_COUNT"
fi

# A2: ion sessions（当前主仓库）应过滤到 2 条（main + wt）
CUR_OUT=$(cd "$REPO_DIR" && $ION_BIN sessions 2>&1)
CUR_COUNT=$(echo "$CUR_OUT" | grep -oE "Total: [0-9]+ sessions" | grep -oE "[0-9]+")
if [ "$CUR_COUNT" = "2" ]; then
    pass "A2: 当前主仓库过滤出 2 条（main + wt，不含 other）"
else
    fail "A2: 应为 2 条，实际 $CUR_COUNT"
    echo "  输出: $(echo "$CUR_OUT" | tail -3)"
fi

# A3: other 仓库只看到自己的 1 条
OTHER_OUT=$(cd "$OTHER_DIR" && $ION_BIN sessions 2>&1)
OTHER_COUNT=$(echo "$OTHER_OUT" | grep -oE "Total: [0-9]+ sessions" | grep -oE "[0-9]+")
if [ "$OTHER_COUNT" = "1" ]; then
    pass "A3: other 仓库只看到 1 条（不含主仓库的）"
else
    fail "A3: 应为 1 条，实际 $OTHER_COUNT"
fi

# A4: other 仓库看不到 main 会话
if ! echo "$OTHER_OUT" | grep -q "sess_main_001"; then
    pass "A4: other 仓库正确过滤掉 sess_main_001"
else
    fail "A4: other 仓库错误显示了 sess_main_001"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group B: JSON 输出字段完整性（含 tokenCacheRead/Write）"

# B1: --json 输出可被 jq 解析
JSON_OUT=$(cd "$REPO_DIR" && $ION_BIN sessions --json 2>&1)
if echo "$JSON_OUT" | jq -e '.sessions | length' >/dev/null 2>&1; then
    pass "B1: --json 输出可被 jq 解析"
else
    fail "B1: --json 输出无法解析"
    echo "  输出: $(echo "$JSON_OUT" | head -3)"
fi

# B2: project 字段含 cwd 和 projectKey
if echo "$JSON_OUT" | jq -e '.project.cwd' >/dev/null 2>&1 && \
   echo "$JSON_OUT" | jq -e '.project.projectKey' >/dev/null 2>&1; then
    pass "B2: JSON project 含 cwd + projectKey"
else
    fail "B2: JSON project 缺字段"
fi

# B3: tokenCacheRead / tokenCacheWrite 字段存在
if echo "$JSON_OUT" | jq -e '.sessions[0].tokenCacheRead' >/dev/null 2>&1 && \
   echo "$JSON_OUT" | jq -e '.sessions[0].tokenCacheWrite' >/dev/null 2>&1; then
    pass "B3: JSON 含 tokenCacheRead / tokenCacheWrite 字段"
else
    fail "B3: JSON 缺 cache 字段"
fi

# B4: 验证 cache 值正确（main 会话 tokenCacheRead=500）
MAIN_CACHE=$(echo "$JSON_OUT" | jq -r '.sessions[] | select(.id=="sess_main_001") | .tokenCacheRead')
if [ "$MAIN_CACHE" = "500" ]; then
    pass "B4: sess_main_001 tokenCacheRead=500（值正确）"
else
    fail "B4: tokenCacheRead 应为 500，实际 $MAIN_CACHE"
fi

# B5: 完整字段清单检查（agent/model/branch/worktree/createdAt/updatedAt/messageCount/turnCount）
FIELD_OK=true
for f in agent model branch createdAt updatedAt messageCount turnCount tokenInput tokenOutput provider projectName; do
    if ! echo "$JSON_OUT" | jq -e ".sessions[0].$f" >/dev/null 2>&1; then
        FIELD_OK=false
        fail "B5: 缺字段 $f"
        break
    fi
done
# worktree / parentSession 可能是 false 或 null（jq -e 对 falsy 返回非零），用 has() 检查存在性
for f in worktree parentSession; do
    if ! echo "$JSON_OUT" | jq -e ".sessions[0] | has(\"$f\")" >/dev/null 2>&1; then
        FIELD_OK=false
        fail "B5: 缺字段 $f"
        break
    fi
done
if [ "$FIELD_OK" = true ]; then
    pass "B5: JSON 含全部 13 个核心字段"
fi

# B6: --all --json 时 project 为 null
ALL_JSON=$(cd "$REPO_DIR" && $ION_BIN sessions --all --json 2>&1)
ALL_PROJECT=$(echo "$ALL_JSON" | jq -r '.project')
if [ "$ALL_PROJECT" = "null" ]; then
    pass "B6: --all --json 时 project 为 null"
else
    fail "B6: --all 时 project 应为 null，实际 $ALL_PROJECT"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group C: worktree 聚合正确性"

# C1: 在主仓库执行，应看到 worktree 会话（sess_wt_001）
# 注：表格 ID 被截断成前 10 字符，用截断后的前缀匹配
if echo "$CUR_OUT" | grep -q "sess_wt_00"; then
    pass "C1: 主仓库能看到 worktree 会话 sess_wt_001"
else
    fail "C1: 主仓库看不到 worktree 会话"
fi

# C2: 在 worktree 目录执行，也应看到主仓库会话（sess_main_001）
WT_OUT=$(cd "$WT_DIR" && $ION_BIN sessions 2>&1)
# ID 被截断成前10字符；worktree 目录下应同时看到 main 和 wt 两个会话
WT_SESS_COUNT=$(echo "$WT_OUT" | grep -oE "Total: [0-9]+ sessions" | grep -oE "[0-9]+")
if [ "$WT_SESS_COUNT" = "2" ] && echo "$WT_OUT" | grep -q "sess_main_" && echo "$WT_OUT" | grep -q "sess_wt_00"; then
    pass "C2: worktree 目录同时看到主仓库 + worktree 会话（2 条）"
else
    fail "C2: worktree 目录聚合不正确（$WT_SESS_COUNT 条）"
    echo "  完整输出:"; echo "$WT_OUT" | sed 's/^/    /'
fi

# C3: worktree 目录的 projectKey 应与主仓库相同
CUR_KEY=$(cd "$REPO_DIR" && $ION_BIN sessions --json 2>&1 | jq -r '.project.projectKey')
WT_KEY=$(cd "$WT_DIR" && $ION_BIN sessions --json 2>&1 | jq -r '.project.projectKey')
if [ -n "$CUR_KEY" ] && [ "$CUR_KEY" = "$WT_KEY" ]; then
    pass "C3: 主仓库和 worktree 的 projectKey 相同（$CUR_KEY）"
else
    fail "C3: projectKey 不一致：main=$CUR_KEY wt=$WT_KEY"
fi

# C4: worktree 会话总数仍为 2（和主仓库一致，不是独立计数）
WT_COUNT=$(echo "$WT_OUT" | grep -oE "Total: [0-9]+ sessions" | grep -oE "[0-9]+")
if [ "$WT_COUNT" = "2" ]; then
    pass "C4: worktree 目录会话数 = 主仓库会话数（聚合一致）"
else
    fail "C4: worktree 会话数应为 2，实际 $WT_COUNT"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group D: 表格格式 + 边界情况"

# D1: 表格 TOKENS(IN/OUT/CA) 格式（含斜杠）
if echo "$CUR_OUT" | grep -qE "[0-9]+/[0-9]+/[0-9]+"; then
    pass "D1: 表格 TOKENS 列为 IN/OUT/CA 格式"
else
    fail "D1: 表格 TOKENS 列格式不对"
fi

# D2: 表格 Total 行含 cache 统计
if echo "$CUR_OUT" | grep -qE "cache"; then
    pass "D2: 表格 Total 行含 cache 统计"
else
    fail "D2: 表格 Total 行缺 cache 统计"
fi

# D3: worktree 会话显示 🌿 标记（用 JSON 验证更可靠）
WT_FLAG=$(echo "$JSON_OUT" | jq -r '.sessions[] | select(.id=="sess_wt_001") | .worktree')
if [ "$WT_FLAG" = "true" ]; then
    pass "D3: worktree 会话 worktree=true（表格会显示 🌿）"
else
    fail "D3: worktree 会话 worktree 标记不对（$WT_FLAG）"
fi

# D4: --limit 限制表格条数（造 3 条，limit 1 应只显示 1 行数据 + header）
LIMIT_OUT=$(cd "$REPO_DIR" && $ION_BIN sessions --limit 1 2>&1)
# 数据行数（排除 header + 分隔线 + Total + Project 行）
DATA_ROWS=$(echo "$LIMIT_OUT" | grep -cE "^sess_|^/tmp/" )
if [ "$DATA_ROWS" -le 1 ]; then
    pass "D4: --limit 1 限制表格数据行（显示 $DATA_ROWS 行）"
else
    fail "D4: --limit 1 应最多 1 行数据，实际 $DATA_ROWS"
fi

# D5: 非 git 目录降级（project_key 退化，应不匹配主仓库会话）
# 注：project 字段指向 /tmp/sessions_ci_*（已 git init），换成纯非 git 目录测试
NON_GIT=$(mktemp -d /tmp/ion_sessions_nongit.XXXXXX)
NON_GIT_OUT=$(cd "$NON_GIT" && $ION_BIN sessions 2>&1)
NON_GIT_COUNT=$(echo "$NON_GIT_OUT" | grep -oE "Total: [0-9]+ sessions" | grep -oE "[0-9]+" 2>/dev/null)
# 非 git 目录 project_key 退化成 cwd hash，与 git 仓库 key 不同 → 0 条
if [ "$NON_GIT_COUNT" = "0" ] || echo "$NON_GIT_OUT" | grep -q "No sessions found"; then
    pass "D5: 非 git 目录不匹配 git 仓库会话（0 条）"
else
    fail "D5: 非 git 目录应 0 条，实际 $NON_GIT_COUNT"
fi
rm -rf "$NON_GIT"

# ── 清理 git worktree ──
cd "$REPO_DIR" && git worktree remove "$WT_DIR" --force 2>/dev/null
cd "$PROJECT_DIR"
rm -rf "$REPO_DIR" "$OTHER_DIR"

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
