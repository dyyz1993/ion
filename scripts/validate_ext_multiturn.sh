#!/usr/bin/env bash
# validate_ext_multiturn.sh - Multi-turn dialog validation (5 parallel, --resume)
#
# Design:
#   - Each feature point = independent session with N turns of dialog
#   - Turn 1: ion --agent developer "prompt" -> get session ID
#   - Turn 2..N: ion --resume <sid> "prompt" -> continue conversation
#   - Export: ion --session <sid> --export html -> 1 HTML with full dialog history
#   - Validate: validate_html.py with --ext checks
#   - 5 features run in parallel (isolated sessions + work dirs)
#
# Usage:
#   bash scripts/validate_ext_multiturn.sh              # all features
#   bash scripts/validate_ext_multiturn.sh --dry-run    # print schedule
#   MAX_PARALLEL=3 bash scripts/validate_ext_multiturn.sh
#   bash scripts/validate_ext_multiturn.sh EXT-02       # single module
#
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

source "$PROJECT_DIR/scripts/ext_features.sh"

ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
ION_MODEL="${ION_MODEL:-glm-5.2}"
ION_PROVIDER="${ION_PROVIDER:-zai}"
CHROME="${CHROME:-/Applications/Chromium.app/Contents/MacOS/Chromium}"
MAX_PARALLEL="${MAX_PARALLEL:-5}"
REPORT_DIR="${REPORT_DIR:-/tmp/ext_multiturn_reports}"
RUN_DIR="${RUN_DIR:-/tmp/ext_multiturn_run}"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

mkdir -p "$REPORT_DIR" "$RUN_DIR"

c_red()    { printf "\033[31m%s\033[0m\n" "$*"; }
c_green()  { printf "\033[32m%s\033[0m\n" "$*"; }
c_yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
c_blue()   { printf "\033[34m%s\033[0m\n" "$*"; }

# -- Run one feature point (multi-turn dialog via --resume) --
# Args: feature_id ext_id feature_name expected_metrics turn1 turn2 ...
# Output: writes result to $REPORT_DIR/<fp_id>.result
run_feature() {
    local fp_id="$1"; shift
    local ext_id="$1"; shift
    local fp_name="$1"; shift
    local expected="$1"; shift
    local turns=("$@")

    local session_dir="$RUN_DIR/sess_${fp_id}"
    local work_dir="$RUN_DIR/work_${fp_id}"
    local log_file="$RUN_DIR/${fp_id}.log"
    local result_file="$REPORT_DIR/${fp_id}.result"
    rm -rf "$session_dir" "$work_dir"
    mkdir -p "$session_dir" "$work_dir"

    local start_ts
    start_ts=$(date +%s)

    echo "[$(date +%H:%M:%S)] > $fp_id ($ext_id) START ($fp_name, ${#turns[@]} turns)" >> "$log_file"

    local sid=""
    local turn_num=0

    for prompt in "${turns[@]}"; do
        turn_num=$((turn_num + 1))
        local prompt_file="$RUN_DIR/${fp_id}_t${turn_num}.txt"
        printf '%s' "$prompt" > "$prompt_file"

        if [ -z "$sid" ]; then
            # Turn 1: new session
            echo "[$(date +%H:%M:%S)] Turn 1: $prompt" >> "$log_file"
            (
                cd "$work_dir"
                ION_SESSION_DIR="$session_dir" \
                ION_HOST_SOCKET="$RUN_DIR/host_${fp_id}.sock" \
                ION_SKIP_MCP=1 \
                timeout 300 \
                "$ION_BIN" --agent developer \
                    --model "$ION_MODEL" --provider "$ION_PROVIDER" \
                    --profile autopilot \
                    "@$prompt_file" >> "$log_file" 2>&1
            )
            # Extract session ID from session dir
            sid=$(find "$session_dir" -name 'sess_*.jsonl' 2>/dev/null | head -1 | xargs -I{} basename {} .jsonl 2>/dev/null)
            if [ -z "$sid" ]; then
                c_red "  X $fp_id Turn 1 failed (no session)"
                echo "$fp_id|ERROR|0|0|$(( $(date +%s) - start_ts ))|" > "$result_file"
                return 1
            fi
            echo "[$(date +%H:%M:%S)] Session: $sid" >> "$log_file"
        else
            # Turn 2+: resume
            echo "[$(date +%H:%M:%S)] Turn $turn_num (resume $sid): $prompt" >> "$log_file"
            (
                cd "$work_dir"
                ION_SESSION_DIR="$session_dir" \
                ION_HOST_SOCKET="$RUN_DIR/host_${fp_id}.sock" \
                ION_SKIP_MCP=1 \
                timeout 300 \
                "$ION_BIN" --resume "$sid" \
                    --model "$ION_MODEL" --provider "$ION_PROVIDER" \
                    --profile autopilot \
                    "@$prompt_file" >> "$log_file" 2>&1
            )
        fi
    done

    local elapsed=$(( $(date +%s) - start_ts ))

    # Export HTML (full multi-turn session)
    local html_file="$REPORT_DIR/${fp_id}_${ext_id}.html"
    rm -f "$html_file"
    ION_SESSION_DIR="$session_dir" "$ION_BIN" --session "$sid" --export "$html_file" >> "$log_file" 2>&1

    if [ ! -s "$html_file" ]; then
        c_red "  X $fp_id HTML export failed"
        echo "$fp_id|ERROR|0|0|$elapsed|$ext_id|$fp_name|${#turns[@]}" > "$result_file"
        return 1
    fi

    # Validate
    local report_json="$REPORT_DIR/${fp_id}_report.json"
    python3 "$PROJECT_DIR/scripts/validate_html.py" "$html_file" \
        --chrome "$CHROME" --ext "$ext_id" \
        --session-jsonl "$(find "$session_dir" -name '*.jsonl' | head -1)" > /dev/null 2>"$report_json"

    # Judge
    local metric_status
    metric_status=$(python3 << PYEOF
import json
try:
    with open("$report_json") as f:
        d = json.load(f)
except:
    print("FAIL|0|0|")
    exit()
expected_ids = "$expected".split(",") if "$expected" else []
checks = {c["id"]: c["status"] for c in d.get("checks", [])}
miss = [eid for eid in expected_ids if checks.get(eid) != "PASS"]
total_pass = d.get("passed", 0)
total_fail = d.get("failed", 0)
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

    echo "$fp_id|$status|$tpass|$tfail|$elapsed|$ext_id|$fp_name|${#turns[@]}" > "$result_file"

    if [ "$status" = "PASS" ]; then
        c_green "  OK $fp_id PASS ($tpass/$((tpass+tfail)) ${elapsed}s ${#turns[@]}turns)"
    else
        c_red "  XX $fp_id FAIL ($tpass/$((tpass+tfail)) missed=$missed ${elapsed}s)"
    fi

    rm -rf "$session_dir"
}

# -- Main --
main() {
    DRY_RUN=0
    FILTER_EXT=""
    for arg in "$@"; do
        case "$arg" in
            --dry-run) DRY_RUN=1 ;;
            EXT-*) FILTER_EXT="$arg" ;;
        esac
    done

    # Collect all feature points
    local all_fps=()
    while IFS= read -r line; do
        [ -n "$line" ] && all_fps+=("$line")
    done < <(get_all_features)

    # Filter by ext if specified (field 2 is EXT-XX)
    if [ -n "$FILTER_EXT" ]; then
        local filtered=()
        for fp in "${all_fps[@]:-}"; do
            [ -z "$fp" ] && continue
            local fp_ext=$(echo "$fp" | cut -d'|' -f2)
            [ "$fp_ext" = "$FILTER_EXT" ] && filtered+=("$fp")
        done
        [ ${#filtered[@]:-0} -gt 0 ] && all_fps=("${filtered[@]}") || all_fps=()
    fi

    local total=${#all_fps[@]}
    c_blue "============================================================"
    c_blue "  ION Multi-Turn Dialog Validation"
    c_blue "============================================================"
    c_blue "  features: $total"
    c_blue "  model:    $ION_MODEL / $ION_PROVIDER"
    c_blue "  parallel: $MAX_PARALLEL"
    c_blue "  reports:  $REPORT_DIR"
    c_blue ""

    if [ $DRY_RUN -eq 1 ]; then
        c_yellow "[DRY-RUN] Feature schedule:"
        for fp in "${all_fps[@]}"; do
            IFS='|' read -r fp_id ext fp_name expected turns_count <<< "$fp"
            echo "  $fp_id ($ext): $fp_name ($turns_count turns)"
        done
        exit 0
    fi

    rm -f "$REPORT_DIR"/*.result "$REPORT_DIR"/*.html "$REPORT_DIR"/summary.csv 2>/dev/null

    local overall_start
    overall_start=$(date +%s)
    local pids=()

    for fp in "${all_fps[@]}"; do
        IFS='|' read -r fp_id ext fp_name expected turns_str <<< "$fp"
        IFS='~' read -ra turns <<< "$turns_str"

        # Throttle
        while true; do
            local running=0
            for p in ${pids[*]:-}; do
                kill -0 "$p" 2>/dev/null && running=$((running + 1))
            done
            [ "$running" -lt "$MAX_PARALLEL" ] && break
            sleep 2
        done

        # Cleanup dead pids
        local alive=()
        for p in ${pids[*]:-}; do
            kill -0 "$p" 2>/dev/null && alive+=("$p")
        done
        pids=("${alive[@]:-}")

        echo "  > launch $fp_id..."
        run_feature "$fp_id" "$ext" "$fp_name" "$expected" "${turns[@]}" &
        pids+=($!)
    done

    # Wait all
    for p in ${pids[*]:-}; do
        wait "$p" 2>/dev/null
    done

    local elapsed=$(( $(date +%s) - overall_start ))
    c_blue "============================================================"
    c_blue "  All done (${elapsed}s)"
    c_blue "============================================================"

    # Summary
    local npass=0 nfail=0 nerr=0
    for rf in "$REPORT_DIR"/*.result; do
        [ -f "$rf" ] || continue
        local s=$(cut -d'|' -f2 "$rf")
        case "$s" in
            PASS) npass=$((npass+1)) ;;
            FAIL) nfail=$((nfail+1)) ;;
            ERROR) nerr=$((nerr+1)) ;;
        esac
    done

    echo ""
    printf "%-12s %-8s %-10s %-8s %-8s\n" "feature" "status" "checks" "elapsed" "turns"
    printf "%-12s %-8s %-10s %-8s %-8s\n" "-------" "------" "------" "-------" "-----"
    for rf in "$REPORT_DIR"/*.result; do
        [ -f "$rf" ] || continue
        IFS='|' read -r fp_id status tpass tfail elapsed ext name nturns <<< "$(cat "$rf")"
        printf "%-12s %-8s %-10s %-8s %-8s\n" "$fp_id" "$status" "$tpass/$((tpass+tfail))" "${elapsed}s" "${nturns}t"
    done

    echo ""
    if [ "$nfail" -eq 0 ] && [ "$nerr" -eq 0 ]; then
        c_green "ALL PASS: $npass"
    else
        c_red "PASS=$npass FAIL=$nfail ERROR=$nerr"
    fi

    # Generate summary report
    python3 "$PROJECT_DIR/scripts/generate_summary_report.py" --report-dir "$REPORT_DIR" 2>/dev/null || true

    echo ""
    c_blue "Reports:  $REPORT_DIR"
    c_blue "Index:    $REPORT_DIR/index.html"
}

main "$@"
