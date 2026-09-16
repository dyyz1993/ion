#!/usr/bin/env bash
# approval_bus_ci.sh — 统一审批总线 CI：泵全来源 policy + 协议基线 + M1 预留断言块
#
# 归属: fix7/m3-pump-schema-ci（M3）。设计对齐：
#   - 泵全来源升级: src/worker_registry.rs approval_pump_fire / auto_respond（M1 薄调用点）
#   - policy per-kind map: src/sandbox_pool.rs ApprovalPolicy::PerKind + config untagged
#   - 协议形状固化: schemas/rpc/subscribe/{approvals_pending,approval_respond}.json
#     + events/{approval_request,approval_resolved}.json（M3 是固化者，M1 以此为对齐基准）
#
# 验证面：
#   Group A  现状基线（master 既有能力）    — verb_pending 空表；file_snapshot 审批链
#           （faux 写文件 → 快照帧 pendingApprovals（Pull 口）→ review_pending 形状 →
#            人工 review_approve_all 归零）
#           ⚠️ 现状已知缺口：worker 产出的 ApprovalRequest 事件只进 reader-loop 泵/
#           instance 流，不到 --ui 订阅流（M1 统一总线补 Push 口，见 Group D 预留）
#   Group B  policy map 形态（M3 新增）     — config 驱动（ION_REMOTE_WORKERS 注入
#           map 形态档案）+ sandbox_policy RPC GET/SET（string|map 两形态、
#           归一化视图、非法值严格拒绝、旧字符串兼容）
#   Group C  泵全来源 policy 实测（M3）     — worker 级 map 覆盖驱动泵：
#           file_snapshot=auto → SandboxAutoApproved + ApprovalResolved（新统一事件，
#           载荷对齐 events/approval_resolved.json）+ review_pending 归零；
#           map 内 file_snapshot=ask → 泵不 fire 对照（pending 保留）
#   Group D  统一审批 RPC 预留块（M1/M2 合并后启用）— approvals_pending /
#           approval_respond / 事件 kind 字段。master 上自动 SKIP（grep 标记:
#           [M1-PENDING:approvals_pending] / [M1-PENDING:approval_respond] /
#           [M2-PENDING:event-kind]），M1/M2 合并后把 SKIP 分支换成真断言
#           （断言形状已在注释里写死，对齐 schemas/ 契约）。
#
# 隔离：每 Phase 一个独立 host（HOME 覆盖 + ION_SESSION_DIR + 私有 socket + faux
#       script），绝不读写真实 ~/.ion。faux 文件名/内容带随机后缀——file-snapshot
#       按 diff 判定，同路径同内容 = 无 diff = 无审批请求（2026-09-16 实测踩坑）。
# 红线：不 ssh 任何真机；cleanup 只 kill 本脚本记录的 PID，严禁宽泛 pkill。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
PASS=0; FAIL=0; SKIP=0
pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }
skip() { printf '  ⏭️  %s\n' "$1"; SKIP=$((SKIP + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }

TEST_ROOT="$(mktemp -d /tmp/ion-approval-bus-XXXXXX)"
HOST_PID=""
SUB_PID=""
cleanup() {
    # 只杀自己起的进程（铁律：绝不 pkill ion）
    [ -n "$SUB_PID" ] && kill "$SUB_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

printf '%s\n' '════════════════════════════════════════════════════'
printf '%s\n' '  Approval Bus / 审批泵全来源 CI — '"$(date)"
printf '%s\n' '════════════════════════════════════════════════════'

echo "[Phase 0] build"
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1; then pass "build ion"; else fail "build ion"; exit 1; fi

# 沙盒池 mock（ION_REMOTE_WORKERS 注入；不 ssh——只在 config/档案层生效）：
#   ci-map  = map 形态档案（file_snapshot+remote_verb auto，ui_ask ask）← M3 新形态
#   ci-auto = 旧字符串形态 auto_approve（兼容性对照）
#   ci-man  = 缺省 default（人工审批对照）
SANDBOX_JSON='{"ci-map":{"hostname":"ci-map.invalid","user":"ci","approval_policy":{"file_snapshot":"auto","ui_ask":"ask","remote_verb":"auto"}},"ci-auto":{"hostname":"ci-auto.invalid","user":"ci","approval_policy":"auto_approve"},"ci-man":{"hostname":"ci-man.invalid","user":"ci"}}'

start_host() { # start_host <phase-tag> <faux-jsonl-path>
    local tag="$1" faux="$2"
    local home="$TEST_ROOT/$tag-home" sock="$TEST_ROOT/$tag.sock"
    mkdir -p "$home/.ion/agent/sessions"
    # config：开 file-snapshot（审批链依赖），关 memory 系（防单例 worker 消费 faux 队列）
    printf '%s\n' '{"extensions":{"file-snapshot":{"enabled":true},"memory":{"enabled":false},"global-memory":{"enabled":false},"learning":{"enabled":false}}}' > "$home/.ion/config.json"
    HOME="$home" ION_SESSION_DIR="$home/.ion/agent/sessions" \
    ION_HOST_SOCKET="$sock" ION_REMOTE_WORKERS="$SANDBOX_JSON" \
    ION_FAUX_SCRIPT="$faux" ION_FAUX_REPEAT=1 \
        "$ION_BIN" serve > "$TEST_ROOT/$tag.log" 2>&1 &
    HOST_PID=$!
    for _ in $(seq 1 20); do
        [ -S "$sock" ] && break
        sleep 0.5
    done
    if [ -S "$sock" ]; then
        pass "host[$tag] 起动（socket=$sock, HOME 隔离, 沙盒池 mock 注入）"
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

# ui 订阅日志里找事件（--ui 流是 pretty-JSON 对象序列 → jq -s 聚合）
ui_event_count() { # ui_event_count <log> <ui_type>
    jq -s --arg t "$2" '[.[] | select(.ui_type==$t)] | length' "$1" 2>/dev/null
}
wait_ui_event() { # wait_ui_event <log> <ui_type> [tries=40]
    local log="$1" t="$2" tries="${3:-40}" n=""
    for _ in $(seq 1 "$tries"); do
        n=$(ui_event_count "$log" "$t")
        [ "${n:-0}" -gt 0 ] && return 0
        sleep 0.2
    done
    return 1
}

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase 1: 现状基线（host p1，default 策略）"
# ════════════════════════════════════════════════════════

P1_PROJ="$TEST_ROOT/p1-proj"; mkdir -p "$P1_PROJ"
R1="$RANDOM"
cat > "$TEST_ROOT/p1-faux.jsonl" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$P1_PROJ/base_a.txt","content":"base-a-$R1"}}}
{"tool_call":{"name":"write","input":{"file_path":"$P1_PROJ/base_b.txt","content":"base-b-$R1"}}}
{"text":"baseline done"}
JSONL
start_host p1 "$TEST_ROOT/p1-faux.jsonl"

echo ""
echo "── Group A: 现状基线（master 既有能力不变）"
VP=$(rpc p1 verb_pending)
echo "$VP" | jq -e '.success == true and (.data.pending | length) == 0' >/dev/null
check $? "A1 verb_pending 空表（remote_verb 来源现状基线：无沙盒 verb 请求）"

AW=$(rpc p1 create_worker '{"agent":"build","project_path":"'"$P1_PROJ"'"}')
AWID=$(echo "$AW" | jq -r '.data.workerId // empty')
ASID=$(echo "$AW" | jq -r '.data.sessionId // empty')
if [ -n "$AWID" ]; then pass "A2 create_worker(本地, default 策略) → $AWID / $ASID"; else fail "A2 create_worker"; echo "$AW" | head -3; fi

wrpc p1 "$ASID" prompt '{"text":"写文件并回报"}' >/dev/null 2>&1
if wait_idle p1 "$AWID"; then pass "A3 faux 轮完成（worker Idle）"; else fail "A3 faux 轮完成"; fi

# A4 审批可观测（Pull 口）：instance 订阅快照帧的 pendingApprovals.count>=1
# （worker 侧 ApprovalRequest 的 Push 口现状只进 reader-loop 泵，不到订阅流——
#  M1 统一总线补 Push，见 Group D 预留块）
INST_LOG="$TEST_ROOT/p1-inst.log"
HOME="$TEST_ROOT/p1-home" ION_HOST_SOCKET="$TEST_ROOT/p1.sock" \
    timeout 4 "$ION_BIN" subscribe --session "$ASID" > "$INST_LOG" 2>&1
jq -s '[.[] | select(.snapshot == true)] | .[0].event.data.pendingApprovals.count >= 1' "$INST_LOG" >/dev/null 2>&1
check $? "A4 快照帧 pendingApprovals.count>=1（审批状态 Pull 口，instance 订阅）"

RP=$(wrpc p1 "$ASID" review_pending)
echo "$RP" | jq -e '.success == true and .data.summary.total >= 1' >/dev/null
check $? "A5 review_pending 形状（summary.total>=1，default 策略保留人工审批）"
echo "$RP" | jq -e '.data.pending | type == "array"' >/dev/null
check $? "A6 review_pending.pending 数组（基线形状）"

# 现状人工审批语义：review_approve_all → 归零
wrpc p1 "$ASID" review_approve_all >/dev/null 2>&1
RP2=$(wrpc p1 "$ASID" review_pending)
echo "$RP2" | jq -e '.data.summary.total == 0' >/dev/null
check $? "A7 人工 review_approve_all 归零（现状审批语义不变）"

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase 2: policy map + 泵全来源 + 预留块（host p2）"
# ════════════════════════════════════════════════════════

P2_PROJ="$TEST_ROOT/p2-proj"; mkdir -p "$P2_PROJ"
R2="$RANDOM"
# 单 worker 两轮：轮1 写 c1/c2（泵放行验证），轮2 写 c3（ask 对照）。
# 每个 worker 进程从头消费 faux 队列；ION_FAUX_REPEAT=1 兜底队列耗尽。
cat > "$TEST_ROOT/p2-faux.jsonl" <<JSONL
{"tool_call":{"name":"write","input":{"file_path":"$P2_PROJ/pump_c1.txt","content":"pump-c1-$R2"}}}
{"tool_call":{"name":"write","input":{"file_path":"$P2_PROJ/pump_c2.txt","content":"pump-c2-$R2"}}}
{"text":"pump round1 done"}
{"tool_call":{"name":"write","input":{"file_path":"$P2_PROJ/pump_c3.txt","content":"pump-c3-$R2"}}}
{"text":"pump round2 done"}
JSONL
start_host p2 "$TEST_ROOT/p2-faux.jsonl"

echo ""
echo "── Group B: policy map 形态（M3：config 驱动 + sandbox_policy RPC 两形态）"
# B1-B3：config 档案（ION_REMOTE_WORKERS 注入，config 加载层实测——untagged 解析）
GP=$(rpc p2 sandbox_policy '{"host":"ci-map"}')
echo "$GP" | jq -e '.data.profile.file_snapshot == "auto" and .data.profile.ui_ask == "ask" and .data.profile.remote_verb == "auto"' >/dev/null
check $? "B1 map 档案 GET：profile 为三来源解析视图（config untagged 实测）"
echo "$GP" | jq -e '.data.effective.file_snapshot == "auto" and .data.effective.ui_ask == "ask"' >/dev/null
check $? "B2 map 档案生效链 effective（无覆盖时 = profile）"
GPA=$(rpc p2 sandbox_policy '{"host":"ci-auto"}')
echo "$GPA" | jq -e '.data.profile == "auto_approve" and .data.effective == "auto_approve"' >/dev/null
check $? "B3 旧字符串档案兼容（ci-auto → auto_approve，零破坏）"

# B4-B8：RPC SET map 形态（对象参数）+ 回读 + 严格拒绝
SPM=$(rpc p2 sandbox_policy '{"host":"ci-man","policy":{"file_snapshot":"auto","remote_verb":"auto"}}')
echo "$SPM" | jq -e '.success == true and .data.effective.file_snapshot == "auto" and .data.effective.ui_ask == "ask"' >/dev/null
check $? "B4 SET host map（对象参数）→ 归一化生效视图"
rpc p2 sandbox_policy '{"host":"ci-man"}' | jq -e '.data.override.file_snapshot == "auto" and .data.override.remote_verb == "auto" and .data.override.ui_ask == "ask"' >/dev/null
check $? "B5 GET 回读 override（map 存储可读，未列来源=ask）"
BADK=$(rpc p2 sandbox_policy '{"host":"ci-man","policy":{"bogus_kind":"auto"}}')
echo "$BADK" | jq -e '.success == false and ((.error // "") | contains("unknown policy kind"))' >/dev/null
check $? "B6 SET 非法 kind key → 严格拒绝（fail-closed）"
BADV=$(rpc p2 sandbox_policy '{"host":"ci-man","policy":{"file_snapshot":"yolo"}}')
echo "$BADV" | jq -e '.success == false' >/dev/null
check $? "B7 SET 非法 action 值 → 严格拒绝"
BADS=$(rpc p2 sandbox_policy '{"host":"ci-man","policy":"maybe"}')
echo "$BADS" | jq -e '.success == false' >/dev/null
check $? "B8 SET 非法 scalar → 严格拒绝（旧校验语义保留）"
# 清掉 ci-man 覆盖，保持档案层纯净
rpc p2 sandbox_policy '{"host":"ci-man","policy":"default"}' >/dev/null

echo ""
echo "── Group C: 泵全来源 policy 实测（worker 级 map 覆盖驱动）"
# ui 订阅：抓 SandboxAutoApproved / ApprovalResolved（broadcast_ui_event 走 ui 路由）
UI_LOG="$TEST_ROOT/p2-ui.log"
HOME="$TEST_ROOT/p2-home" ION_HOST_SOCKET="$TEST_ROOT/p2.sock" \
    "$ION_BIN" subscribe --ui > "$UI_LOG" 2>&1 &
SUB_PID=$!
sleep 1

CW=$(rpc p2 create_worker '{"agent":"build","project_path":"'"$P2_PROJ"'"}')
CWID=$(echo "$CW" | jq -r '.data.workerId // empty')
CSID=$(echo "$CW" | jq -r '.data.sessionId // empty')
if [ -n "$CWID" ]; then pass "C1 create_worker(泵实测) → $CWID / $CSID"; else fail "C1 create_worker"; echo "$CW" | head -3; fi

rpc p2 sandbox_policy "{\"worker\":\"$CWID\"}" | jq -e '.data.effective == "default"' >/dev/null
check $? "C2 出厂 default（本地 worker 无出生档案）"
SPW=$(rpc p2 sandbox_policy "{\"worker\":\"$CWID\",\"policy\":{\"file_snapshot\":\"auto\",\"ui_ask\":\"ask\"}}")
echo "$SPW" | jq -e '.data.effective.file_snapshot == "auto" and .data.effective.ui_ask == "ask"' >/dev/null
check $? "C3 SET worker map → per-kind 生效（file_snapshot auto / ui_ask ask）"

wrpc p2 "$CSID" prompt '{"text":"写文件并回报"}' >/dev/null 2>&1
if wait_idle p2 "$CWID"; then pass "C4 faux 轮1 完成"; else fail "C4 faux 轮1 完成"; fi

wait_ui_event "$UI_LOG" SandboxAutoApproved
check $? "C5 SandboxAutoApproved 事件可见（遗留事件保留，file_snapshot 泵路径）"
wait_ui_event "$UI_LOG" ApprovalResolved
check $? "C6 ApprovalResolved 统一事件可见（M3 新增：by=pump, kind=file_snapshot）"
jq -s '[.[] | select(.ui_type=="ApprovalResolved")][0] | .data.kind == "file_snapshot" and .data.by == "pump" and (.data.id | startswith("appr_"))' "$UI_LOG" >/dev/null 2>&1
check $? "C7 ApprovalResolved 载荷（kind=file_snapshot, by=pump, id=appr_*；对齐 events/approval_resolved.json）"
CRP=$(wrpc p2 "$CSID" review_pending)
echo "$CRP" | jq -e '.data.summary.total == 0' >/dev/null
check $? "C8 review_pending 归零（map 内 file_snapshot=auto 放行）"

# C9-C12: 对照——map 内 file_snapshot=ask → 同 worker 再写 → 泵不 fire，pending 保留
rpc p2 sandbox_policy "{\"worker\":\"$CWID\",\"policy\":{\"file_snapshot\":\"ask\",\"remote_verb\":\"auto\"}}" | jq -e '.data.effective.file_snapshot == "ask"' >/dev/null
check $? "C9 改 map（file_snapshot=ask / remote_verb=auto）→ 生效视图翻转"
wrpc p2 "$CSID" prompt '{"text":"再写一个文件"}' >/dev/null 2>&1
if wait_idle p2 "$CWID"; then pass "C10 对照轮完成"; else fail "C10 对照轮完成"; fi
sleep 2
N_RESOLVED=$(ui_event_count "$UI_LOG" ApprovalResolved)
N_AUTO=$(ui_event_count "$UI_LOG" SandboxAutoApproved)
[ "${N_RESOLVED:-0}" -eq 1 ] && [ "${N_AUTO:-0}" -eq 1 ]
check $? "C11 对照轮无新放行事件（per-kind ask 不触发泵；事件计数仍为轮1 的 1）"
CRP2=$(wrpc p2 "$CSID" review_pending)
echo "$CRP2" | jq -e '.data.summary.total >= 1' >/dev/null
check $? "C12 pending 保留（file_snapshot=ask 人工审批语义）"
kill "$SUB_PID" 2>/dev/null; SUB_PID=""

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase 3: 统一审批 RPC 预留块（M1 合并后启用）"
# ════════════════════════════════════════════════════════
echo ""
echo "── Group D: 统一审批总线 RPC（[M1-PENDING] 自动检测；master 上 SKIP）"

# 检测方式：master 上未知 host 命令回 success=false +
# "unknown method: <method> (and no `session` field for forwarding)"
# （src/bin/ion.rs socket 层语义）。M1 合并后命令存在 → 走真断言块。
UP=$(rpc p2 approvals_pending 2>/dev/null)
if echo "$UP" | jq -e '.success == false and ((.error // "") | contains("unknown method: approvals_pending"))' >/dev/null 2>&1; then
    skip "[M1-PENDING:approvals_pending] 命令不存在（master 基线）——合并 M1 后把本分支改为："
    echo "         # 断言块（M1 合并后启用，形状对齐 schemas/rpc/subscribe/approvals_pending.json）："
    echo "         # 1) 空表: .data.pending | length == 0（新 host 无三来源请求）"
    echo "         # 2) C 组场景后: .data.pending[].kind 枚举 ⊆ {ui_ask,file_snapshot,remote_verb}"
    echo "         # 3) 每条必有 id/kind/sessionId/summary/payload/raisedAtMs"
else
    # M1 已合并：真断言（形状对齐 schema 契约）
    echo "$UP" | jq -e '.success == true and (.data.pending | type == "array")' >/dev/null
    check $? "D1 approvals_pending 可调且 data.pending 为数组（M1 已合并）"
    echo "$UP" | jq -e '[.data.pending[]? | .kind] | all(. == "ui_ask" or . == "file_snapshot" or . == "remote_verb")' >/dev/null
    check $? "D2 pending[].kind 枚举合法（对齐 ApprovalEntry 契约）"
    echo "$UP" | jq -e '[.data.pending[]?] | all(has("id") and has("sessionId") and has("summary") and has("payload") and has("raisedAtMs"))' >/dev/null
    check $? "D3 ApprovalEntry 必填字段齐全"
fi

UR=$(rpc p2 approval_respond '{"id":"nonexistent","decision":"approve"}' 2>/dev/null)
if echo "$UR" | jq -e '.success == false and ((.error // "") | contains("unknown method: approval_respond"))' >/dev/null 2>&1; then
    skip "[M1-PENDING:approval_respond] 命令不存在（master 基线）——合并 M1 后把本分支改为："
    echo "         # 断言块（M1 合并后启用，形状对齐 schemas/rpc/subscribe/approval_respond.json）："
    echo "         # 1) 未知 id: .error | startswith(\"approval not found: \")"
    echo "         # 2) 缺 decision: .error == \"missing params.decision\"（fail-closed）"
    echo "         # 3) 非法 decision: .error | startswith(\"invalid decision '\")"
    echo "         # 4) 真实闭环: approvals_pending 取 id → approval_respond approve → 事件流见 ApprovalResolved(by=user)"
else
    echo "$UR" | jq -e '.success == false and ((.error // "") | startswith("approval not found: "))' >/dev/null
    check $? "D4 approval_respond 未知 id → not found（M1 已合并）"
    URM=$(rpc p2 approval_respond '{"id":"x"}' 2>/dev/null)
    echo "$URM" | jq -e '.success == false and .error == "missing params.decision"' >/dev/null
    check $? "D5 approval_respond 缺 decision → fail-closed 拒绝"
fi

# 事件 kind 字段（M2 三来源接入后统一形态 ApprovalRequest 带 kind）。
# 现状可观测口：--ui 流看不到 worker 产出的 ApprovalRequest（只进 reader-loop 泵），
# M1/M2 合并后 ApprovalRequest 进统一总线 → --ui 可见且带 kind → 本分支断言枚举。
KIND_HIT=$(jq -s '[.[] | select(.ui_type=="ApprovalRequest") | .data.kind // empty] | length' "$UI_LOG" 2>/dev/null)
if [ "${KIND_HIT:-0}" -gt 0 ]; then
    jq -s -e '[.[] | select(.ui_type=="ApprovalRequest") | .data.kind] | all(. == "ui_ask" or . == "file_snapshot" or . == "remote_verb")' "$UI_LOG" >/dev/null 2>&1
    check $? "D6 ApprovalRequest.data.kind 枚举合法（M2 已接入统一形态）"
else
    skip "[M2-PENDING:event-kind] ApprovalRequest 未进 --ui 流（master：worker 事件只进 reader-loop 泵；M1/M2 统一总线补 Push 后本分支断言 kind 枚举）"
fi

echo ""
echo "════════════════════════════════════════════════════"
echo "结果: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
echo "（SKIP = M1/M2 预留块在 master 上的预期形态，合并后改为真断言）"
[ "$FAIL" -eq 0 ] || exit 1
