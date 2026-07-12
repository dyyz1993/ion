#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# 文档扫描脚本 — 检查所有 .md 文件的索引状态
#
# 功能：
#   1. 扫描项目所有 .md 文件
#   2. 检查每个文件是否在 AGENTS.md 导航表中
#   3. 检查每个文件是否在 docs/README.md 索引中
#   4. 生成大纲索引（标题层级 + 状态标注 + 一句话摘要）
#   5. 输出缺失项 + 统计
# ──────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

# 颜色
green() { echo -e "\033[32m$1\033[0m"; }
red()   { echo -e "\033[31m$1\033[0m"; }
yellow(){ echo -e "\033[33m$1\033[0m"; }
dim()   { echo -e "\033[2m$1\033[0m"; }

echo "════════════════════════════════════════════════════"
echo "  文档扫描 — $(date '+%Y-%m-%d %H:%M')"
echo "════════════════════════════════════════════════════"

# ──────────────────────────────────────────────────────────
echo ""
echo "## 1. 所有 .md 文件统计"
echo ""

# 扫描所有 .md 文件（排除 target/ node_modules/ .git/）
ALL_MDS=$(find . -name "*.md" -not -path "./target/*" -not -path "./node_modules/*" -not -path "./.git/*" -not -path "./.design_library/*" | sort)

TOTAL=$(echo "$ALL_MDS" | wc -l | tr -d ' ')
echo "  总计: $TOTAL 个 .md 文件"

# 按目录分组统计
echo ""
echo "  按目录分布:"
echo "$ALL_MDS" | sed 's|/[^/]*$||' | sort | uniq -c | sort -rn | while read count dir; do
    printf "    %-40s %d\n" "$dir" "$count"
done

# ──────────────────────────────────────────────────────────
echo ""
echo "## 2. AGENTS.md 导航表覆盖检查"
echo ""

# 提取 AGENTS.md 引用的所有 .md 路径
AGENTS_REFS=$(grep -oE '\./[A-Za-z_/.-]+\.md' AGENTS.md 2>/dev/null | sed 's|^\./||' | sort -u)
AGENTS_COUNT=$(echo "$AGENTS_REFS" | grep -c . 2>/dev/null || echo 0)
echo "  AGENTS.md 引用了 $AGENTS_COUNT 个 .md 文件"

# 找出"有文件但 AGENTS.md 没引用"的
echo ""
echo "  未在 AGENTS.md 导航表中的文件:"
MISSING_IN_AGENTS=0
while IFS= read -r md; do
    rel="${md#./}"
    # 跳过 AGENTS.md 自身和 README.md（根目录的特殊文件）
    if [ "$rel" = "AGENTS.md" ] || [ "$rel" = "README.md" ]; then
        continue
    fi
    if ! echo "$AGENTS_REFS" | grep -qF "$rel"; then
        echo "    $(red '✗') $rel"
        MISSING_IN_AGENTS=$((MISSING_IN_AGENTS + 1))
    fi
done <<< "$ALL_MDS"

if [ "$MISSING_IN_AGENTS" = "0" ]; then
    echo "    $(green '✅ 全部覆盖')"
else
    echo ""
    echo "    $(red "共 $MISSING_IN_AGENTS 个文件未在 AGENTS.md 中引用")"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "## 3. docs/README.md 索引覆盖检查"
echo ""

if [ -f "docs/README.md" ]; then
    DOCS_REFS=$(grep -oE '[A-Za-z_/.-]+\.md' docs/README.md 2>/dev/null | sort -u)
    DOCS_COUNT=$(echo "$DOCS_REFS" | grep -c . 2>/dev/null || echo 0)
    echo "  docs/README.md 引用了 $DOCS_COUNT 个 .md 文件"

    # 只检查 docs/ 下的文件
    echo ""
    echo "  docs/ 下未在 docs/README.md 索引中的文件:"
    MISSING_IN_DOCS=0
    for md in $(find docs/ -name "*.md" -not -path "docs/archive/*" | sort); do
        basename=$(basename "$md")
        if ! echo "$DOCS_REFS" | grep -qF "$basename"; then
            echo "    $(red '✗') $md"
            MISSING_IN_DOCS=$((MISSING_IN_DOCS + 1))
        fi
    done

    if [ "$MISSING_IN_DOCS" = "0" ]; then
        echo "    $(green '✅ 全部覆盖')"
    else
        echo "    $(red "共 $MISSING_IN_DOCS 个文件未在 docs/README.md 中引用")"
    fi
else
    echo "  $(yellow '⚠ docs/README.md 不存在')"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "## 4. 状态标注检查"
echo ""

echo "  各状态分布:"
STATES="已完成 已验证 开发中 暂不开发 待定 设计稿"
for state in $STATES; do
    count=$(grep -rl "状态.*$state\|状态：$state" docs/ --include="*.md" 2>/dev/null | wc -l | tr -d ' ')
    if [ "$count" -gt 0 ]; then
        printf "    %-12s %d 个文档\n" "$state" "$count"
    fi
done

# 找没有状态标注的 docs/design/ 文件
echo ""
echo "  docs/design/ 中缺少状态标注的文件:"
NO_STATUS=0
for md in docs/design/*.md; do
    [ -f "$md" ] || continue
    if ! grep -qE "状态[：:]" "$md" 2>/dev/null; then
        echo "    $(yellow '⚠') $md"
        NO_STATUS=$((NO_STATUS + 1))
    fi
done
if [ "$NO_STATUS" = "0" ]; then
    echo "    $(green '✅ 全部有状态标注')"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "## 5. 大纲索引（docs/design/ 标题层级）"
echo ""

for md in docs/design/*.md; do
    [ -f "$md" ] || continue
    title=$(head -1 "$md" | sed 's/^# *//')
    status=$(grep -oE "状态[：:] *[^—]*" "$md" | head -1 | sed 's/状态[：:] *//' | sed 's/ *—.*//' | sed 's/ //g')

    # 统计二级标题数
    h2_count=$(grep -c "^## " "$md" 2>/dev/null || echo 0)

    printf "  %-50s [%s] %d 节\n" "$(basename "$md")" "${status:-无状态}" "$h2_count"
done

# ──────────────────────────────────────────────────────────
echo ""
echo "## 6. 归档检查"
echo ""

ARCHIVE_COUNT=$(find docs/archive/ -name "*.md" 2>/dev/null | wc -l | tr -d ' ')
echo "  docs/archive/ 中有 $ARCHIVE_COUNT 个归档文档"

if [ "$ARCHIVE_COUNT" -gt 0 ]; then
    echo "  归档文件列表:"
    for md in docs/archive/*.md; do
        [ -f "$md" ] || continue
        title=$(head -1 "$md" | sed 's/^# *//')
        printf "    %s\n" "$(basename "$md")"
    done
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  扫描完成"
echo "  总文档: $TOTAL"
echo "  AGENTS.md 缺失: $MISSING_IN_AGENTS"
echo "  docs/README.md 缺失: ${MISSING_IN_DOCS:-N/A}"
echo "  无状态标注: $NO_STATUS"
echo "════════════════════════════════════════════════════"
