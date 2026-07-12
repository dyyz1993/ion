#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# ION E2E 全量回归 — 一键跑全部 12 Group
# ──────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$PROJECT_DIR"

TOTAL_PASS=0; TOTAL_FAIL=0; TOTAL_SKIP=0

# 用数组避免 shell 环境变量污染
declare -a GRP_LIST=("A" "B" "C" "D" "E" "F" "G" "H" "I" "J" "K" "L")

echo "══════════════════════════════════════════════════════════"
echo "  ION E2E 全量回归 — $(date '+%Y-%m-%d %H:%M')"
echo "══════════════════════════════════════════════════════════"

for GRP in "${GRP_LIST[@]}"; do
    case $GRP in
        A) SCRIPT="tests/e2e/group_a_basic.sh" ;;
        B) SCRIPT="tests/e2e/group_b_session.sh" ;;
        C) SCRIPT="tests/e2e/group_c_tree.sh" ;;
        D) SCRIPT="tests/e2e/group_d_rpc.sh" ;;
        E) SCRIPT="tests/e2e/group_e_tools.sh" ;;
        F) SCRIPT="tests/mcp_ci.sh" ;;
        G) SCRIPT="tests/e2e/group_g_team.sh" ;;
        H) SCRIPT="tests/e2e/group_h_memory.sh" ;;
        I) SCRIPT="tests/e2e/group_i_snapshot.sh" ;;
        J) SCRIPT="tests/e2e/group_j_permission.sh" ;;
        K) SCRIPT="tests/e2e/group_k_compaction.sh" ;;
        L) SCRIPT="tests/e2e/group_l_workflow.sh" ;;
    esac

    echo ""
    echo "▶ Running Group $GRP ($SCRIPT)..."

    OUTPUT=$(bash "$SCRIPT" 2>&1)
    RESULT=$(echo "$OUTPUT" | grep "Group $GRP:" | head -1)

    if [ -z "$RESULT" ]; then
        # MCP CI 格式："结果: PASS=N FAIL=N SKIP=N"
        RESULT=$(echo "$OUTPUT" | grep "结果:" | head -1)
        if [ -n "$RESULT" ]; then
            # 转换格式
            P=$(echo "$RESULT" | grep -o 'PASS=[0-9]*' | grep -o '[0-9]*' || echo 0)
            F=$(echo "$RESULT" | grep -o 'FAIL=[0-9]*' | grep -o '[0-9]*' || echo 0)
            S=$(echo "$RESULT" | grep -o 'SKIP=[0-9]*' | grep -o '[0-9]*' || echo 0)
            TOTAL_PASS=$((TOTAL_PASS + P))
            TOTAL_FAIL=$((TOTAL_FAIL + F))
            TOTAL_SKIP=$((TOTAL_SKIP + S))
            if [ "$F" = "0" ]; then
                echo "  ✅ Group $GRP: PASS=$P FAIL=$F SKIP=$S"
            else
                echo "  ❌ Group $GRP: PASS=$P FAIL=$F SKIP=$S"
            fi
            continue
        fi
        RESULT="PASS=0 FAIL=0 SKIP=0"
    fi

    P=$(echo "$RESULT" | grep -o 'PASS=[0-9]*' | grep -o '[0-9]*' || echo 0)
    F=$(echo "$RESULT" | grep -o 'FAIL=[0-9]*' | grep -o '[0-9]*' || echo 0)
    S=$(echo "$RESULT" | grep -o 'SKIP=[0-9]*' | grep -o '[0-9]*' || echo 0)

    TOTAL_PASS=$((TOTAL_PASS + P))
    TOTAL_FAIL=$((TOTAL_FAIL + F))
    TOTAL_SKIP=$((TOTAL_SKIP + S))

    if [ "$F" = "0" ]; then
        echo "  ✅ Group $GRP: PASS=$P FAIL=$F SKIP=$S"
    else
        echo "  ❌ Group $GRP: PASS=$P FAIL=$F SKIP=$S"
    fi
done

echo ""
echo "══════════════════════════════════════════════════════════"
echo "  总计: PASS=$TOTAL_PASS  FAIL=$TOTAL_FAIL  SKIP=$TOTAL_SKIP"
echo "══════════════════════════════════════════════════════════"

[ "$TOTAL_FAIL" = "0" ] && exit 0 || exit 1
