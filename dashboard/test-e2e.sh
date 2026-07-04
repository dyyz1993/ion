#!/bin/bash
# ION Dashboard 端到端自动化测试
# 在 tmux 里启动 dashboard，用 send-keys 模拟键盘，验证每个流程

set +e
cd /Users/xuyingzhou/Project/study-rust/ion

PASS=0; FAIL=0; CASES=()
check() {
  local name="$1"; local cond="$2"
  if eval "$cond"; then
    PASS=$((PASS+1)); CASES+=("✅ $name")
  else
    FAIL=$((FAIL+1)); CASES+=("❌ $name")
  fi
}

cleanup() {
  pkill -9 -f "target/debug/ion" 2>/dev/null
  pkill -9 -f "bun run src/index.ts" 2>/dev/null
  rm -f ~/.ion/manager.sock ~/.ion/manager.pid ~/.ion/tui-state.json
  tmux kill-session -t ion_e2e 2>/dev/null
  sleep 1
}
trap cleanup EXIT
cleanup

echo "╔══════════════════════════════════════════════╗"
echo "║   ION Dashboard 端到端测试                   ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# ── 启动 ──
tmux new-session -d -s ion_e2e -x 145 -y 42 \
  'bash -c "/Users/xuyingzhou/Project/study-rust/ion/target/debug/ion dashboard 2>&1"'
sleep 6

# CASE 1: dashboard 启动
PANE=$(tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null)
check "Case 1: ion dashboard 启动" '[ -n "$PANE" ] && echo "$PANE" | grep -q "ION\|Projects"'
check "Case 1b: 三栏布局渲染" 'echo "$PANE" | grep -q "Projects" && echo "$PANE" | grep -q "Workers"'
check "Case 1c: 日志容器存在" 'echo "$PANE" | grep -q "Logs"'

# CASE 2: 自动连 Manager（等 2s 后看 connected）
sleep 2
PANE=$(tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null)
# ● 字符在 tmux 抓取后可能被拆开，用更宽松的匹配
check "Case 2: 自动连接 Manager" 'echo "$PANE" | grep -qi "connected"'

# CASE 3: 用 RPC 创建 worker，看板自动刷新显示
echo '{"id":"c1","method":"create_worker","session":"sess_e2e_a","agent":"builder","model":"deepseek-v4"}' | nc -U ~/.ion/manager.sock > /dev/null 2>&1
echo '{"id":"c2","method":"create_worker","session":"sess_e2e_b","agent":"reviewer","model":"claude-opus-4"}' | nc -U ~/.ion/manager.sock > /dev/null 2>&1
sleep 2
PANE=$(tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null)
check "Case 3a: 看板显示 worker (≥1)" 'echo "$PANE" | grep -qE "Workers · [1-9]"'
check "Case 3b: 项目树显示项目" 'echo "$PANE" | grep -qE "ion|proj"'

# CASE 4: n 键打开创建模态
tmux send-keys -t ion_e2e n
sleep 1
PANE=$(tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null)
check "Case 4: n 键弹出创建模态" 'echo "$PANE" | grep -qi "create.*new.*worker\|Create New"'

# CASE 5: Esc 关闭模态
tmux send-keys -t ion_e2e Escape
sleep 1
PANE=$(tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null)
check "Case 5: Esc 关闭创建模态" '! echo "$PANE" | grep -qi "create.*new.*worker\|Create New"'

# CASE 6: Tab 切换焦点
tmux send-keys -t ion_e2e Tab
sleep 1
PANE=$(tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null)
# Tab 后焦点应在 kanban，状态栏显示
check "Case 6: Tab 切换焦点（不崩）" '[ -n "$PANE" ]'

# CASE 7: q 退出
tmux send-keys -t ion_e2e q
sleep 2
check "Case 7: q 退出 dashboard" '! pgrep -f "bun run src/index.ts" > /dev/null'

# ── 输出结果 ──
echo ""
echo "═══════════════════════════════════════"
for c in "${CASES[@]}"; do echo "  $c"; done
echo "═══════════════════════════════════════"
echo ""
echo "总计: $((PASS+FAIL)) 个  |  ✅ $PASS 通过  |  ❌ $FAIL 失败"
echo ""
