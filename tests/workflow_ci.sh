#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：Workflow Engine 验证
# 覆盖 WORKFLOW_ENGINE.md 的 Group W1-W7
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || ION_BIN="ion"

TEST_ID=$$
TEST_DIR="${TMPDIR:-/tmp}/ion-wf-ci-$TEST_ID"
cleanup() { rm -rf "$TEST_DIR"; }
trap cleanup EXIT

setup_project() {
    rm -rf "$TEST_DIR"
    mkdir -p "$TEST_DIR/.ion/agents"
    cd "$TEST_DIR"
    git init -q
    echo "# test" > README.md
    git add . && git commit -q -m init
    echo '{"runtime":{"default_mode":"local"}}' > .ion/config.json
    cp "$PROJECT_DIR/examples/agents/wf.md" .ion/agents/ 2>/dev/null
    cp "$PROJECT_DIR/examples/agents/developer.md" .ion/agents/ 2>/dev/null
    cp "$PROJECT_DIR/examples/agents/merger.md" .ion/agents/ 2>/dev/null
}

echo "════════════════════════════════════════════════════"
echo "  ION Workflow CI Test — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null && pass "build" || { fail "build"; exit 1; }

# ──────────────────────────────────────────────────────────
# Group W1: DSL 校验
# ──────────────────────────────────────────────────────────
echo ""
echo "Group W1: DSL 校验"

# W1-1: 合法 workflow
setup_project
cp "$PROJECT_DIR/examples/workflows/delivery.wf.yaml" .ion/workflow.yaml
OUTPUT=$($ION_BIN workflow validate .ion/workflow.yaml 2>&1)
if echo "$OUTPUT" | grep -q "Valid"; then
    pass "W1-1: 合法 workflow 通过校验"
else
    fail "W1-1: 合法 workflow 校验失败"
fi

# W1-2: 缺必填字段
echo 'name: bad
stages:
  - id: x' > /tmp/bad_wf.yaml
OUTPUT=$($ION_BIN workflow validate /tmp/bad_wf.yaml 2>&1)
if echo "$OUTPUT" | grep -q "missing required"; then
    pass "W1-2: 缺 agent/commands 报错"
else
    fail "W1-2: 未报错"
fi

# W1-3: 坏 loop_back
echo 'name: bad
stages:
  - id: x
    agent: dev
    task: t
    on_fail:
      loop_back: nope' > /tmp/bad_wf2.yaml
OUTPUT=$($ION_BIN workflow validate /tmp/bad_wf2.yaml 2>&1)
if echo "$OUTPUT" | grep -q "nope"; then
    pass "W1-3: 坏 loop_back 报错"
else
    fail "W1-3: 未报错"
fi

# W1-4: agent + commands 互斥
echo 'name: bad
stages:
  - id: x
    agent: dev
    task: t
    commands: [echo hi]' > /tmp/bad_wf3.yaml
OUTPUT=$($ION_BIN workflow validate /tmp/bad_wf3.yaml 2>&1)
if echo "$OUTPUT" | grep -q "mutually exclusive"; then
    pass "W1-4: agent+commands 互斥报错"
else
    fail "W1-4: 未报错"
fi

# ──────────────────────────────────────────────────────────
# Group W2: 单 stage 执行
# ──────────────────────────────────────────────────────────
echo ""
echo "Group W2: 单 stage 执行"

# W2-1: gate 通过
setup_project
cat > .ion/workflow.yaml << 'WF'
name: simple
stages:
  - id: develop
    agent: developer
    task: "create hello.py with print('hello')"
    gate:
      command: "ls hello.py && echo EXISTS"
      expected: EXISTS
WF

OUTPUT=$(ION_HOST_TIMEOUT=120 timeout 150 $ION_BIN workflow run .ion/workflow.yaml 2>&1)
if echo "$OUTPUT" | grep -qi "complete\|all.*done\|pipeline"; then
    pass "W2-1: 单 stage gate 通过 → COMPLETE"
else
    fail "W2-1: 未完成"
fi
if [ -f "$TEST_DIR/hello.py" ]; then
    pass "W2-1b: hello.py 实际创建"
else
    fail "W2-1b: hello.py 未创建"
fi

# ──────────────────────────────────────────────────────────
# Group W3: 条件分支
# ──────────────────────────────────────────────────────────
echo ""
echo "Group W3: 条件分支"

# W3-2: if=false → skip
setup_project
cat > .ion/workflow.yaml << 'WF'
name: if-test
context:
  run_step2: false
stages:
  - id: step1
    agent: developer
    task: "create a.py with print('a')"
    gate:
      command: "ls a.py && echo EXISTS"
      expected: EXISTS
  - id: step2
    agent: developer
    task: "create b.py"
    if: "context.run_step2 == true"
WF

OUTPUT=$(ION_HOST_TIMEOUT=120 timeout 150 $ION_BIN workflow run .ion/workflow.yaml 2>&1)
if echo "$OUTPUT" | grep -qi "complete\|all.*done\|pipeline\|skipped"; then
    pass "W3-2: if=false → skip → COMPLETE"
else
    fail "W3-2: 未完成"
fi
if [ -f "$TEST_DIR/a.py" ] && [ ! -f "$TEST_DIR/b.py" ]; then
    pass "W3-2b: a.py 存在 + b.py 不存在（skip 正确）"
else
    fail "W3-2b: 文件状态不对"
fi

# ──────────────────────────────────────────────────────────
# Group W4: 上下文传递
# ──────────────────────────────────────────────────────────
echo ""
echo "Group W4: 上下文传递"

# W4-2: context 初始值
setup_project
cat > .ion/workflow.yaml << 'WF'
name: ctx-test
context:
  filename: "myctx.py"
stages:
  - id: develop
    agent: developer
    task: "create {{context.filename}} with print('ctx works')"
    gate:
      command: "ls myctx.py && echo EXISTS"
      expected: EXISTS
WF

OUTPUT=$(ION_HOST_TIMEOUT=120 timeout 150 $ION_BIN workflow run .ion/workflow.yaml 2>&1)
if [ -f "$TEST_DIR/myctx.py" ]; then
    pass "W4-2: context.filename → myctx.py 创建"
else
    fail "W4-2: myctx.py 未创建"
fi

# ──────────────────────────────────────────────────────────
# Group W5: 多 stage 串联
# ──────────────────────────────────────────────────────────
echo ""
echo "Group W5: 多 stage 串联"

# W5-1: develop → merge
setup_project
cat > .ion/workflow.yaml << 'WF'
name: multi
stages:
  - id: develop
    agent: developer
    task: "create calc.py with function add(a,b)"
    gate:
      command: "ls calc.py && echo EXISTS"
      expected: EXISTS
  - id: merge
    agent: merger
    task: "Merge any branches to master"
    if: "stages.develop.status == 'done'"
    gate:
      command: "git log --oneline -1 | grep -qi 'add\\|calc' && echo HAS || echo NO"
      expected: HAS
WF

OUTPUT=$(ION_HOST_TIMEOUT=180 timeout 210 $ION_BIN workflow run .ion/workflow.yaml 2>&1)
if echo "$OUTPUT" | grep -qi "complete\|all.*done\|pipeline"; then
    pass "W5-1: develop → merge → COMPLETE"
else
    fail "W5-1: 未完成"
fi
if [ -f "$TEST_DIR/calc.py" ]; then
    pass "W5-1b: calc.py 在 master 上"
else
    fail "W5-1b: calc.py 不在 master"
fi

# ──────────────────────────────────────────────────────────
# Group W7: 持久化 / 断点恢复
# ──────────────────────────────────────────────────────────
echo ""
echo "Group W7: 持久化 / 断点恢复"

# W7-1: 从 pending stage 恢复
setup_project
echo "print('a')" > a.py && git add a.py && git commit -q -m "step1"
cat > .ion/workflow.yaml << 'WF'
name: resume
stages:
  - id: step1
    agent: developer
    task: "create a.py"
    status: done
  - id: step2
    agent: developer
    task: "create b.py with print('b')"
    status: pending
    gate:
      command: "ls b.py && echo EXISTS"
      expected: EXISTS
WF

OUTPUT=$(ION_HOST_TIMEOUT=120 timeout 150 $ION_BIN workflow run .ion/workflow.yaml 2>&1)
if echo "$OUTPUT" | grep -qi "complete\|all.*done\|pipeline"; then
    pass "W7-1: 从 pending step2 恢复 → COMPLETE"
else
    fail "W7-1: 恢复失败"
fi
if [ -f "$TEST_DIR/b.py" ]; then
    pass "W7-1b: b.py 创建（step2 执行了）"
else
    fail "W7-1b: b.py 未创建"
fi

# W7-2: 全部 done 后 status 持久化
STATUS_OUTPUT=$($ION_BIN workflow status .ion/workflow.yaml 2>&1)
if echo "$STATUS_OUTPUT" | grep -q "step1.*done" && echo "$STATUS_OUTPUT" | grep -q "step2.*done"; then
    pass "W7-2: 全部 done 状态持久化"
else
    fail "W7-2: 状态不对"
fi

# ──────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "════════════════════════════════════════════════════"
[ $FAIL -eq 0 ]
