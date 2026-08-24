#!/bin/bash
# ─────────────────────────────────────────────────────────────
# Session Workspace CI — create/close/snapshot RPC + 事件 + 清理策略
# 对应 docs/design/SESSION_WORKSPACE_CHAT.md §3（RPC 接口规格）
#
# 用法：bash tests/session_workspace_ci.sh
# 隔离：HOME/SESSION_DIR/WORKTREE_ROOT/SOCKET 全部指向 /tmp 临时目录，
#       不碰真实 ~/.ion，不碰真实仓库。
# ─────────────────────────────────────────────────────────────
set -u
ION_BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/ion"
[ -x "$ION_BIN" ] || { echo "FATAL: $ION_BIN 不存在，先 cargo build --bin ion"; exit 1; }

TEST_DIR=$(mktemp -d /tmp/ion-session-workspace-test.XXXXXX)
export HOME="$TEST_DIR/home"; mkdir -p "$HOME/.ion"
export ION_SESSION_DIR="$TEST_DIR/home/.ion/agent/sessions"
export ION_WORKTREE_ROOT="$TEST_DIR/worktrees"
export ION_HOST_SOCKET="$TEST_DIR/host.sock"
export ION_FAUX_REPLY="子任务已在独立工作空间完成。"
mkdir -p "$ION_SESSION_DIR" "$ION_WORKTREE_ROOT"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2"; fi; }

cleanup() {
  [ -n "${SUB_PID:-}" ] && kill "$SUB_PID" 2>/dev/null
  [ -n "${UI_PID:-}" ] && kill "$UI_PID" 2>/dev/null
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
}
trap cleanup EXIT

echo "═══ 准备：测试 git 仓库 + 隔离 host ═══"
mkdir "$TEST_DIR/repo" && cd "$TEST_DIR/repo"
git init -q -b main && git config user.email t@t && git config user.name t
echo hello > README.md && git add -A && git commit -qm init

rm -f "$ION_HOST_SOCKET"
"$ION_BIN" serve > "$TEST_DIR/host.log" 2>&1 &
HOST_PID=$!
for i in $(seq 1 15); do grep -q "Host listening" "$TEST_DIR/host.log" 2>/dev/null && break; sleep 1; done
grep -q "Host listening" "$TEST_DIR/host.log" || { echo "FATAL: host 未启动"; exit 1; }
ok "host 就绪 (PID $HOST_PID)"

rpc() { local p="${2:-}"; [ -z "$p" ] && p='{}'; "$ION_BIN" rpc --method "$1" --params "$p" 2>/dev/null; }

echo; echo "═══ Group A: create_workspace_session ═══"
SID_A=$(rpc create_session "{\"project_path\":\"$TEST_DIR/repo\"}" | jq -r '.data.session_id')
[ -n "$SID_A" ] && [ "$SID_A" != "null" ]; check $? "create_session 父会话 ($SID_A)"

# 后台订阅父会话事件流（验证 Push）
"$ION_BIN" subscribe --session "$SID_A" > "$TEST_DIR/events.log" 2>&1 &
SUB_PID=$!
sleep 1

CREATE_OUT=$(rpc create_worker "{
  \"relation\":\"child\",
  \"creator\":\"$SID_A\",
  \"project_path\":\"$TEST_DIR/repo\",
  \"worktree\":{\"branch\":\"feat/ws-ci\"},
  \"require_clean\":true,
  \"initial_prompt\":\"在独立工作空间处理 CI 任务\"
}")
SID_B=$(echo "$CREATE_OUT" | jq -r '.data.sessionId // empty')
WS_PATH=$(echo "$CREATE_OUT" | jq -r '.data.worktree_path // empty')
WS_BRANCH=$(echo "$CREATE_OUT" | jq -r '.data.worktree_branch // empty')
[ -n "$SID_B" ]; check $? "create_worker 返回 sessionId ($SID_B)"
[ "$WS_BRANCH" = "feat/ws-ci" ]; check $? "worktree_branch 尊重显式指定 ($WS_BRANCH)"
[ -d "$WS_PATH" ]; check $? "响应带 worktree_path（卡片元数据）"
jq -e --arg sid "$SID_B" --arg parent "$SID_A" '.sessions[$sid].parent_session == $parent' $HOME/.ion/agent/sessions.index.json >/dev/null; check $? "索引血缘正确（parent_session）"
[ -d "$WS_PATH" ] && [ -f "$WS_PATH/README.md" ]; check $? "worktree 目录真实存在且有内容 ($WS_PATH)"
git -C "$TEST_DIR/repo" branch --list feat/ws-ci | grep -q feat/ws-ci; check $? "主仓库出现分支 feat/ws-ci"

echo; echo "═══ Group B: get_session_snapshot（Pull 恢复）═══"
sleep 2   # 等 worker B 跑完首轮
SNAP=$(rpc get_session_snapshot "{\"session_id\":\"$SID_B\"}")
echo "$SNAP" | jq -e '.data.workspace.sessionId == "'"$SID_B"'"' >/dev/null; check $? "快照含 workspace 元数据"
echo "$SNAP" | jq -e '.data.workspace.status == "idle" or .data.workspace.status == "running"' >/dev/null; check $? "运行态合并 (idle/running)"
echo "$SNAP" | jq -e '.data.worker != null' >/dev/null; check $? "快照含 worker 状态"
[ "$(echo "$SNAP" | jq -r '.data.messageCount')" -ge 1 ]; check $? "快照含最近消息"

echo; echo "═══ Group C: 事件推送（Push）═══"
sleep 1
# 收敛后统一发 workspace_session_created（一个事件携带全量元数据：分支/路径/sessionId）
N_WS=$(grep -c "workspace_session_created" "$TEST_DIR/events.log" || true)
[ "$N_WS" -ge 1 ]; check $? "subscribe 收到 workspace_session_created（实测 ${N_WS}）"
grep -q "feat/ws-ci" "$TEST_DIR/events.log"; check $? "事件携带 branch 元数据"
grep -q "$SID_B" "$TEST_DIR/events.log"; check $? "事件携带 sessionId（可跳转）"
grep -qE '"extension": ?"workspace"' "$TEST_DIR/events.log"; check $? "事件外壳 extension=workspace"

echo; echo "═══ Group G: LLM spawn_worker(worktree) 路径统一管线 ═══"
# 模拟"输入框一句话 → LLM 调 spawn_worker(worktree:true)"：
# 应与显式 RPC 同构——事件 + 持久化 + 响应元数据齐全
SPAWN_OUT=$("$ION_BIN" rpc --session "$SID_A" --method call_tool --params '{"tool":"spawn_worker","args":{"relation":"child","agent":"developer","task":"LLM 触发的独立工作空间","wait":true,"worktree":true,"branch":"feat/llm-path"}}' 2>/dev/null)
echo "$SPAWN_OUT" | jq -e '.data.output != null' >/dev/null; check $? "call_tool spawn_worker 成功"
INNER=$(echo "$SPAWN_OUT" | jq -r '.data.output')
echo "$INNER" | jq -e '.worktree_path != null' >/dev/null; check $? "工具响应含 worktree_path（P0 修复）"
echo "$INNER" | jq -e '.session_id != null' >/dev/null; check $? "工具响应含 session_id（UI 可订阅）"
SPAWN_SID=$(echo "$INNER" | jq -r '.session_id')
sleep 2
grep -q "workspace_session_created" "$TEST_DIR/events.log"; check $? "spawn 路径广播 workspace_session_created"
jq -e --arg sid "$SPAWN_SID" '.sessions[$sid].branch == "feat/llm-path"' $HOME/.ion/agent/sessions.index.json >/dev/null; check $? "spawn 路径写入索引（branch）"
# 清理 spawn 出的会话（走统一 kill 路径）
SPAWN_WID=$(rpc list_workers | jq -r '.data.workers[] | select(.sessionId == "'"$SPAWN_SID"'") | .workerId' | head -1)
[ -n "$SPAWN_WID" ] && rpc kill "{\"workerId\":\"$SPAWN_WID\"}" >/dev/null

echo; echo "═══ Group D: require_clean 拒绝脏源 ═══"
echo dirty > "$TEST_DIR/repo/dirty.txt"
DIRTY_OUT=$(rpc create_worker "{
  \"relation\":\"child\",
  \"project_path\":\"$TEST_DIR/repo\",
  \"worktree\":{\"branch\":\"feat/ws-dirty\"},
  \"require_clean\":true
}")
echo "$DIRTY_OUT" | jq -e '.success == false' >/dev/null; check $? "脏源目录拒绝创建"
echo "$DIRTY_OUT" | jq -e '.error | test("uncommitted")' >/dev/null; check $? "错误信息明确 (uncommitted changes)"
rm "$TEST_DIR/repo/dirty.txt"

echo; echo "═══ Group E: close_workspace_session 清理策略 ═══"
WID_B=$(rpc list_workers | jq -r '.data.workers[] | select(.sessionId == "'"$SID_B"'") | .workerId' | head -1)
[ -n "$WID_B" ]; check $? "按 sessionId 找到 workerId ($WID_B)"
CLOSE_OUT=$(rpc kill "{\"workerId\":\"$WID_B\",\"cleanupWorktree\":true,\"deleteBranch\":false}")
echo "$CLOSE_OUT" | jq -e '.data.killed == true' >/dev/null; check $? "kill 成功（含清理策略参数）"
[ ! -d "$WS_PATH" ]; check $? "worktree 目录已删除"
git -C "$TEST_DIR/repo" branch --list feat/ws-ci | grep -q feat/ws-ci; check $? "分支 feat/ws-ci 保留（默认策略）"
sleep 1
grep -q "workspace_session_closed" "$TEST_DIR/events.log"; check $? "subscribe 收到 workspace_session_closed"
SNAP2=$(rpc get_session_snapshot "{\"session_id\":\"$SID_B\"}")
echo "$SNAP2" | jq -e '.data.workspace.status == "closed"' >/dev/null; check $? "kill 后快照状态 closed（workspace 落盘）"
echo "$SNAP2" | jq -e '.data.worker == null' >/dev/null; check $? "worker 已停止"

echo; echo "═══ Group H: 会话产生/终止必推送（subscribe --ui）═══"
# 语义：任何会话产生都广播 session_created、终止广播 session_closed，
# 接收方接不接收是它的事，但发送方一定推。
"$ION_BIN" subscribe --ui > "$TEST_DIR/ui_events.log" 2>&1 &
UI_PID=$!
sleep 1
# 触发 1：普通 create_session
SID_H=$(rpc create_session "{\"project_path\":\"$TEST_DIR/repo\"}" | jq -r '.data.session_id')
[ -n "$SID_H" ] && [ "$SID_H" != "null" ]; check $? "触发：普通会话创建 ($SID_H)"
# 触发 2：LLM spawn 子会话（worktree 路径，之后可关闭验证终止推送）
SPAWN_H=$("$ION_BIN" rpc --session "$SID_H" --method call_tool --params '{"tool":"spawn_worker","args":{"relation":"child","agent":"developer","task":"推送验证","wait":true,"worktree":true,"branch":"feat/push-ci"}}' 2>/dev/null)
SID_H2=$(echo "$SPAWN_H" | jq -r '.data.output | fromjson | .session_id // empty')
[ -n "$SID_H2" ]; check $? "触发：spawn 子会话 ($SID_H2)"
sleep 2
N_CREATED=$(grep -c "session_created" "$TEST_DIR/ui_events.log" || true)
[ "$N_CREATED" -ge 2 ]; check $? "session_created 推送 ≥2 次（实测 ${N_CREATED}）"
grep -qE "\"sessionId\": ?\"$SID_H\"" "$TEST_DIR/ui_events.log"; check $? "推送携带 sessionId（接收方可定位）"
# 触发 3：终止也推（统一 kill 路径）
H2_WID=$(rpc list_workers | jq -r '.data.workers[] | select(.sessionId == "'"$SID_H2"'") | .workerId' | head -1)
[ -n "$H2_WID" ] && rpc kill "{\"workerId\":\"$H2_WID\"}" >/dev/null
sleep 2
grep -q "session_closed" "$TEST_DIR/ui_events.log"; check $? "session_closed 终止推送到达"
grep -qE '"extension": ?"session"' "$TEST_DIR/ui_events.log"; check $? "事件外壳 extension=session"

echo; echo "═══ Group F: 持久化（重启恢复数据源）═══"
jq -e --arg sid "$SID_B" '.sessions[$sid].workspace_status == "closed"' $HOME/.ion/agent/sessions.index.json >/dev/null; check $? "索引 workspace_status=closed（无 sidecar 文件）"
[ ! -f "$HOME/.ion/agent/workspaces.json" ]; check $? "未产生 sidecar 文件（存储落位原则）"
# JSONL 留痕：父会话 custom entry 记录创建/关闭全过程（重放可还原；卡片也在父时间线）
A_JSONL=$(find "$ION_SESSION_DIR" -name "$SID_A.jsonl" | head -1)
[ -n "$A_JSONL" ]; check $? "找到父会话 JSONL"
grep -q '"customType":"workspace_session"' "$A_JSONL"; check $? "父 JSONL 含 workspace_session custom 条目"
sleep 3  # 创建留痕是延迟重试写入（等父会话 header），给足窗口
grep -q '"event":"created"' "$A_JSONL" && grep -q "feat/ws-ci" "$A_JSONL"; check $? "留痕创建事件（含完整参数）"
grep -q '"event":"closed"' "$A_JSONL"; check $? "留痕关闭事件（含清理策略）"

echo; echo "══════════════════════════════════"
echo "结果: $PASS passed / $FAIL failed"
[ "$FAIL" = "0" ] || { echo "失败详情见 $TEST_DIR"; exit 1; }
rm -rf "$TEST_DIR"
