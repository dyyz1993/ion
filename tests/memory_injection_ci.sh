#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────
# Memory Injection CI — save → search → list → forget lifecycle
#
# Validates the full memory active injection flow against a live
# `ion serve` instance:
#   1. Start ion serve in background
#   2. save  : store a CI test memory
#   3. search: query the memory back via FTS5
#   4. verify search returned the saved memory
#   5. list  : enumerate all stored memories
#   6. forget: soft-delete the memory by id
#   7. verify list no longer contains the forgotten entry
#   8. cleanup + pass/fail report
#
# Exit code = number of failed checks (0 = all green).
# ──────────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

ION_BIN="$PROJECT_DIR/target/debug/ion"
DB_PATH="$HOME/.ion/agent/global-memory.db"
SOCK_PATH="$HOME/.ion/host.sock"

# --- pass/fail helpers (all counters & messages in English) ---
PASS=0
FAIL=0
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
pass()  { green "  ✅ PASS: $1"; ((PASS++)); }
fail()  { red   "  ❌ FAIL: $1"; ((FAIL++)); }

# Wrapper around `ion rpc extension_rpc` for the global-memory extension.
# Usage: rpc_mem <method> <args-json>
rpc_mem() {
    local method="$1"
    local args="${2:-{}}"
    "$ION_BIN" rpc \
        --method extension_rpc \
        --params "{\"extension\":\"global-memory\",\"method\":\"$method\",\"args\":$args}" \
        2>&1
}

echo "══════════════════════════════════════════════════════"
echo "  Memory Injection CI — $(date)"
echo "══════════════════════════════════════════════════════"

# ──────────────────────────────────────────────────────────────
# Phase 0: Build + ensure a clean slate
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Phase 0: Build ──"
cargo build --bin ion 2>&1 | tail -2

# Clean any leftover DB / running serve from previous runs.
rm -f "$DB_PATH"
lsof -ti "$SOCK_PATH" 2>/dev/null | xargs kill 2>/dev/null
sleep 1

# ──────────────────────────────────────────────────────────────
# Step 1: Start ion serve in the background
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 1: Start ion serve ──"
timeout 120 "$ION_BIN" serve >/tmp/mem-inject-serve.log 2>&1 &
SERVE_PID=$!
# Wait for the host socket to come up.
for _ in $(seq 1 20); do
    if [ -S "$SOCK_PATH" ]; then break; fi
    sleep 0.5
done

if "$ION_BIN" rpc --method get_state 2>/dev/null | grep -q "success"; then
    pass "serve is up (socket ready)"
else
    fail "serve failed to start"
    red "  --- serve log (tail) ---"
    tail -n 10 /tmp/mem-inject-serve.log
    exit 1
fi

# ──────────────────────────────────────────────────────────────
# Step 2: Save a memory and capture the returned id
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 2: Save a CI test memory ──"
SAVE_OUTPUT=$(rpc_mem save \
    '{"content":"CI test memory","project":"test","tags":"ci","category":"note","importance":5}')

# Memory ids look like gmem_<hex>. Extract the first one if present.
MEM_ID=$(echo "$SAVE_OUTPUT" | grep -o 'gmem_[a-f0-9]*' | head -1)

if [ -n "$MEM_ID" ]; then
    pass "save returned memory id ($MEM_ID)"
else
    fail "save did not return a gmem id"
    red "  output: $(echo "$SAVE_OUTPUT" | head -c 300)"
fi

# ──────────────────────────────────────────────────────────────
# Step 3: Search for the memory we just saved
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 3: Search for 'CI test' ──"
SEARCH_OUTPUT=$(rpc_mem search '{"query":"CI test"}')

if echo "$SEARCH_OUTPUT" | grep -q "CI test memory"; then
    pass "search executed and returned content"
else
    fail "search did not return expected content"
    red "  output: $(echo "$SEARCH_OUTPUT" | head -c 300)"
fi

# ──────────────────────────────────────────────────────────────
# Step 4: Verify the search result contains the saved memory
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 4: Verify saved memory is in search results ──"
if echo "$SEARCH_OUTPUT" | grep -q "CI test memory"; then
    pass "search results contain the saved memory ('CI test memory')"
else
    fail "search results do NOT contain the saved memory"
fi

# ──────────────────────────────────────────────────────────────
# Step 5: List all memories
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 5: List all memories ──"
LIST_OUTPUT=$(rpc_mem list '{}')

if echo "$LIST_OUTPUT" | grep -q "$MEM_ID"; then
    pass "list contains the saved memory ($MEM_ID)"
else
    fail "list does NOT contain the saved memory before forget"
    red "  output: $(echo "$LIST_OUTPUT" | head -c 300)"
fi

# ──────────────────────────────────────────────────────────────
# Step 6: Forget (soft-delete) the memory by id
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 6: Forget the memory ($MEM_ID) ──"
if [ -n "$MEM_ID" ]; then
    FORGET_OUTPUT=$(rpc_mem forget "{\"id\":\"$MEM_ID\"}")
    if echo "$FORGET_OUTPUT" | grep -qi "true\|success\|ok"; then
        pass "forget acknowledged the memory id"
    else
        fail "forget did not acknowledge"
        red "  output: $(echo "$FORGET_OUTPUT" | head -c 300)"
    fi
else
    fail "skip forget (no MEM_ID captured in step 2)"
fi

# ──────────────────────────────────────────────────────────────
# Step 7: Verify the forgotten entry no longer appears in list
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 7: Verify forgotten memory is gone from list ──"
LIST_AFTER=$(rpc_mem list '{}')

if [ -n "$MEM_ID" ]; then
    if echo "$LIST_AFTER" | grep -q "$MEM_ID"; then
        fail "list STILL contains the forgotten memory ($MEM_ID) — forget ineffective"
    else
        pass "list no longer contains the forgotten memory"
    fi
else
    fail "cannot verify forget (no MEM_ID)"
fi

# ──────────────────────────────────────────────────────────────
# Step 8: Cleanup + report
# ──────────────────────────────────────────────────────────────
echo ""
echo "── Step 8: Cleanup ──"
# Kill serve via the socket (avoid pkill which could clobber other procs).
lsof -ti "$SOCK_PATH" 2>/dev/null | xargs kill 2>/dev/null
kill "$SERVE_PID" 2>/dev/null
wait "$SERVE_PID" 2>/dev/null
rm -f "$DB_PATH"
echo "  serve stopped, DB removed"

echo ""
echo "══════════════════════════════════════════════════════"
echo "  RESULT: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════"

if [ "$FAIL" -eq 0 ]; then
    green "✅ memory_injection_ci — ALL CHECKS PASSED"
    exit 0
else
    red   "❌ memory_injection_ci — $FAIL CHECK(S) FAILED"
    exit "$FAIL"
fi
