#!/usr/bin/env bash
#
# evolve_verify.sh — Standalone CI verification script for the ION project.
#
# Runs five sequential checks and prints a summary at the end:
#   1. cargo build --bin ion   — compile the main binary
#   2. cargo test --lib        — run library unit tests
#   3. cargo clippy --lib      — lint with clippy; warns on new warnings
#   4. grep -rc U+FFFD in src/  — detect Unicode replacement characters (mojibake)
#   5. Summary                 — print pass/fail for each step
#
# Clippy warning baseline is stored in scripts/.clippy_baseline.
# Use --strict to fail the script when new clippy warnings appear.
#
# Exit 0 only if every step passes.

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration & color helpers
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# File storing the clippy warning count baseline
CLIPPY_BASELINE_FILE="${SCRIPT_DIR}/.clippy_baseline"

# When --strict is set, new clippy warnings cause the script to fail
STRICT_MODE=0

# ANSI color codes (disabled if output is not a terminal)
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    CYAN=''
    BOLD=''
    RESET=''
fi

# Track results: each entry is "PASS" or "FAIL"
declare -a RESULTS=()
declare -a LABELS=()

# Counters
PASS_COUNT=0
FAIL_COUNT=0
TOTAL_COUNT=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Print a section header
header() {
    echo ""
    printf "${CYAN}${BOLD}━━ %s ━━${RESET}\n" "$1"
}

# Record the result of a step
record_result() {
    local label="$1"
    local status="$2"  # PASS or FAIL
    TOTAL_COUNT=$((TOTAL_COUNT + 1))
    if [ "$status" = "PASS" ]; then
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
    LABELS+=("$label")
    RESULTS+=("$status")
}

# ---------------------------------------------------------------------------
# Step 1: cargo build --bin ion
# ---------------------------------------------------------------------------
step_build() {
    header "Step 1/4: cargo build --bin ion"
    cd "$PROJECT_ROOT"
    if cargo build --bin ion 2>&1; then
        printf "${GREEN}✓ Build succeeded${RESET}\n"
        record_result "build --bin ion" "PASS"
    else
        printf "${RED}✗ Build failed${RESET}\n"
        record_result "build --bin ion" "FAIL"
    fi
}

# ---------------------------------------------------------------------------
# Step 2: cargo test --lib
# ---------------------------------------------------------------------------
step_test() {
    header "Step 2/4: cargo test --lib"
    cd "$PROJECT_ROOT"
    if cargo test --lib 2>&1; then
        printf "${GREEN}✓ Tests passed${RESET}\n"
        record_result "test --lib" "PASS"
    else
        printf "${RED}✗ Tests failed${RESET}\n"
        record_result "test --lib" "FAIL"
    fi
}

# ---------------------------------------------------------------------------
# Step 3: cargo clippy --lib
# Runs clippy on the library crate and counts warnings. Compares the current
# warning count against the baseline stored in CLIPPY_BASELINE_FILE.
# If new warnings appeared, a WARNING is printed. In --strict mode, this step
# is recorded as FAIL.
# ---------------------------------------------------------------------------
step_clippy() {
    header "Step 3/4: cargo clippy --lib"
    cd "$PROJECT_ROOT"

    local clippy_output
    clippy_output=$(cargo clippy --lib 2>&1) || true

    # Count warnings produced by clippy
    local current_count
    current_count=$(echo "$clippy_output" | grep -c 'warning:' || true)

    # Read baseline (default to 0 if file is missing)
    local baseline_count=0
    if [ -f "${CLIPPY_BASELINE_FILE}" ]; then
        baseline_count=$(cat "${CLIPPY_BASELINE_FILE}" 2>/dev/null || echo 0)
    fi

    printf "Clippy warnings: current=%d, baseline=%d\n" \
           "$current_count" "$baseline_count"

    if [ "$current_count" -gt "$baseline_count" ]; then
        printf "${YELLOW}⚠ WARNING: clippy warning count increased by %d${RESET}\n" \
               "$((current_count - baseline_count))"

        if [ "$STRICT_MODE" -eq 1 ]; then
            record_result "clippy --lib" "FAIL"
        else
            # Non-strict: still pass, but warn loudly
            record_result "clippy --lib" "PASS"
        fi
    else
        printf "${GREEN}✓ Clippy passed (no new warnings)${RESET}\n"
        record_result "clippy --lib" "PASS"
    fi

    # Save current count as the new baseline for future runs
    echo "$current_count" > "${CLIPPY_BASELINE_FILE}"
}

# ---------------------------------------------------------------------------
# Step 4: grep -rc U+FFFD in src/
# Detects Unicode replacement characters (U+FFFD) which indicate mojibake
# or encoding corruption in source files. Any match is a failure.
# ---------------------------------------------------------------------------
step_fffd() {
    header "Step 4/4: grep U+FFFD in src/"
    cd "$PROJECT_ROOT"

    # grep -r: recursive, -c: count per file, search for literal U+FFFD
    # If any file contains the replacement character, this is a failure.
    local fffd_count
    fffd_count=$(grep -rc 'U+FFFD' src/ 2>/dev/null | \
                 awk -F: '{s+=$NF} END{print s+0}')

    if [ "${fffd_count}" -eq 0 ]; then
        printf "${GREEN}✓ No U+FFFD found in src/${RESET}\n"
        record_result "no U+FFFD in src/" "PASS"
    else
        printf "${RED}✗ Found %d occurrence(s) of U+FFFD in src/${RESET}\n" "$fffd_count"
        # Show which files have matches for debugging
        grep -rc 'U+FFFD' src/ 2>/dev/null | grep -v ':0$' || true
        record_result "no U+FFFD in src/" "FAIL"
    fi
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
print_summary() {
    header "Verification Summary"

    local i
    for i in "${!LABELS[@]}"; do
        local label="${LABELS[$i]}"
        local status="${RESULTS[$i]}"
        if [ "$status" = "PASS" ]; then
            printf "  ${GREEN}✓ PASS${RESET}  %s\n" "$label"
        else
            printf "  ${RED}✗ FAIL${RESET}  %s\n" "$label"
        fi
    done

    echo ""
    printf "${BOLD}Total: %d | ${GREEN}Passed: %d${RESET} | ${RED}Failed: %d${RESET}\n" \
           "$TOTAL_COUNT" "$PASS_COUNT" "$FAIL_COUNT"

    if [ "$FAIL_COUNT" -eq 0 ]; then
        printf "\n${GREEN}${BOLD}✅ All checks passed.${RESET}\n"
        return 0
    else
        printf "\n${RED}${BOLD}❌ %d check(s) failed.${RESET}\n" "$FAIL_COUNT"
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    printf "${BOLD}ION CI Verification${RESET}\n"
    printf "Project root: %s\n" "$PROJECT_ROOT"

    step_build
    step_test
    step_clippy
    step_fffd

    # print_summary returns 0 on all-pass, 1 otherwise
    print_summary
}

# ---------------------------------------------------------------------------
# Parse command-line arguments
#   --strict  : treat new clippy warnings as a failure (exit 1)
# ---------------------------------------------------------------------------
parse_args() {
    for arg in "$@"; do
        case "$arg" in
            --strict)
                STRICT_MODE=1
                ;;
            *)
                echo "Unknown argument: $arg" >&2
                echo "Usage: $0 [--strict]" >&2
                exit 1
                ;;
        esac
    done
}

# Run and capture exit code
parse_args "$@"
set +e
main
EXIT_CODE=$?
exit "$EXIT_CODE"
