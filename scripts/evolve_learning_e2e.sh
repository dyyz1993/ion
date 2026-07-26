#!/usr/bin/env bash
#
# evolve_learning_e2e.sh  A→B E2E test for Learning Extension Phase 3 (Skill Distillation)
#
# Flow:
#   1. Start container B with ION source (with latest Phase 3 changes)
#   2. Wait for ion binary to build
#   3. Run a session that does write operations (creates a file)
#   4. Let the session end normally (triggers on_session_shutdown -> skill distillation)
#   5. Verify ~/.ion/agent/skills/distilled-*.md exists with provenance header
#
# Usage:
#   bash scripts/evolve_learning_e2e.sh
#
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
IMAGE="${EVOLVE_IMAGE:-ion-evolve-rust:latest}"
CONTAINER_NAME="ion-learning-e2e-$$"

echo ""
echo "=========================================="
echo "  Learning Extension E2E (A→B Pattern)"
echo "=========================================="
echo ""

# ─── Step 1: Start container ───────────────────────────────────────────────
echo "[Step 1] Starting container $CONTAINER_NAME ..."
"$CONTAINER_BIN" run -d \
    --name "$CONTAINER_NAME" \
    -v ion-cargo-cache:/root/.cargo \
    -v ion-target-cache:/workspace/target \
    "$IMAGE" \
    sleep 3600 2>&1 | grep -v "command not found"
trap "[ -n \"$CONTAINER_NAME\" ] && $CONTAINER_BIN stop $CONTAINER_NAME 2>/dev/null" EXIT

sleep 5
"$CONTAINER_BIN" ps 2>&1 | grep "$CONTAINER_NAME" | head -1

# ─── Step 2: Copy latest source into container ────────────────────────────
echo ""
echo "[Step 2] Copying latest ION source (with Phase 3) into container..."
tar -C "$PROJECT_DIR" \
    --exclude='target' --exclude='.git' --exclude='node_modules' \
    -cf - src/ ion-provider/ Cargo.toml Cargo.lock 2>/dev/null \
    | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" \
        sh -c 'mkdir -p /workspace && cd /workspace && tar xf -'
echo "    source copied"

# ─── Step 3: Build ion binary in container ────────────────────────────────
echo ""
echo "[Step 3] Building ion binary (may take 2-3 min on incremental)..."
"$CONTAINER_BIN" exec "$CONTAINER_NAME" \
    sh -c 'cd /workspace && cargo build --bin ion 2>&1 | tail -5'

BUILD_STATUS=$?
if [ "$BUILD_STATUS" -ne 0 ]; then
    echo "    ❌ BUILD FAILED"
    exit 1
fi
echo "    ✅ build ok"

# ─── Step 4: Run a session with write operations ──────────────────────────
echo ""
echo "[Step 4] Running a session that writes files (should trigger skill distillation)..."

# Set up ion config in container — reuse host's actual config.json + auth.json
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c '
mkdir -p /root/.ion/agent/skills
# Clean any previous distilled skills (fresh test)
rm -f /root/.ion/agent/skills/distilled-*.md 2>/dev/null
'

# Copy host config/auth into container (preserve permissions)
cp "$HOME/.ion/config.json" /tmp/_ion_config.json 2>/dev/null
cp "$HOME/.ion/auth.json" /tmp/_ion_auth.json 2>/dev/null
if [ -f /tmp/_ion_config.json ]; then
    tar -cf - -C /tmp _ion_config.json _ion_auth.json 2>/dev/null \
        | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" \
            sh -c "cd /root/.ion && tar xf - && mv _ion_config.json config.json 2>/dev/null; mv _ion_auth.json auth.json 2>/dev/null; chmod 600 auth.json; chmod 644 config.json"
fi

# Show what provider will be used (without leaking secrets)
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c '
python3 -c "import json; c=json.load(open(\"/root/.ion/config.json\")); print(\"Provider:\", c.get(\"default_provider\"), \"| Model:\", c.get(\"default_model\"))" 2>/dev/null || cat /root/.ion/config.json
'

# Create a test working directory
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'mkdir -p /tmp/test-skill-distill && cd /tmp/test-skill-distill && cat > README.md <<EOF
# Test Project
A minimal project to trigger skill distillation.
EOF
'

# Run the actual ion session — ask it to do real file edits
echo "    → launching ion session with file-edit task..."
"$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c '
cd /tmp/test-skill-distill
/workspace/target/debug/ion --print "Add a Rust hello world file at src/main.rs, then edit README.md to document it. Make sure both files compile. Then summarize what you did."
' 2>&1 | grep -v "command not found" | tail -40

echo ""
echo "[Step 5] Checking for distilled skill files..."
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c '
echo "--- skills dir contents ---"
ls -la /root/.ion/agent/skills/ 2>/dev/null
echo ""
echo "--- distilled skill contents (if any) ---"
for f in /root/.ion/agent/skills/distilled-*.md; do
    [ -f "$f" ] || { echo "(no distilled skills found)"; continue; }
    echo "=== $f ==="
    head -20 "$f"
    echo "..."
    echo ""
done
'

# ─── Step 6: Result analysis ──────────────────────────────────────────────
echo ""
echo "[Step 6] Analysis..."
SKILL_COUNT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'ls /root/.ion/agent/skills/distilled-*.md 2>/dev/null | wc -l')
SKILL_COUNT=$(echo "$SKILL_COUNT" | tr -d ' \r\n')
echo "    Distilled skills created: $SKILL_COUNT"

if [ "$SKILL_COUNT" -gt 0 ]; then
    echo ""
    echo "✅ E2E PASSED: skill distillation triggered, $SKILL_COUNT skill file(s) created"
    exit 0
else
    echo ""
    echo "⚠️  No skill files created — check session transcript above."
    echo "   Possible causes:"
    echo "     - LLM declined (NO_SKILL) — session was too trivial"
    echo "     - analyze_session returned should_distill_skill=false"
    echo "     - on_session_shutdown didn't fire (session didn't exit cleanly)"
    exit 2
fi
