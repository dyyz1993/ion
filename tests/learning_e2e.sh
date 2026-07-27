#!/usr/bin/env bash
#
# learning_e2e_local.sh — E2E test for Learning Extension Phase 3 (Skill Distillation)
#
# Runs ION locally with a directive prompt that REQUIRES using write/edit tools.
# After session ends, verifies ~/.ion/agent/skills/distilled-*.md was created.
#
set -uo pipefail

cd "$(dirname "$0")/.."

# Fresh test dir
TEST_DIR="/tmp/learning-e2e-$$"
mkdir -p "$TEST_DIR/src"
cd "$TEST_DIR"
echo "# Test Project" > README.md

# Snapshot current skill files (to detect new ones)
SKILLS_DIR="$HOME/.ion/agent/skills"
mkdir -p "$SKILLS_DIR"
BEFORE_COUNT=$(ls "$SKILLS_DIR"/distilled-*.md 2>/dev/null | wc -l | tr -d ' ')

echo ""
echo "=========================================="
echo "  Learning Extension E2E (Local)"
echo "  Test dir: $TEST_DIR"
echo "  Skills before: $BEFORE_COUNT"
echo "=========================================="
echo ""

# Run a session with a SUBSTANTIVE multi-step task that warrants skill extraction.
# The skill distillation LLM is instructed to refuse trivial sessions.
PROMPT='You are setting up a small Rust CLI project. Perform these steps in order using the appropriate tools (read, write, edit, bash):

1. Create a Cargo.toml at the project root with package name "demo-cli", edition 2021, and a binary target pointing to src/main.rs.
2. Use the write tool to create src/main.rs containing a Rust program that:
   - Defines a struct Config with fields: input_path (String), verbose (bool)
   - Implements a fn parse_args(args: &[String]) -> Result<Config, String> that parses --input and --verbose flags
   - Has a main() that calls parse_args, prints the parsed config, and exits 0 on success / 1 on parse error
3. Use the bash tool to run `cargo build` and verify the project compiles.
4. If compilation succeeds, edit README.md to add a Usage section documenting the CLI flags (--input, --verbose) with an example invocation.
5. Summarize the steps you took at the end.

This is a multi-step task — actually invoke each tool. Do not just describe what you would do.'

echo "[Step 1] Running ion session with write+edit directive..."
echo ""
ION_BIN="$HOME/Project/study-rust/ion/target/debug/ion"
"$ION_BIN" --provider zai --model glm-5.2 -p "$PROMPT" 2>&1 | grep -v "command not found" | tail -30
echo ""
echo "[Step 2] Verifying files were written..."
ls -la "$TEST_DIR/src/main.rs" 2>&1 | head -1
ls -la "$TEST_DIR/README.md" 2>&1 | head -1
echo "README content:"
cat "$TEST_DIR/README.md" | head -10

echo ""
echo "[Step 3] Checking for distilled skills..."
AFTER_COUNT=$(ls "$SKILLS_DIR"/distilled-*.md 2>/dev/null | wc -l | tr -d ' ')
echo "Skills before: $BEFORE_COUNT"
echo "Skills after:  $AFTER_COUNT"
echo ""
echo "--- New distilled skills (if any) ---"
for f in "$SKILLS_DIR"/distilled-*.md; do
    [ -f "$f" ] || { echo "(none)"; continue; }
    # Show only files newer than the test start
    if [ "$BEFORE_COUNT" -lt "$AFTER_COUNT" ]; then
        echo "=== $f ==="
        head -25 "$f"
        echo "..."
        echo ""
    fi
done

echo ""
if [ "$AFTER_COUNT" -gt "$BEFORE_COUNT" ]; then
    echo "✅ E2E PASSED: skill distillation fired, $((AFTER_COUNT - BEFORE_COUNT)) new skill(s) created"
    exit 0
else
    echo "⚠️  No new skills — session may not have ended cleanly, or LLM declined NO_SKILL."
    echo ""
    echo "Diagnostic: did on_session_shutdown fire? Check ~/.ion/agent/last_session for session_id."
    LAST_SID=$(cat "$HOME/.ion/agent/last_session" 2>/dev/null)
    echo "Last session: $LAST_SID"
    if [ -n "$LAST_SID" ]; then
        echo "Session file lines:"
        wc -l "$HOME/.ion/agent/sessions/$LAST_SID.jsonl" 2>/dev/null
    fi
    exit 2
fi
