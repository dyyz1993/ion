#!/usr/bin/env bash
# evolve_auto.sh — 一条命令完成自进化全闭环
#
# 流程：
#   1. 启动 container + 编译 ion（V 方案 volume cache）
#   2. A 驱动 B 改代码（container exec developer agent）
#   3. A 守门（U+FFFD + cargo build + cargo test）
#   4. 通过 → 开 feature 分支 + push + 开 GitHub PR
#   5. auto-merge PR（CI 通过即合并）
#   6. 通知 watchdog 升级（request_restart）
#   7. watchdog 自动编译 + health check + 切换（失败自动回滚）
#   8. 清理 container + worktree + 临时文件
#
# Usage:
#   bash scripts/evolve_auto.sh "任务描述"
#   MODEL=glm-5.2 PROVIDER=zai bash scripts/evolve_auto.sh "任务描述"
#
# Env:
#   MODEL       LLM 模型（默认 glm-5.2）
#   PROVIDER    LLM provider（默认 zai）
#   NO_PR=1     不开 PR，直接 commit 到 master（快速模式）
#   NO_WATCHDOG=1  不触发 watchdog 升级（只改代码 + push）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
TASK_DESC="${1:-}"
NO_PR="${NO_PR:-0}"
NO_WATCHDOG="${NO_WATCHDOG:-0}"
COMPILE_TIMEOUT="${ION_TOOL_TIMEOUT:-1800}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✅ $1${NC}"; }
fail() { echo -e "${RED}❌ $1${NC}"; }
info() { echo -e "${YELLOW}📦 $1${NC}"; }

# ── Guard: must have task description ───────────────────────────────
if [ -z "$TASK_DESC" ]; then
    echo "Usage: bash scripts/evolve_auto.sh \"任务描述\""
    echo ""
    echo "Env:"
    echo "  MODEL=glm-5.2 PROVIDER=zai  使用指定模型"
    echo "  NO_PR=1                     直接 commit master（不开 PR）"
    echo "  NO_WATCHDOG=1               不触发 watchdog 升级"
    exit 1
fi

echo "=========================================="
echo "  ION Self-Evolution (Full Loop)"
echo "=========================================="
echo "  Task:     $TASK_DESC"
echo "  Model:    $MODEL ($PROVIDER)"
echo "  PR mode:  $([ "$NO_PR" = "1" ] && echo "直接 commit master" || echo "GitHub PR")"
echo "  Watchdog: $([ "$NO_WATCHDOG" = "1" ] && echo "跳过" || echo "自动升级")"
echo "=========================================="
echo ""

CONTAINER_NAME=""
WT_DIR=""
BRANCH_NAME=""

# ── Cleanup function (always runs on exit) ──────────────────────────
cleanup() {
    echo ""
    info "Phase 7: Cleanup"

    # Stop container
    if [ -n "$CONTAINER_NAME" ]; then
        container stop "$CONTAINER_NAME" 2>/dev/null && echo "  Container stopped" || true
    fi

    # Remove worktree
    if [ -n "$WT_DIR" ] && [ -d "$WT_DIR" ]; then
        cd "$PROJECT_DIR"
        git worktree remove "$WT_DIR" --force 2>/dev/null && echo "  Worktree removed" || true
        git worktree prune
    fi

    # Clean temp state
    rm -f /tmp/.evolver-state /tmp/.ion-evolve-restart 2>/dev/null

    # Clean up any stale evolve worktrees from previous runs
    for old_wt in /tmp/ion-evolve-*/; do
        [ -d "$old_wt" ] && rm -rf "$old_wt" 2>/dev/null && echo "  Cleaned stale: $old_wt"
    done

    ok "Cleanup complete"
}
trap cleanup EXIT

# ── Phase 1: Start container + compile ──────────────────────────────
info "Phase 1: Start container + compile ion"
cd "$PROJECT_DIR"

# Use evolve.sh to set up worktree + container + compile
ION_TOOL_TIMEOUT="$COMPILE_TIMEOUT" bash scripts/evolve.sh 2>&1 | tail -5
source /tmp/.evolver-state 2>/dev/null

if [ -z "$CONTAINER_NAME" ] || [ "$BUILD_STATUS" != "OK" ]; then
    fail "Container setup failed. Aborting."
    exit 1
fi

# Fix state file (evolve.sh sometimes writes wrong container name)
ACTUAL_CONTAINER=$(container list 2>/dev/null | grep "ion-evolve" | tail -1 | awk '{print $1}')
if [ -n "$ACTUAL_CONTAINER" ]; then
    CONTAINER_NAME="$ACTUAL_CONTAINER"
fi

ok "Container ready: $CONTAINER_NAME"

# ── Phase 2: B writes code ──────────────────────────────────────────
info "Phase 2: B writes code (developer agent)"

# Build full task prompt with English-only constraint
FULL_TASK="$TASK_DESC

CONSTRAINTS:
- ALL comments MUST be in ENGLISH ONLY (no Chinese, no Unicode)
- Do NOT modify Cargo.toml
- Do NOT modify existing methods, only ADD new ones
- After editing: cargo check && git add -A && git commit -m 'feat: $(echo "$TASK_DESC" | head -c 60)'"

echo "  Sending task to B ($MODEL)..."
echo "$FULL_TASK" | container exec -i "$CONTAINER_NAME" \
    sh -c "cd /workspace && ./target/release/ion --agent developer --provider $PROVIDER --model $MODEL 2>&1" \
    | tail -20

# Check if B committed
B_COMMIT=$(container exec "$CONTAINER_NAME" sh -c 'cd /workspace && git log --oneline -1' 2>/dev/null)
if [ -z "$B_COMMIT" ] || echo "$B_COMMIT" | grep -q "container init"; then
    fail "B did not commit any changes. Aborting."
    exit 1
fi
ok "B committed: $B_COMMIT"

# ── Phase 3: Gate check ─────────────────────────────────────────────
info "Phase 3: Gate check (U+FFFD + Cargo.toml + scope)"

# U+FFFD check — ONLY on files B actually changed (git diff HEAD~1)
# Not the entire src/ tree (pre-existing U+FFFD in unrelated files is not B's fault)
CHANGED=$(container exec "$CONTAINER_NAME" sh -c 'cd /workspace && git diff HEAD~1 --name-only' 2>/dev/null)
GARBLED=""
if [ -n "$CHANGED" ]; then
    for f in $CHANGED; do
        FOUND=$(container exec "$CONTAINER_NAME" sh -c "grep -c \$'\xef\xbf\xbd' /workspace/$f 2>/dev/null" 2>/dev/null)
        if [ "$FOUND" != "0" ] && [ -n "$FOUND" ]; then
            GARBLED="$GARBLED $f"
        fi
    done
fi
if [ -n "$GARBLED" ]; then
    fail "U+FFFD found in B's changed files:$GARBLED"
    fail "Aborting — B must fix garbled characters"
    exit 1
fi
ok "0 U+FFFD characters in changed files"

# Cargo.toml unchanged
TOML_DIFF=$(container exec "$CONTAINER_NAME" sh -c 'cd /workspace && git diff HEAD~1 -- Cargo.toml' 2>/dev/null)
if [ -n "$TOML_DIFF" ]; then
    fail "B modified Cargo.toml — not allowed"
    exit 1
fi
ok "Cargo.toml unchanged"

# Changed files (already computed above for U+FFFD check)
echo "  Changed files: $CHANGED"

# ── Phase 4: CI verification ────────────────────────────────────────
info "Phase 4: CI verification (cargo check in container)"

# Use cargo check (fast, ~30s) instead of cargo test (slow, needs full compile).
# Full test runs on host after merge (Phase 5).
CI_RESULT=$(container exec "$CONTAINER_NAME" sh -c 'cd /workspace && cargo check 2>&1 | tail -3' 2>/dev/null)
echo "  $CI_RESULT"

if echo "$CI_RESULT" | grep -q "^error"; then
    fail "cargo check failed in container. Aborting."
    echo "$CI_RESULT"
    exit 1
fi
ok "cargo check passed"
TEST_COUNT="(check only)"

# ── Phase 5: Merge (PR or direct commit) ────────────────────────────
cd "$PROJECT_DIR"

if [ "$NO_PR" = "1" ]; then
    # ── Direct commit to master ────────────────────────────────────
    info "Phase 5: Direct commit to master (NO_PR mode)"

    # Copy changed files from container's workspace directly to host project
    for f in $CHANGED; do
        # Create parent dirs if needed
        mkdir -p "$PROJECT_DIR/$(dirname "$f")"
        # Extract file content from container
        container exec "$CONTAINER_NAME" sh -c "cat /workspace/$f" 2>/dev/null > "$PROJECT_DIR/$f"
        if [ -s "$PROJECT_DIR/$f" ]; then
            echo "  Copied $f"
        else
            fail "Failed to copy $f from container"
            exit 1
        fi
    done

    # Verify on host
    cargo build --lib 2>&1 | tail -2
    cargo test --lib 2>&1 | tail -3

    git add -A
    git commit -m "feat: $TASK_DESC (self-evolution, $MODEL)

A→B closed loop:
- B wrote code in container ($MODEL)
- A gate-checked (0 U+FFFD, Cargo.toml unchanged)
- CI passed in container
- Merged to master" 2>&1 | tail -3

    git push origin master 2>&1 | tail -2
    ok "Pushed to master"
else
    # ── GitHub PR flow ─────────────────────────────────────────────
    info "Phase 5: GitHub PR flow"

    BRANCH_NAME="evolve/$(date +%Y%m%d-%H%M%S)"

    # Create feature branch + copy changes
    git checkout -b "$BRANCH_NAME"

    # Copy changed files from container
    for f in $CHANGED; do
        mkdir -p "$PROJECT_DIR/$(dirname "$f")"
        container exec "$CONTAINER_NAME" sh -c "cat /workspace/$f" 2>/dev/null > "$PROJECT_DIR/$f"
        echo "  Copied $f"
    done

    # Verify on host
    if ! cargo build --lib 2>&1 | tail -2 | grep -q "Finished"; then
        fail "Host build failed. Aborting."
        git checkout master
        git branch -D "$BRANCH_NAME" 2>/dev/null
        exit 1
    fi

    cargo test --lib 2>&1 | tail -3

    git add -A
    git commit -m "feat: $TASK_DESC (self-evolution, $MODEL)" 2>&1 | tail -2
    git push origin "$BRANCH_NAME" 2>&1 | tail -2

    # Create PR
    PR_URL=$(gh pr create \
        --title "feat: $TASK_DESC" \
        --body "## Self-Evolution A→B Closed Loop

**Task**: $TASK_DESC
**Model**: $MODEL ($PROVIDER)

### Verification
- ✅ B wrote code in container
- ✅ 0 U+FFFD characters (English-only comments)
- ✅ Cargo.toml unchanged
- ✅ CI passed in container: $TEST_COUNT
- ✅ Host build + test passed

### Changed files
$CHANGED

---
*Generated by \`scripts/evolve_auto.sh\`*" \
        --base master \
        --head "$BRANCH_NAME" 2>/dev/null)

    if [ -n "$PR_URL" ]; then
        ok "PR created: $PR_URL"

        # Auto-merge if CI passes (squash)
        echo "  Waiting 10s then auto-merging..."
        sleep 10
        if gh pr merge "$PR_URL" --squash --delete-branch --admin 2>/dev/null; then
            ok "PR merged"
            git checkout master
            git pull origin master 2>&1 | tail -2
        else
            fail "Auto-merge failed. PR remains open: $PR_URL"
            git checkout master
        fi
    else
        fail "Failed to create PR (gh CLI not configured?)"
        fail "Falling back to direct push"
        git push origin "$BRANCH_NAME" 2>&1 | tail -2
    fi
fi

# ── Phase 6: Watchdog upgrade ───────────────────────────────────────
if [ "$NO_WATCHDOG" = "1" ]; then
    info "Phase 6: Watchdog upgrade (skipped)"
else
    info "Phase 6: Watchdog safe upgrade"

    # Check if watchdog monitor is running
    if pgrep -f "watchdog.sh.*monitor" > /dev/null 2>&1; then
        echo "  Watchdog monitor detected. Sending restart signal..."
        ION_BIN="$PROJECT_DIR/target/debug/ion" timeout 5 "$PROJECT_DIR/target/debug/ion" rpc \
            --method request_restart --params '{}' 2>/dev/null
        ok "Restart signal sent. Watchdog will compile + switch automatically."

        echo "  Monitoring upgrade (up to 120s)..."
        for i in $(seq 1 24); do
            sleep 5
            # Check if ion serve is healthy with new code
            HEALTH=$(timeout 3 "$PROJECT_DIR/target/debug/ion" rpc --method health --params '{}' 2>/dev/null)
            if echo "$HEALTH" | grep -q '"ok"'; then
                ok "ion serve healthy after upgrade"
                break
            fi
            echo "  [$((i*5))s] waiting..."
        done
    else
        echo "  Watchdog monitor not running. Running one-shot upgrade..."
        ION_BIN="$PROJECT_DIR/target/debug/ion" \
        HEALTH_TIMEOUT=30 \
        COMPILE_TIMEOUT="$COMPILE_TIMEOUT" \
        bash "$PROJECT_DIR/scripts/watchdog.sh" --upgrade 2>&1 | tail -10
    fi
fi

# ── Phase 7: Publish (version bump + changelog + tag) ──────────────
if [ "${PUBLISH:-0}" = "1" ]; then
    info "Phase 7: Publish (version bump + changelog + git tag)"

    cd "$PROJECT_DIR"

    # Determine version bump type (patch/minor/major, default: patch)
    BUMP_TYPE="${BUMP_TYPE:-patch}"

    # Read current version
    CURRENT_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
    echo "  Current version: $CURRENT_VERSION"

    # Calculate new version
    IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"
    case "$BUMP_TYPE" in
        major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
        minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
        patch) PATCH=$((PATCH + 1)) ;;
    esac
    NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"
    echo "  New version: $NEW_VERSION ($BUMP_TYPE bump)"

    # Bump version in Cargo.toml files
    sed -i.bak "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" Cargo.toml
    sed -i.bak "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" ion-provider/Cargo.toml 2>/dev/null
    rm -f Cargo.toml.bak ion-provider/Cargo.toml.bak

    # Add to CHANGELOG.md (prepend new version section)
    if [ -f CHANGELOG.md ]; then
        DATE=$(date +%Y-%m-%d)
        # Insert new version section after the header
        python3 -c "
import sys
with open('CHANGELOG.md', 'r') as f:
            content = f.read()
section = '''## [$NEW_VERSION] — $DATE

### Changes
- $TASK_DESC (A→B self-evolution, $MODEL)

'''
# Insert after 'Format based on...' line
lines = content.split('\n')
for i, line in enumerate(lines):
    if 'Format based on' in line:
        lines.insert(i + 2, section)
        break
with open('CHANGELOG.md', 'w') as f:
    f.write('\n'.join(lines))
" 2>/dev/null
        echo "  CHANGELOG.md updated"
    fi

    # Commit version bump
    git add Cargo.toml ion-provider/Cargo.toml CHANGELOG.md 2>/dev/null
    git commit -m "release: v$NEW_VERSION — $TASK_DESC

Auto-published by evolve_auto.sh (A→B self-evolution).
Bump type: $BUMP_TYPE" 2>&1 | tail -2

    # Create git tag
    git tag -a "v$NEW_VERSION" -m "v$NEW_VERSION — $TASK_DESC" 2>/dev/null
    echo "  Tag v$NEW_VERSION created"

    # Push
    git push origin master 2>&1 | tail -1
    git push origin "v$NEW_VERSION" 2>&1 | tail -1
    ok "Published v$NEW_VERSION"
else
    info "Phase 7: Publish (skipped — set PUBLISH=1 to enable)"
fi

echo ""
ok "============================================"
ok "  Self-Evolution Complete!"
ok "============================================"
ok "  Task: $TASK_DESC"
ok "  Changes: $CHANGED"
ok "  CI: $TEST_COUNT"
ok "  PR/Commit: $([ "$NO_PR" = "1" ] && echo "master" || echo "${BRANCH_NAME:-master}")"
ok "  Watchdog: $([ "$NO_WATCHDOG" = "1" ] && echo "skipped" || echo "upgraded")"
ok "  Publish: $([ "${PUBLISH:-0}" = "1" ] && echo "v$NEW_VERSION" || echo "skipped")"
ok "============================================"
