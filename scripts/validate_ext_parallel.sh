#!/usr/bin/env bash
# validate_ext_parallel.sh - Phase 1: parallel validation for 5 modules (DAG + isolated HOME)
#
# Design:
#   - 5 modules (EXT-02~06) x 3 scenarios = 15 scenarios
#   - Wave grouping: same module's scenarios run in different Waves (serial across Waves)
#   - Each parallel task: isolated temp HOME + isolated cwd + isolated socket
#   - After each scenario: export HTML + validate_html.py auto-check
#   - Final: generate index.html summary
#
# Usage:
#   bash scripts/validate_ext_parallel.sh              # run all 15 scenarios
#   bash scripts/validate_ext_parallel.sh --dry-run    # print schedule only
#   MAX_PARALLEL=3 bash scripts/validate_ext_parallel.sh
#
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

source "$PROJECT_DIR/scripts/ext_scenarios.sh"

ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
ION_MODEL="${ION_MODEL:-glm-5.2}"
ION_PROVIDER="${ION_PROVIDER:-zai}"
CHROME="${CHROME:-/Applications/Chromium.app/Contents/MacOS/Chromium}"
MAX_PARALLEL="${MAX_PARALLEL:-5}"
REPORT_DIR="/tmp/ext_parallel_reports"
RUN_DIR="/tmp/ext_parallel_run"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

mkdir -p "$REPORT_DIR" "$RUN_DIR"

c_red()    { printf "\033[31m%s\033[0m\n" "$*"; }
c_green()  { printf "\033[32m%s\033[0m\n" "$*"; }
c_yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
c_blue()   { printf "\033[34m%s\033[0m\n" "$*"; }

WAVES_FILE="$RUN_DIR/waves.txt"

# -- Run a single scenario (safe to run in background) --
# Args: scenario_line
# Output: writes result to $REPORT_DIR/<sid>.result (format: sid|status|pass|fail|elapsed|html)
run_one_scenario() {
    local scenario_line="$1"
    IFS='|' read -r sid ext_id name prompt pre_setup expected <<< "$scenario_line"

    local session_dir="$RUN_DIR/sessions_${sid}"
    local work_dir="$RUN_DIR/work_${sid}"
    local log_file="$RUN_DIR/${sid}.log"
    local result_file="$REPORT_DIR/${sid}.result"
    rm -rf "$session_dir" "$work_dir"
    mkdir -p "$session_dir" "$work_dir"

    local start_ts
    start_ts=$(date +%s)

    echo "[$(date +%H:%M:%S)] > $sid ($ext_id) START" >> "$log_file"

    # PRE_SETUP
    if [ -n "$pre_setup" ] && type "$pre_setup" &>/dev/null; then
        (cd "$work_dir" && "$pre_setup" "$work_dir") >> "$log_file" 2>&1
    fi

    # Write prompt file (UTF-8 safe)
    local prompt_file="$RUN_DIR/${sid}_prompt.txt"
    python3 -c "
with open('$prompt_file', 'w', encoding='utf-8') as f:
    f.write('''$prompt''')
"

    # Run ion - isolation strategy (NO HOME override, keeps provider/npm cache):
    #   - ION_SESSION_DIR: each scenario gets own session JSONL dir (no session collision)
    #   - ION_HOST_SOCKET: each scenario gets own socket (parallel hosts don't clash)
    #   - work_dir as cwd: isolates file_snapshot project_key + project-level .ion/hooks.json
    #   - Same-module scenarios are in different Waves (serial), so global-memory.db is safe
    (
        cd "$work_dir"
        ION_SESSION_DIR="$session_dir" \
        ION_HOST_SOCKET="$RUN_DIR/host_${sid}.sock" \
        timeout 600 \
        "$ION_BIN" --agent developer \
            --model "$ION_MODEL" --provider "$ION_PROVIDER" \
            --profile autopilot \
            "@$prompt_file" >> "$log_file" 2>&1
    )
    local rc=$?
    local elapsed
    elapsed=$(( $(date +%s) - start_ts ))

    if [ $rc -ne 0 ] && [ $rc -ne 124 ]; then
        echo "[$(date +%H:%M:%S)] ! $sid ion exit=$rc (${elapsed}s)" >> "$log_file"
    fi

    # Export HTML - find session in ION_SESSION_DIR (not ~/.ion)
    local latest_dir jsonl_file html_file
    latest_dir=$(ls -dt "$session_dir"/* 2>/dev/null | head -1)
    jsonl_file=""
    if [ -n "$latest_dir" ]; then
        jsonl_file=$(ls "$latest_dir"/sess_*.jsonl "$latest_dir"/session.jsonl 2>/dev/null | head -1)
    fi
    html_file="$REPORT_DIR/${sid}_${ext_id}.html"

    if [ -z "$jsonl_file" ]; then
        c_red "  X $sid no session found (exit=$rc ${elapsed}s)"
        echo "$sid|ERROR|0|0|$elapsed|" > "$result_file"
        return 1
    fi

    local sid_from_jsonl
    sid_from_jsonl=$(basename "$jsonl_file" .jsonl)
    rm -f "$html_file"
    # CRITICAL: --export <path> is the OUTPUT path, not session id.
    # Session id must be passed via --session. Also set ION_SESSION_DIR so
    # ion searches the isolated session dir (not ~/.ion/agent/sessions/).
    (ION_SESSION_DIR="$session_dir" "$ION_BIN" --session "$sid_from_jsonl" --export "$html_file") >> "$log_file" 2>&1

    # Copy hook log to HTML dir for EXT-06 validation (validate_html.py looks for it there)
    if [ -f "$work_dir/hook_log.txt" ]; then
        cp "$work_dir/hook_log.txt" "$REPORT_DIR/hook_log.txt"
    fi

    if [ ! -s "$html_file" ]; then
        c_red "  X $sid HTML export failed"
        echo "$sid|ERROR|0|0|$elapsed|" > "$result_file"
        return 1
    fi

    # Validate
    local report_json="$REPORT_DIR/${sid}_report.json"
    python3 "$PROJECT_DIR/scripts/validate_html.py" "$html_file" \
        --chrome "$CHROME" --ext "$ext_id" > /dev/null 2>"$report_json"

    # Judge: expected metrics all pass?
    local metric_status
    metric_status=$(python3 << PYEOF
import json
with open("$report_json") as f:
    data = json.load(f)
expected_ids = "$expected".split(",") if "$expected" else []
checks = {c["id"]: c["status"] for c in data.get("checks", [])}
miss = [eid for eid in expected_ids if checks.get(eid) != "PASS"]
total_pass = data.get("passed", 0)
total_fail = data.get("failed", 0)
if miss:
    print("FAIL|%d|%d|%s" % (total_pass, total_fail, ",".join(miss)))
else:
    print("PASS|%d|%d|" % (total_pass, total_fail))
PYEOF
)
    local status tpass tfail missed
    status=$(echo "$metric_status" | cut -d'|' -f1)
    tpass=$(echo "$metric_status" | cut -d'|' -f2)
    tfail=$(echo "$metric_status" | cut -d'|' -f3)
    missed=$(echo "$metric_status" | cut -d'|' -f4)

    echo "$sid|$status|$tpass|$tfail|$elapsed|$html_file" > "$result_file"

    if [ "$status" = "PASS" ]; then
        c_green "  OK $sid PASS ($tpass/$((tpass+tfail)) ${elapsed}s)"
    else
        c_red "  XX $sid FAIL (pass=$tpass fail=$tfail missed=$missed ${elapsed}s)"
    fi

    rm -rf "$session_dir"
}

# -- Build Wave schedule (bash 3.2 compatible: file-based, no associative arrays) --
# Strategy: module N's scenario N goes to Wave N (same-module scenarios serial across Waves)
# Output: $WAVES_FILE (each line "wave_num|scenario_line") + echo max_waves
build_waves() {
    rm -f "$WAVES_FILE"
    local group_dir="$RUN_DIR/groups"
    rm -rf "$group_dir" && mkdir -p "$group_dir"

    # Group scenarios by ext_id
    get_scenarios_for_ext "" | while IFS= read -r line; do
        [ -z "$line" ] && continue
        local eid
        eid=$(echo "$line" | cut -d'|' -f2)
        echo "$line" >> "$group_dir/$eid"
    done

    # Find max scenarios per module (= number of Waves)
    local max_waves=0
    for gf in "$group_dir"/*; do
        [ -f "$gf" ] || continue
        local count
        count=$(grep -c . "$gf")
        [ "$count" -gt "$max_waves" ] && max_waves=$count
    done

    # Wave N = each module's Nth scenario
    local w
    for w in $(seq 1 $max_waves); do
        for gf in "$group_dir"/*; do
            [ -f "$gf" ] || continue
            local scenario
            scenario=$(sed -n "${w}p" "$gf")
            [ -z "$scenario" ] && continue
            echo "${w}|${scenario}" >> "$WAVES_FILE"
        done
    done

    echo "$max_waves"
}

# -- Generate HTML index summary --
generate_index() {
    python3 << PYEOF
import os, html
from collections import defaultdict

report_dir = "$REPORT_DIR"
rows = []
for f in sorted(os.listdir(report_dir)):
    if not f.endswith('.result'):
        continue
    with open(os.path.join(report_dir, f)) as fh:
        line = fh.read().strip()
        if not line:
            continue
        parts = line.split('|')
        if len(parts) >= 6:
            rows.append(parts)

total = len(rows)
passed = sum(1 for r in rows if r[1] == 'PASS')
failed = sum(1 for r in rows if r[1] == 'FAIL')
errors = sum(1 for r in rows if r[1] == 'ERROR')

by_module = defaultdict(list)
for r in rows:
    ext_id = r[0].split('-')[0]
    by_module[ext_id].append(r)

    with open(os.path.join(report_dir, 'index.html'), 'w') as f:
        f.write(f'''<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>ION Ext Validation $TIMESTAMP</title>
<style>
body {{ font-family: -apple-system, sans-serif; margin: 40px; background: #f6f8fa; }}
h1 {{ color: #24292e; }}
.summary {{ display: flex; gap: 20px; margin: 20px 0; }}
.card {{ background: white; padding: 20px; border-radius: 8px; box-shadow: 0 1px 3px rgba(0,0,0,0.1); text-align: center; }}
.card .num {{ font-size: 2em; font-weight: bold; }}
.card .label {{ color: #586069; }}
.pass .num {{ color: #28a745; }} .fail .num {{ color: #cb2431; }} .error .num {{ color: #d73a49; }}
table {{ border-collapse: collapse; width: 100%; background: white; border-radius: 8px; overflow: hidden; }}
th, td {{ padding: 10px 14px; text-align: left; border-bottom: 1px solid #e1e4e8; }}
th {{ background: #f1f8ff; }}
.status-PASS {{ color: #28a745; font-weight: bold; }}
.status-FAIL {{ color: #cb2431; font-weight: bold; }}
.status-ERROR {{ color: #d73a49; font-weight: bold; }}
a {{ color: #0366d6; }}
</style></head><body>
<h1>ION Extension Validation Report</h1>
<p style="color:#586069">$TIMESTAMP | model: $ION_MODEL/$ION_PROVIDER</p>
<div class="summary">
  <div class="card"><div class="num">{total}</div><div class="label">total</div></div>
  <div class="card pass"><div class="num">{passed}</div><div class="label">passed</div></div>
  <div class="card fail"><div class="num">{failed}</div><div class="label">failed</div></div>
  <div class="card error"><div class="num">{errors}</div><div class="label">error</div></div>
</div>
<table><tr><th>scenario</th><th>module</th><th>status</th><th>checks</th><th>elapsed</th><th>html</th></tr>
''')
        for ext_id in sorted(by_module):
            for r in sorted(by_module[ext_id]):
                sid, status, tpass, tfail, elapsed, html_path = r[:6]
                short_html = os.path.basename(html_path) if html_path else '-'
                f.write('<tr><td>%s</td><td>EXT-%s</td><td class="status-%s">%s</td>'
                        '<td>%s/%d</td><td>%ss</td>'
                        '<td><a href="%s">%s</a></td></tr>\n' % (
                            html.escape(sid), ext_id, status, status,
                            tpass, int(tpass)+int(tfail), elapsed,
                            short_html, short_html))
        f.write('</table></body></html>\n')

print("  Index: %s/index.html" % report_dir)
PYEOF
}

# -- Main --
main() {
    DRY_RUN=0
    [ "${1:-}" = "--dry-run" ] && DRY_RUN=1

    c_blue "============================================================"
    c_blue "  ION Extension Parallel Validation (Phase 1: EXT-02~06)"
    c_blue "============================================================"
    c_blue "  model:    $ION_MODEL / $ION_PROVIDER"
    c_blue "  parallel: $MAX_PARALLEL"
    c_blue "  reports:  $REPORT_DIR"
    c_blue ""

    rm -f "$REPORT_DIR"/*.result "$REPORT_DIR"/*.html "$REPORT_DIR"/summary.csv "$REPORT_DIR"/index.html 2>/dev/null
    echo "scenario|status|pass|fail|elapsed|html" > "$REPORT_DIR/summary.csv"

    local max_waves
    max_waves=$(build_waves)
    c_blue "  waves:    $max_waves (5 modules parallel per Wave, serial across Waves)"
    c_blue ""

    if [ $DRY_RUN -eq 1 ]; then
        c_yellow "[DRY-RUN] Schedule:"
        local w
        for w in $(seq 1 "$max_waves"); do
            echo "  Wave $w:"
            while IFS='|' read -r wnum sline; do
                [ "$wnum" != "$w" ] && continue
                local sname sdesc
                sname=$(echo "$sline" | cut -d'|' -f1)
                sdesc=$(echo "$sline" | cut -d'|' -f3)
                echo "    $sname: $sdesc"
            done < "$WAVES_FILE"
        done
        exit 0
    fi

    # -- Pre-warm npm cache for MCP servers (avoid ENOTEMPTY on concurrent npx) --
    c_blue "Pre-warming npm cache for MCP servers..."
    npm_config_prefer_offline=true npm_config_audit=false npx -y @z_ai/mcp-server --version >/dev/null 2>&1 &
    npm_config_prefer_offline=true npm_config_audit=false npx -y @playwright/mcp@latest --help >/dev/null 2>&1 &
    wait
    c_blue "npm cache ready"
    echo ""

    # -- Execute by Wave --
    local overall_start
    overall_start=$(date +%s)
    local w
    for w in $(seq 1 "$max_waves"); do
        c_blue "-------- Wave $w/$max_waves --------"

        # Collect this Wave's scenarios into a plain array first (avoid process
        # substitution issues with jobs -rp in bash 3.2)
        local wave_scenarios=()
        while IFS= read -r wline; do
            wave_scenarios+=("$wline")
        done < "$WAVES_FILE"

        local idx=0
        local total=${#wave_scenarios[@]}
        local pids=()

        for wline in "${wave_scenarios[@]}"; do
            IFS='|' read -r wnum rest <<< "$wline"
            [ "$wnum" != "$w" ] && continue
            local sid
            sid=$(echo "$rest" | cut -d'|' -f1)

            # Throttle: count running background jobs via kill -0 polling
            while true; do
                local running=0
                for p in ${pids[*]:-}; do
                    kill -0 "$p" 2>/dev/null && running=$((running + 1))
                done
                [ "$running" -lt "$MAX_PARALLEL" ] && break
                sleep 2
            done

            # Cleanup dead pids from array
            local alive_pids=()
            for p in ${pids[*]:-}; do
                kill -0 "$p" 2>/dev/null && alive_pids+=("$p")
            done
            pids=("${alive_pids[@]:-}")

            echo "  > launch $sid ..."
            run_one_scenario "$rest" &
            pids+=($!)
            idx=$((idx + 1))
        done

        # Wait for all remaining background jobs in this Wave
        for p in ${pids[*]:-}; do
            wait "$p" 2>/dev/null
        done

        c_blue "  Wave $w done ($idx scenarios)"
        echo ""
    done

    local overall_elapsed
    overall_elapsed=$(( $(date +%s) - overall_start ))

    c_blue "============================================================"
    c_blue "  All done (${overall_elapsed}s)"
    c_blue "============================================================"

    # Merge results to summary.csv
    for rf in "$REPORT_DIR"/*.result; do
        [ -f "$rf" ] || continue
        cat "$rf" >> "$REPORT_DIR/summary.csv"
    done

    # Print summary table
    echo ""
    printf "%-10s %-8s %-10s %-10s\n" "scenario" "status" "checks" "elapsed"
    printf "%-10s %-8s %-10s %-10s\n" "--------" "------" "------" "-------"
    while IFS='|' read -r sid status tpass tfail elapsed h; do
        [ "$sid" = "scenario" ] && continue
        printf "%-10s %-8s %-10s %-10s\n" "$sid" "$status" "$tpass/$((tpass+tfail))" "${elapsed}s"
    done < "$REPORT_DIR/summary.csv"

    # Stats
    local npass nfail nerr
    npass=$(grep -c '|PASS|' "$REPORT_DIR/summary.csv" 2>/dev/null)
    nfail=$(grep -c '|FAIL|' "$REPORT_DIR/summary.csv" 2>/dev/null)
    nerr=$(grep -c '|ERROR|' "$REPORT_DIR/summary.csv" 2>/dev/null)
    npass=${npass:-0}; nfail=${nfail:-0}; nerr=${nerr:-0}

    echo ""
    if [ "$nfail" -eq 0 ] && [ "$nerr" -eq 0 ]; then
        c_green "ALL PASS: $npass"
    else
        c_red "HAS FAILURES: PASS=$npass FAIL=$nfail ERROR=$nerr"
    fi

    generate_index

    echo ""
    c_blue "Reports:  $REPORT_DIR"
    c_blue "Index:    $REPORT_DIR/index.html"
    c_blue "CSV:      $REPORT_DIR/summary.csv"
}

main "$@"
