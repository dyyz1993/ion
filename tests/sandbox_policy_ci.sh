#!/usr/bin/env bash
# sandbox_policy_ci.sh — 沙盒档案 approval_policy + 审批泵 命令行验证（mock 级，零 ssh）
#
# 设计文档: docs/design/SANDBOX_POOL.md §3.3（Phase 2 / Phase 1.5 审批停摆机制性解决）
#
# 验证面：
#   Group A  沙盒档案视图      — list_sandboxes 展示 approvalPolicy/notes；sandbox_policy GET
#   Group B  运行时覆盖        — sandbox_policy SET host（set→get→list 三层一致 + 非法值拒绝）
#   Group C  审批泵闭环        — auto_approve 沙盒 worker 写文件 → ApprovalRequest 自动
#                               review_approve_all（SandboxAutoApproved 事件 + review_pending 归零）
#   Group D  对照组            — default 策略同样任务 → 泵不 fire，pending 保留人工审批
#
# 隔离：每个 Phase 一个独立 host（HOME 覆盖 + ION_SESSION_DIR + 私有 socket + faux script）。
# 红线：不 ssh 任何真机；cleanup 只 kill 本脚本记录的 HOST_PID，严禁宽泛 pkill。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
PASS=0; FAIL=0
pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }

TEST_ROOT="$(mktemp -d /tmp/ion-sandbox-policy-XXXXXX)"
HOST_PID=""
cleanup() {
    # 只杀自己起的 host（铁律：绝不 pkill ion）
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

printf '%s\n' '════════════════════════════════════════════════════'
printf '%s\n' '  Sandbox Policy / 审批泵 CI — '"$(date)"
printf '%s\n' '════════════════════════════════════════════════════'

echo "[Phase 0] build"
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1; then pass "build ion"; else fail "build ion"; exit 1; fi

# start_host <name> — 起一台 HOME 隔离 host；沙盒池经 ION_REMOTE_WORKERS 注入
#（两台 host 共用同一份沙盒配置：ci-auto=auto_approve+notes，ci-man=default）
SANDBOX_JSON='{"ci-auto":{"hostname":"ci-auto.invalid","user":"ci","approval_policy":"auto_approve","notes":["cargo 在 ~/.cargo/bin（CI mock 事实）"]},"ci-man":{"hostname":"ci-man.invalid","user":"ci"}}'

start_host() { # start_host <phase-tag> <faux-jsonl-path>
    local tag="$1" faux="$2"
    local home="$TEST_ROOT/$tag-home" sock="$TEST_ROOT/$tag.sock"
    mkdir -p "$home/.ion/agent/sessions"
    # config：开 file-snapshot（审批链依赖），关 memory 系（防单例 worker 消费 faux
    # 队列导致步骤错位）；LLM 全走 faux（ION_FAUX_SCRIPT 注入 worker）
    printf '%s\n' '{"extensions":{"file-snapshot":{"enabled":true},"memory":{"enabled":false},"global-memory":{"enabled":false},"learning":{"enabled":false}}}' > "$home/.ion/config.json"
    HOME="$home" ION_SESSION_DIR="$home/.ion/agent/sessions" \
    ION_HOST_SOCKET="$sock" ION_REMOTE_WORKERS="$SANDBOX_JSON" \
    ION_FAUX_SCRIPT="$faux" \
        "$ION_BIN" serve > "$TEST_ROOT/$tag.log" 2>&1 &
    HOST_PID=$!
    for _ in $(seq 1 20); do
        [ -S "$sock" ] && break
        sleep 0.5
    done
    if [ -S "$sock" ]; then
        pass "host[$tag] 起动（socket=$sock, HOME 隔离, file-snapshot 开）"
    else
        fail "host[$tag] 起动"
        tail -10 "$TEST_ROOT/$tag.log"
        exit 1
    fi
}

rpc() { # rpc <tag> <method> [params]
    local tag="$1" method="$2" params="${3:-}"
    if [ -n "$params" ]; then
        HOME="$TEST_ROOT/$1-home" ION_HOST_SOCKET="$TEST_ROOT/$1.sock" \
            "$ION_BIN" rpc --method "$method" --params "$params" 2>/dev/null
    else
        HOME="$TEST_ROOT/$1-home" ION_HOST_SOCKET="$TEST_ROOT/$1.sock" \
            "$ION_BIN" rpc --method "$method" 2>/dev/null
    fi
}

wrpc() { # wrpc <tag> <session> <method> [params]
    local tag="$1" sid="$2" method="$3" params="${4:-}"
    if [ -n "$params" ]; then
        HOME="$TEST_ROOT/$tag-home" ION_HOST_SOCKET="$TEST_ROOT/$tag.sock" \
            "$ION_BIN" rpc --session "$sid" --method "$method" --params "$params" 2>/dev/null
    else
        HOME="$TEST_ROOT/$tag-home" ION_HOST_SOCKET="$TEST_ROOT/$tag.sock" \
            "$ION_BIN" rpc --session "$sid" --method "$method" 2>/dev/null
    fi
}

wait_idle() { # wait_idle <tag> <wid> — 等 worker Idle（faux 无网络，60s 兜底）
    local tag="$1" wid="$2" st=""
    for _ in $(seq 1 60); do
        st=$(rpc "$tag" list_workers | jq -r --arg w "$wid" \
            '.data.workers[]? | select(.workerId==$w) | .status' 2>/dev/null)
        [ "$st" = "Idle" ] && return 0
        sleep 0.5
    done
    return 1
}

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase 1: 沙盒档案 + 覆盖 + 审批泵（host p1）"
# ════════════════════════════════════════════════════════

# faux 任务：写一个文件（触发 file-snapshot diff → ApprovalRequest）→ 回话
P1_PROJ="$TEST_ROOT/p1-proj"; mkdir -p "$P1_PROJ"
cat > "$TEST_ROOT/p1-faux.jsonl" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$P1_PROJ/pump_target.txt","content":"pump ci"}}}
{"text":"pump write done"}
JSONL
start_host p1 "$TEST_ROOT/p1-faux.jsonl"

echo ""
echo "── Group A: 沙盒档案视图"
LS=$(rpc p1 list_sandboxes)
echo "$LS" | jq -e '.data.sandboxes[] | select(.name=="ci-auto") | .approvalPolicy == "auto_approve"' >/dev/null
check $? "A1 list_sandboxes: ci-auto approvalPolicy=auto_approve（config 档案）"
echo "$LS" | jq -e '.data.sandboxes[] | select(.name=="ci-auto") | (.notes|length) == 1' >/dev/null
check $? "A2 list_sandboxes: ci-auto notes 注入（环境层事实）"
echo "$LS" | jq -e '.data.sandboxes[] | select(.name=="ci-man") | .approvalPolicy == "default"' >/dev/null
check $? "A3 list_sandboxes: ci-man 缺省 default"

GP=$(rpc p1 sandbox_policy '{"host":"ci-auto"}')
echo "$GP" | jq -e '.data.profile == "auto_approve" and .data.effective == "auto_approve" and .data.override == null' >/dev/null
check $? "A4 sandbox_policy GET host=ci-auto → profile/effective=auto_approve, 无覆盖"
rpc p1 sandbox_policy '{"host":"nope"}' | jq -e '.success == false' >/dev/null
check $? "A5 sandbox_policy GET 未知沙盒 → 明确报错"
rpc p1 sandbox_policy '{}' | jq -e '.success == false' >/dev/null
check $? "A6 sandbox_policy 缺 key → 明确报错"

echo ""
echo "── Group B: 运行时覆盖（内存态）"
SP=$(rpc p1 sandbox_policy '{"host":"ci-man","policy":"auto_approve"}')
echo "$SP" | jq -e '.data.effective == "auto_approve"' >/dev/null
check $? "B1 SET host=ci-man → auto_approve"
rpc p1 sandbox_policy '{"host":"ci-man"}' | jq -e '.data.effective == "auto_approve" and .data.override == "auto_approve"' >/dev/null
check $? "B2 GET 回读：effective=auto_approve, override 落值"
rpc p1 list_sandboxes | jq -e '.data.sandboxes[] | select(.name=="ci-man") | .approvalPolicy == "auto_approve"' >/dev/null
check $? "B3 list_sandboxes 视图同步（覆盖 > 档案）"
rpc p1 sandbox_policy '{"host":"ci-man","policy":"yolo"}' | jq -e '.success == false' >/dev/null
check $? "B4 非法 policy → 严格拒绝（不静默回落）"
rpc p1 sandbox_policy '{"host":"nope","policy":"auto_approve"}' | jq -e '.success == false' >/dev/null
check $? "B5 未知沙盒 SET → 拒绝"
rpc p1 sandbox_policy '{"host":"ci-man","policy":"default"}' | jq -e '.data.effective == "default"' >/dev/null
check $? "B6 SET 回 default → 覆盖生效（为 C 组把 ci-man 拨回人工审批）"

echo ""
echo "── Group C: 审批泵闭环（worker 级覆盖 → 自动放行）"
# mock 级：本地 worker + sandbox_policy worker 覆盖（泵是 host 侧统一链路，
# 与沙盒同一条 reader-loop→review_approve_all 通路；真沙盒端到端见真机验证清单）
# ⚠️ 先建 worker（不带 initial_prompt）→ 设策略 → 再 prompt：避免 turn 在 SET
# 之前就跑完导致泵错过 ApprovalRequest（出生即跑的竞态）。
CW=$(rpc p1 create_worker '{"agent":"build","project_path":"'"$P1_PROJ"'"}')
WID=$(echo "$CW" | jq -r '.data.workerId // empty')
SID=$(echo "$CW" | jq -r '.data.sessionId // empty')
if [ -n "$WID" ]; then pass "C1 create_worker(本地, 空任务) → $WID / $SID"; else fail "C1 create_worker"; echo "$CW" | head -3; fi

rpc p1 sandbox_policy "{\"worker\":\"$WID\"}" | jq -e '.data.effective == "default"' >/dev/null
check $? "C2 出厂 default（本地 worker 无出生档案）"
rpc p1 sandbox_policy "{\"worker\":\"$WID\",\"policy\":\"auto_approve\"}" | jq -e '.data.effective == "auto_approve"' >/dev/null
check $? "C3 SET worker → auto_approve（写后回读生效链）"

# 订阅：--ui 抓 SandboxAutoApproved（broadcast_ui_event 走 ui 路由）
UI_LOG="$TEST_ROOT/p1-ui.log"
HOME="$TEST_ROOT/p1-home" ION_HOST_SOCKET="$TEST_ROOT/p1.sock" \
    "$ION_BIN" subscribe --ui > "$UI_LOG" 2>&1 &
SUB_PID=$!
sleep 1

wrpc p1 "$SID" prompt '{"text":"写文件并回报"}' >/dev/null 2>&1
if wait_idle p1 "$WID"; then pass "C4 faux 轮完成（worker Idle）"; else fail "C4 faux 轮完成"; fi

# 等泵事件异步到达（最多 8s；ui 流是 pretty-JSON 对象序列 → jq -s 聚合）
PUMP_HIT=1
for _ in $(seq 1 40); do
    if jq -s -e '[.[] | select(.ui_type=="SandboxAutoApproved")] | length > 0' "$UI_LOG" >/dev/null 2>&1; then
        PUMP_HIT=0; break
    fi
    sleep 0.2
done
check $PUMP_HIT "C5 SandboxAutoApproved 事件可见（subscribe --ui）"
jq -s -e '[.[] | select(.ui_type=="SandboxAutoApproved")][0].data.workerId' "$UI_LOG" 2>/dev/null | grep -q "$WID"
check $? "C6 事件带 workerId"
jq -s -e '[.[] | select(.ui_type=="SandboxAutoApproved")][0].data.resolved.approved >= 1' "$UI_LOG" 2>/dev/null
check $? "C7 事件带放行明细（resolved.approved>=1）"

RP=$(wrpc p1 "$SID" review_pending)
echo "$RP" | jq -e '.data.summary.total == 0' >/dev/null
check $? "C8 review_pending 归零（泵已自动放行，无人工积压）"
kill "$SUB_PID" 2>/dev/null

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase 2: 对照组——default 策略泵不 fire（host p2）"
# ════════════════════════════════════════════════════════

HOST_PID=""
P2_PROJ="$TEST_ROOT/p2-proj"; mkdir -p "$P2_PROJ"
cat > "$TEST_ROOT/p2-faux.jsonl" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$P2_PROJ/ctrl_target.txt","content":"ctrl ci"}}}
{"text":"ctrl write done"}
JSONL
start_host p2 "$TEST_ROOT/p2-faux.jsonl"

XW=$(rpc p2 create_worker "{\"agent\":\"build\",\"project_path\":\"$P2_PROJ\"}")
XWID=$(echo "$XW" | jq -r '.data.workerId // empty')
XSID=$(echo "$XW" | jq -r '.data.sessionId // empty')
if [ -n "$XWID" ]; then pass "D1 create_worker(本地, default 策略) → $XWID"; else fail "D1 create_worker"; echo "$XW" | head -3; fi

SUB2_LOG="$TEST_ROOT/p2-ui.log"
HOME="$TEST_ROOT/p2-home" ION_HOST_SOCKET="$TEST_ROOT/p2.sock" \
    "$ION_BIN" subscribe --ui > "$SUB2_LOG" 2>&1 &
SUB2_PID=$!
sleep 1

wrpc p2 "$XSID" prompt '{"text":"写文件并回报"}' >/dev/null 2>&1
if wait_idle p2 "$XWID"; then pass "D2 faux 轮完成（worker Idle）"; else fail "D2 faux 轮完成"; fi
sleep 2
if jq -s -e '[.[] | select(.ui_type=="SandboxAutoApproved")] | length == 0' "$SUB2_LOG" >/dev/null 2>&1; then
    pass "D3 对照组无 SandboxAutoApproved（泵不 fire）"
else
    fail "D3 对照组无 SandboxAutoApproved（泵不 fire）"
fi
RP2=$(wrpc p2 "$XSID" review_pending)
echo "$RP2" | jq -e '.data.summary.total >= 1' >/dev/null
check $? "D4 review_pending 保留待审（人工审批语义不变）"
kill "$SUB2_PID" 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════"
echo "结果: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
