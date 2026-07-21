#!/usr/bin/env bash
# 
# evolve.sh  A  B 
#
#  worktree + container +  ion 
# A  bash  3 
#
# : bash scripts/evolve.sh
# :  /tmp/.evolver-stateWT_DIR + CONTAINER_NAME + BUILD_STATUS
# 
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
IMAGE="${EVOLVE_IMAGE:-ion-evolve-rust:latest}"
ION_DIR="${HOME}/.ion"

echo ""
echo "   AB "
echo ""

#  0.  worktree
echo " Step 0:  worktree "
for old_wt in /tmp/ion-evolve-*/; do
    [ -d "$old_wt" ] && rm -rf "$old_wt" && echo "   $old_wt"
done
cd "$PROJECT_DIR"
git worktree prune 2>/dev/null
echo "   "

#  1.  worktree master  HEAD
echo ""
echo " Step 1:  worktree "
WT_DIR=$(mktemp -d /tmp/ion-evolve-XXXXXX)
cd "$PROJECT_DIR"
#  master  remote
git checkout master 2>/dev/null || true
#  HEAD  worktree master  detached HEAD 
WORKTREE_BRANCH="evolve/$(date +%Y%m%d-%H%M%S)"
git worktree add "$WT_DIR" -b "$WORKTREE_BRANCH" HEAD 2>&1 | tail -2
echo "   worktree: $WT_DIR (branch: $WORKTREE_BRANCH)"

#  worktree  .git  repo
if [ -f "$WT_DIR/.git" ]; then
    rm -f "$WT_DIR/.git"
    (cd "$WT_DIR" && git init -q && git config user.email 'ion-evolver@example.com' && git config user.name 'ION Evolver' && git add -A && git commit -q -m 'container init' 2>/dev/null)
fi

#  2.  container 
echo ""
echo " Step 2:  container "
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
CONTAINER_NAME="ion-evolve-${TIMESTAMP}"

#  ion-provider
GIT_COMMON_DIR=$(cd "$PROJECT_DIR" && git rev-parse --git-common-dir 2>/dev/null || echo "")
HOST_PARENT=""
if [ -n "$GIT_COMMON_DIR" ]; then
    MAIN_REPO=$(cd "$GIT_COMMON_DIR/.." 2>/dev/null && pwd)
    HOST_PARENT=$(dirname "$MAIN_REPO")
fi
ION_PROVIDER_DIR="$HOST_PARENT/ion-provider"

CONTAINER_CMD=(
    "$CONTAINER_BIN" run
    --name "$CONTAINER_NAME"
    --detach --rm
    --network default
    -v "${WT_DIR}:/workspace"
    -w /workspace
    --memory 4G --cpus 4
)

[ -d "$ION_PROVIDER_DIR" ] && CONTAINER_CMD+=("-v" "${ION_PROVIDER_DIR}:/ion-provider")
[ -d "$ION_DIR" ] && CONTAINER_CMD+=("-v" "${ION_DIR}:/root/.ion")

CONTAINER_CMD+=("$IMAGE" sh -lc "sleep infinity")

"${CONTAINER_CMD[@]}" 2>&1 | tail -2
echo "   container: $CONTAINER_NAME"

#  Cargo.toml 
if [ -d "$ION_PROVIDER_DIR" ]; then
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "cd /workspace && sed -i 's|path = \"../ion-provider\"|path = \"/ion-provider\"|' Cargo.toml" 2>/dev/null
fi

#  3.  ion
echo ""
echo " Step 3:  ion6-15 "
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
    'source $HOME/.cargo/env && cd /workspace && cargo build --release --bin ion 2>&1 | tail -5 && touch /tmp/ion-build-done'
BUILD_EXIT=$?

if [ $BUILD_EXIT -eq 0 ]; then
    echo "   ion "
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'cd /workspace && ./target/release/ion --version' 2>&1 | head -1
else
    echo "   ion "
    cat > /tmp/.evolver-state << EOF
WT_DIR=$WT_DIR
CONTAINER_NAME=$CONTAINER_NAME
PROJECT_DIR=$PROJECT_DIR
BUILD_STATUS=FAILED
EOF
    exit 1
fi

#   
cat > /tmp/.evolver-state << EOF
WT_DIR=$WT_DIR
CONTAINER_NAME=$CONTAINER_NAME
PROJECT_DIR=$PROJECT_DIR
BUILD_STATUS=OK
EOF

echo ""
echo ""
echo "   "
echo ""
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo ""
