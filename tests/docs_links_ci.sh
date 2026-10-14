#!/usr/bin/env bash
# docs_links_ci.sh — 文档链接守卫：AGENTS.md + docs/ 活动文档的相对链接必须可解析
#
# 背景：2026-10-14 全仓扫描发现 13 处存量坏链（文档归档/搬迁后链接没跟改）。
# 本脚本把「评审时逐个检查文档链接可解析」（SUBSYSTEM_DOC_TEMPLATE §维护规则 4）
# 变成违反就红的守卫测试，改文档的 commit 过一遍。
#
# 排除项（见 case 分支，均为已知合法遗留，不算坏链）：
#   - http(s)/mailto：外链
#   - file://：旧设计文档的绝对路径风格（迁移是待办，存量见 docs/design/ 多篇）
#   - /tmp/*：CI 报告快照里的临时日志
#   - *xxx* / 含 < 或 {：模板格式占位符（如 src/xxx.rs、{extension}/MANUAL.md）
# 链接指向 archive/ 是允许的（归档文档可被引用，只是不在 archive/ 里维护内容）。
#
# 用法：bash tests/docs_links_ci.sh   （退出码 0=全绿，1=有坏链）

set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

FAIL=0
CHECKED=0
BROKEN_LIST=""

for f in AGENTS.md $(find docs -name '*.md' -not -path '*/archive/*' | sort); do
  [ -f "$f" ] || continue
  while IFS= read -r p; do
    [ -z "$p" ] && continue
    case "$p" in
      http*|mailto:*|file://*|/tmp/*) continue ;;
      *xxx*|*\<*|*\{*) continue ;;
    esac
    d="$(dirname "$f")"
    CHECKED=$((CHECKED + 1))
    if [ ! -e "$d/$p" ]; then
      echo "[FAIL] [$f] -> $p"
      FAIL=$((FAIL + 1))
    fi
  done < <(grep -oE '\]\([^)]+\)' "$f" 2>/dev/null | sed -e 's/^](//' -e 's/)$//' -e 's/#.*$//' | sort -u)
done

echo "========================================="
echo "  docs_links_ci: $CHECKED links checked, $FAIL broken"
echo "========================================="
[ "$FAIL" -eq 0 ] || exit 1
exit 0
