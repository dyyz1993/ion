#!/usr/bin/env bash
#
# aggregate_ci_fault_ci.sh — 故障注入验证 scripts/aggregate_ci_results.sh（T01）
#
# 验证退出码语义 + manifest 清单 + attempts 保留：
#   1)  全 PASS 且 manifest 完整              → exit 0
#   2)  任一 FAIL                             → exit 1
#   3)  FAIL 后重试 PASS（后一次不得抹掉失败）  → exit 1，报告保留 FAIL→PASS
#   4)  manifest 条目缺结果                    → exit 2
#   5)  结果不在 manifest（脏结果混入）         → exit 2
#   6)  畸形记录（非 JSON / 缺字段 / 坏 status）→ exit 3
#   7)  结果目录为空                           → exit 4
#   8)  结果目录不存在                         → exit 4
#   9)  无 manifest（旧 runner 兼容）           → exit 0，跳过缺失检查
#  10)  all.jsonl 合并副本不重复计数
#  11)  全 SKIP                               → exit 0
#
# 用法: bash tests/aggregate_ci_fault_ci.sh   （纯故障注入，无 LLM、无 host、无 cargo）
#
set -u

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
AGG="$PROJECT_DIR/scripts/aggregate_ci_results.sh"
SB="$(mktemp -d /tmp/ion-agg-fault.XXXXXX)"
trap 'rm -rf "$SB"' EXIT

P=0
F=0

ok()   { echo "  ✅ $1"; P=$((P+1)); }
bad()  { echo "  ❌ $1"; F=$((F+1)); }

# run_agg <case> — 对沙箱 case 目录执行汇总器，返回其退出码
run_agg() {
    RESULTS_DIR="$SB/$1/results" \
    MANIFEST_FILE="$SB/$1/results/manifest.txt" \
    REPORT_PATH="$SB/$1/report.md" \
        bash "$AGG" >"$SB/$1/out.log" 2>&1
}

check_rc() { # <name> <expected_rc> <actual_rc>
    if [ "$3" -eq "$2" ]; then ok "$1 (exit=$3)"; else bad "$1: want exit=$2 got=$3"; fi
}

# ─── Case 1: 全 PASS + manifest 完整 → 0 ────────────────────────────────────
c=c1; mkdir -p "$SB/$c/results"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1,"log_path":"/x"}' > "$SB/$c/results/a.jsonl"
printf '%s\n' '{"script":"tests/b_ci.sh","status":"PASS","exit_code":0,"duration_s":2,"log_path":"/x"}' > "$SB/$c/results/b.jsonl"
printf 'tests/a_ci.sh\ntests/b_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; check_rc "C1 全 PASS + manifest 完整" 0 $?

# ─── Case 2: 任一 FAIL → 1 ─────────────────────────────────────────────────
c=c2; mkdir -p "$SB/$c/results"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' > "$SB/$c/results/a.jsonl"
printf '%s\n' '{"script":"tests/b_ci.sh","status":"FAIL","exit_code":7,"duration_s":2,"log_path":"/x.log"}' > "$SB/$c/results/b.jsonl"
printf 'tests/a_ci.sh\ntests/b_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; rc=$?
check_rc "C2 任一 FAIL" 1 $rc
grep -q "Failed Scripts" "$SB/$c/report.md" && ok "C2 报告含 Failed Scripts 章节" || bad "C2 报告缺 Failed Scripts 章节"

# ─── Case 3: FAIL 后重试 PASS → 仍 exit 1，且报告保留 FAIL→PASS ────────────
c=c3; mkdir -p "$SB/$c/results"
printf '%s\n' \
 '{"script":"tests/a_ci.sh","status":"FAIL","exit_code":1,"duration_s":1}' \
 '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":2}' > "$SB/$c/results/a.jsonl"
printf 'tests/a_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; rc=$?
check_rc "C3 后一次 PASS 不得抹掉前一次 FAIL" 1 $rc
grep -q "FAIL→PASS" "$SB/$c/report.md" && ok "C3 报告保留 FAIL→PASS 重试历史" || bad "C3 报告丢失重试历史"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if d["counts"]["fail"]==1 and d["counts"]["pass"]==1 else 1)' \
    "$SB/$c/results/aggregate-summary.json" \
    && ok "C3 summary 保留双 attempt 计数" || bad "C3 summary 计数错误"

# ─── Case 4: manifest 条目缺结果 → 2 ───────────────────────────────────────
c=c4; mkdir -p "$SB/$c/results"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' > "$SB/$c/results/a.jsonl"
printf 'tests/a_ci.sh\ntests/ghost_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; check_rc "C4 清单条目缺结果" 2 $?

# ─── Case 5: 结果不在 manifest → 2 ─────────────────────────────────────────
c=c5; mkdir -p "$SB/$c/results"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' > "$SB/$c/results/a.jsonl"
printf '%s\n' '{"script":"tests/leftover_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' > "$SB/$c/results/leftover.jsonl"
printf 'tests/a_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; check_rc "C5 脏结果混入（不在清单）" 2 $?

# ─── Case 6: 畸形记录 → 3 ──────────────────────────────────────────────────
c=c6; mkdir -p "$SB/$c/results"
printf '%s\n' \
 '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' \
 'not-json-at-all' \
 '{"script":"","status":"PASS"}' \
 '{"script":"tests/b_ci.sh","status":"MAYBE"}' > "$SB/$c/results/a.jsonl"
printf 'tests/a_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; check_rc "C6 畸形记录（3 种）" 3 $?
grep -q "Malformed Records" "$SB/$c/report.md" && ok "C6 报告含 Malformed Records 章节" || bad "C6 报告缺 Malformed Records 章节"

# ─── Case 7: 结果目录为空 → 4 ──────────────────────────────────────────────
c=c7; mkdir -p "$SB/$c/results"
run_agg "$c"; check_rc "C7 结果目录为空" 4 $?

# ─── Case 8: 结果目录不存在 → 4 ────────────────────────────────────────────
c=c8; mkdir -p "$SB/$c"
RESULTS_DIR="$SB/$c/no-such-dir" \
MANIFEST_FILE="$SB/$c/no-such-dir/manifest.txt" \
REPORT_PATH="$SB/$c/report.md" \
    bash "$AGG" >"$SB/$c/out.log" 2>&1
check_rc "C8 结果目录不存在" 4 $?

# ─── Case 9: 无 manifest（旧 runner 兼容）→ 0，缺失检查关闭 ────────────────
c=c9; mkdir -p "$SB/$c/results"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' > "$SB/$c/results/a.jsonl"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"PASS","exit_code":0,"duration_s":1}' > "$SB/$c/results/all.jsonl"
run_agg "$c"   # manifest.txt 不存在
rc=$?
check_rc "C9 无 manifest 兼容模式" 0 $rc
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); c=d["counts"]; sys.exit(0 if c["attempts"]==1 and c["unique_scripts"]==1 else 1)' \
    "$SB/$c/results/aggregate-summary.json" \
    && ok "C9 all.jsonl 不重复计数（attempts=1）" || bad "C9 all.jsonl 被重复计数"

# ─── Case 10: 全 SKIP → 0 ──────────────────────────────────────────────────
c=c10; mkdir -p "$SB/$c/results"
printf '%s\n' '{"script":"tests/a_ci.sh","status":"SKIP","reason":"env-dependent","exit_code":-1,"duration_s":0}' > "$SB/$c/results/a.jsonl"
printf 'tests/a_ci.sh\n' > "$SB/$c/results/manifest.txt"
run_agg "$c"; check_rc "C10 全 SKIP" 0 $?

# ─── Summary ────────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo "  aggregate_ci_fault_ci: $P passed / $F failed"
echo "════════════════════════════════════════"
[ "$F" -eq 0 ]
