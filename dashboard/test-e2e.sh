#!/bin/bash
# ION Dashboard 完整端到端测试
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
  rm -f ~/.ion/host.sock ~/.ion/host.pid ~/.ion/tui-state.json
  tmux kill-session -t ion_e2e 2>/dev/null
  sleep 1
}
trap cleanup EXIT
cleanup

echo "╔══════════════════════════════════════════════╗"
echo "║   ION Dashboard 完整端到端测试               ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# ── 启动 ──
tmux new-session -d -s ion_e2e -x 145 -y 42 \
  'bash -c "/Users/xuyingzhou/Project/study-rust/ion/target/debug/ion dashboard 2>&1"'
sleep 6
PANE() { tmux capture-pane -t ion_e2e -p -J -e 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g'; }

# ── 基础 ──
check "Case 1: dashboard 启动" 'PANE | grep -q "Projects"'
check "Case 2: 三栏布局" 'PANE | grep -q "Projects" && PANE | grep -q "Workers" && PANE | grep -q "Detail"'
check "Case 3: 日志容器" 'PANE | grep -q "Logs"'
sleep 2
check "Case 4: 自动连接 Manager" 'PANE | grep -qi "connected"'

# ── Worker 显示 ──
echo '{"id":"c1","method":"create_worker","session":"sess_a","agent":"builder","model":"deepseek-v4"}' | nc -U ~/.ion/host.sock > /dev/null 2>&1
echo '{"id":"c2","method":"create_worker","session":"sess_b","agent":"reviewer","model":"claude-opus-4"}' | nc -U ~/.ion/host.sock > /dev/null 2>&1
sleep 2
check "Case 5: 看板显示 worker" 'PANE | grep -qE "Workers · [1-9]"'
check "Case 6: 项目树显示" 'PANE | grep -qE "ion|proj"'

# ── n 键创建模态 ──
tmux send-keys -t ion_e2e n; sleep 1
check "Case 7: n 弹创建模态" 'PANE | grep -qi "Create New Worker"'
tmux send-keys -t ion_e2e Escape; sleep 1
check "Case 8: Esc 关闭模态" '! PANE | grep -qi "Create New Worker"'

# ── 输入框 ──
tmux send-keys -t ion_e2e i; sleep 1
tmux send-keys -t ion_e2e 'test message'; sleep 1
check "Case 9: 输入框接受字符" 'PANE | grep -q "test message"'
tmux send-keys -t ion_e2e Enter; sleep 2
check "Case 10: Enter 触发（提示选中 worker）" 'PANE | grep -q "选中\|select"'
tmux send-keys -t ion_e2e Escape; sleep 1

# ── 选中 worker 进 Focus ──
tmux send-keys -t ion_e2e Tab; sleep 1   # 焦点到 kanban
tmux send-keys -t ion_e2e Enter; sleep 2 # 选中 worker
check "Case 11: Enter 进 Focus 模式" 'PANE | grep -qi "Todo\|Output\|Memory"'

# ── q 退出 ──
tmux send-keys -t ion_e2e Escape; sleep 1  # 退出 focus
tmux send-keys -t ion_e2e q; sleep 2
check "Case 12: q 退出 dashboard" '! pgrep -f "bun run src/index.ts" > /dev/null'

# ── 输出 ──
echo ""
echo "═══════════════════════════════════════"
for c in "${CASES[@]}"; do echo "  $c"; done
echo "═══════════════════════════════════════"
echo ""
echo "总计: $((PASS+FAIL)) 个  |  ✅ $PASS 通过  |  ❌ $FAIL 失败"
[ $FAIL -eq 0 ] && echo "" && echo "🎉 全部通过！"
echo ""
