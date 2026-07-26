#!/usr/bin/env bash
# self_heal_ci.sh — Self-Healing Pipeline CI 测试
#
# 验证 monitor → coordinator → developer → reviewer → merger → publisher 完整闭环。
# 使用 mock gh + test repo (ion-self-heal-test) 避免污染真实仓库。
#
# Group A: 单 issue 端到端（serial_skip）
# Group B: 多 issue 并行（concurrent）
# Group C: 失败处理（reviewer REQUEST_CHANGES）
# Group D: active state 持久化
#
# Usage: bash tests/self_heal_ci.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
TEST_REPO="/tmp/ion-self-heal-ci"
TEST_REMOTE="dyyz1993/ion-self-heal-ci"
PASS=0
FAIL=0
SERVE_PID=""

record_pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
record_fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

rpc_call() {
    local method="$1" params="$2" outfile="$3"
    "$ION" rpc --method "$method" --params "$params" > "$outfile" 2>/dev/null
}

cleanup_serve() {
    ps aux | grep "study-rust/ion/target/debug/ion" | grep -v grep | awk '{print $2}' | xargs kill -9 2>/dev/null
    rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
    sleep 2
}

setup_test_repo() {
    rm -rf "$TEST_REPO"
    mkdir -p "$TEST_REPO"
    cd "$TEST_REPO"
    git init -q
    git config user.email "ci@self-heal"
    git config user.name "CI"
    cat > Cargo.toml <<'EOF'
[package]
name = "ci-test"
version = "0.1.0"
edition = "2021"
[dependencies]
EOF
    mkdir -p src
    cat > src/lib.rs <<'EOF'
pub fn process(data: Option<String>) -> String {
    let s = data.unwrap();
    format!("[{}]", s)
}
EOF
    git add -A
    git commit -q -m "init"
    cd "$PROJECT_DIR"
}

create_issue() {
    local title="$1" body="$2"
    local issues_file="$TEST_REPO/issues.json"
    # Count existing issues to determine next number (avoid arithmetic on empty/multi-line)
    local count
    count=$(grep -o '"number"' "$issues_file" 2>/dev/null | wc -l | tr -d ' ')
    count=${count:-0}
    local num=$(( count + 1 ))
    # Build issues file fresh each time (simpler than incremental editing)
    python3 -c "
import json
issues = []
try:
    with open('$issues_file') as f: issues = json.load(f)
except: pass
issues.append({'number': $num, 'title': '$title'})
with open('$issues_file', 'w') as f: json.dump(issues, f)
" 2>/dev/null
    echo $num
}

setup_mock_gh() {
    mkdir -p /tmp/mock-gh-dir
    cat > /tmp/mock-gh-dir/gh <<'EOF'
#!/bin/sh
ISSUES_FILE="/tmp/ion-self-heal-ci/issues.json"
case "$1 $2" in
    "issue list")
        cat "$ISSUES_FILE" 2>/dev/null
        ;;
    "issue close")
        NUM="$3"
        # Remove the issue from the file
        python3 -c "
import json, sys
try:
    with open('$ISSUES_FILE') as f: d = json.load(f)
    d = [i for i in d if i.get('number') != $NUM]
    with open('$ISSUES_FILE', 'w') as f: json.dump(d, f)
except: pass
" 2>/dev/null
        echo "Closed issue #$NUM"
        ;;
    *) echo "" ;;
esac
EOF
    chmod +x /tmp/mock-gh-dir/gh
    export PATH="/tmp/mock-gh-dir:$PATH"
}

echo "=========================================="
echo "  Self-Healing Pipeline CI"
echo "=========================================="

# ── Setup ────────────────────────────────────────────
setup_test_repo
setup_mock_gh

# Enable monitor + global-memory in config
python3 -c "
import json
with open('$HOME/.ion/config.json') as f:
    d = json.load(f)
d.setdefault('extensions', {})['monitor'] = {'enabled': True}
with open('$HOME/.ion/config.json', 'w') as f:
    json.dump(d, f, indent=2)
" 2>/dev/null

# ── Group A: Single issue (serial_skip) ──────────────
echo ""
echo "=== Group A: 单 issue 端到端 ==="

# A1: Start serve + monitor + coordinator + verify pipeline
echo "--- A1: monitor → coordinator → developer → reviewer → merger → publisher ---"

# Setup: 1 issue
ISSUE_NUM=$(create_issue "test bug: process() panics on None" "Fix unwrap in src/lib.rs")

# Monitor config (event_only to avoid channel_send deadlock)
mkdir -p .ion/monitors
cat > .ion/monitors/ci-test.json <<EOF
{
  "name": "ci-test",
  "interval_secs": 5,
  "script": "PATH=/tmp/mock-gh-dir:\$PATH gh issue list --repo test --state open --json number,title 2>/dev/null",
  "agent": "coordinator",
  "prompt_template": "Issues",
  "mode": "concurrent",
  "trigger_mode": "event_only",
  "cooldown_secs": 300
}
EOF

cleanup_serve
nohup bash -c "cd $PROJECT_DIR && RUST_LOG=ion=info $ION serve" > /tmp/heal_ci.log 2>&1 &
SERVE_PID=$!
sleep 8

# Verify monitor triggered (wait up to 15s for first trigger)
TRIG=0
for i in 1 2 3; do
    sleep 5
    TRIG=$(awk '/monitor_triggered.*ci-test/{c++} END{print c+0}' /tmp/heal_ci.log)
    if [ "$TRIG" -ge 1 ]; then break; fi
done
if [ "$TRIG" -ge 1 ]; then
    record_pass "A1.1: monitor triggered ($TRIG times)"
else
    record_fail "A1.1: monitor not triggered"
fi

# Spawn coordinator with explicit task
COORD_SID=$(rpc_call create_session "{\"agent\":\"coordinator\",\"initial_prompt\":\"Fix issue #$ISSUE_NUM in $TEST_REPO. Run self-healing pipeline.\"}" /tmp/heal_a1_coord.json; python3 -c "import json; print(json.load(open('/tmp/heal_a1_coord.json')).get('data',{}).get('session_id',''))" 2>/dev/null)

if [ -n "$COORD_SID" ]; then
    record_pass "A1.2: coordinator spawned ($COORD_SID)"
else
    record_fail "A1.2: coordinator spawn failed"
fi

# Wait for pipeline (up to 3 min)
A1_FIXED=0
for i in $(seq 1 18); do
    sleep 10
    if grep -q "unwrap_or" "$TEST_REPO/src/lib.rs" 2>/dev/null; then
        A1_FIXED=1
        record_pass "A1.3: developer fixed code (unwrap → unwrap_or_default)"
        break
    fi
done
if [ "$A1_FIXED" = "0" ]; then
    record_fail "A1.3: developer didn't fix code (timeout)"
fi

# A2: Verify file modified
echo "--- A2: 验证代码修改 ---"
if [ -f "$TEST_REPO/src/lib.rs" ]; then
    if grep -q "unwrap_or_default\|unwrap_or" "$TEST_REPO/src/lib.rs"; then
        record_pass "A2: code modified with safer unwrap_or_default"
    else
        record_fail "A2: code not properly modified"
    fi
fi

# A3: Verify commit (wait up to 60s for developer to commit)
echo "--- A3: 验证 commit ---"
A3_COMMITTED=0
for i in 1 2 3 4 5 6; do
    sleep 10
    cd "$TEST_REPO"
    if git log --oneline 2>/dev/null | grep -qiE "fix|patch|update|resolve|process_output|panic|issue"; then
        A3_COMMITTED=1
        record_pass "A3: commit exists with fix/patch/update message"
        break
    fi
    cd "$PROJECT_DIR"
done
if [ "$A3_COMMITTED" = "0" ]; then
    record_fail "A3: no fix commit (timeout)"
fi
cd "$PROJECT_DIR"

# ── Group B: Parallel issues ────────────────────────
echo ""
echo "=== Group B: 多 issue 并行 ==="

# Reset test repo
setup_test_repo
ISSUE_1=$(create_issue "fix typo in README" "doc fix")
ISSUE_2=$(create_issue "add foo() function" "src/lib.rs add foo")
ISSUE_3=$(create_issue "add docs/file.txt" "new file")

# Spawn coordinator with 3 issues
COORD_B=$(rpc_call create_session "{\"agent\":\"coordinator\",\"initial_prompt\":\"3 independent issues: #$ISSUE_1 (README typo), #$ISSUE_2 (add foo), #$ISSUE_3 (add docs). All in $TEST_REPO. Process in PARALLEL.\"}" /tmp/heal_b.json; python3 -c "import json; print(json.load(open('/tmp/heal_b.json')).get('data',{}).get('session_id',''))" 2>/dev/null)

# Wait for parallel dev spawns (up to 2 min)
PARALLEL_SPAWN=0
for i in $(seq 1 12); do
    sleep 10
    DEV_COUNT=$(rpc_call list_workers '{}' /tmp/heal_b_workers.json; python3 -c "import json; d=json.load(open('/tmp/heal_b_workers.json')); print(sum(1 for w in d.get('data',{}).get('workers',[]) if w.get('agent')=='developer'))" 2>/dev/null)
    if [ "$DEV_COUNT" -ge 2 ]; then
        PARALLEL_SPAWN=1
        record_pass "B1: $DEV_COUNT developers spawned in parallel"
        break
    fi
done
if [ "$PARALLEL_SPAWN" = "0" ]; then
    record_fail "B1: no parallel developers (got $DEV_COUNT)"
fi

# ── Group C: Active state persistence ────────────────
echo ""
echo "=== Group C: active state 持久化 ==="

# C1: Test mark_active / check_active / release_active / list_active
echo "--- C1: mark_active + check_active ---"
rpc_call extension_rpc '{"extension":"monitor","method":"mark_active","args":{"monitor":"test","key":"issue-1","worker_id":"wkr_test","stage":"developer"}}' /tmp/heal_c1a.json
if python3 -c "import json; d=json.load(open('/tmp/heal_c1a.json')); exit(0 if d.get('success') else 1)" 2>/dev/null; then
    record_pass "C1.1: mark_active succeeded"
else
    record_fail "C1.1: mark_active failed"
fi

rpc_call extension_rpc '{"extension":"monitor","method":"check_active","args":{"monitor":"test","key":"issue-1"}}' /tmp/heal_c1b.json
if python3 -c "import json; d=json.load(open('/tmp/heal_c1b.json')); print(d.get('data',{}).get('active',''))" 2>/dev/null | grep -qi "true"; then
    record_pass "C1.2: check_active returns true"
else
    record_fail "C1.2: check_active didn't return true"
fi

echo "--- C2: list_active ---"
rpc_call extension_rpc '{"extension":"monitor","method":"list_active"}' /tmp/heal_c2.json
ACTIVE_COUNT=$(python3 -c "import json; d=json.load(open('/tmp/heal_c2.json')); print(len(d.get('data',{}).get('active',[])))" 2>/dev/null)
if [ "$ACTIVE_COUNT" -ge 1 ]; then
    record_pass "C2: list_active returns $ACTIVE_COUNT active"
else
    record_fail "C2: list_active empty"
fi

echo "--- C3: release_active ---"
rpc_call extension_rpc '{"extension":"monitor","method":"release_active","args":{"monitor":"test","key":"issue-1"}}' /tmp/heal_c3.json
if python3 -c "import json; d=json.load(open('/tmp/heal_c3.json')); exit(0 if d.get('success') else 1)" 2>/dev/null; then
    record_pass "C3.1: release_active succeeded"
else
    record_fail "C3.1: release_active failed"
fi

rpc_call extension_rpc '{"extension":"monitor","method":"check_active","args":{"monitor":"test","key":"issue-1"}}' /tmp/heal_c3b.json
if python3 -c "import json; d=json.load(open('/tmp/heal_c3b.json')); print(d.get('data',{}).get('active',''))" 2>/dev/null | grep -qi "false"; then
    record_pass "C3.2: after release, check_active returns false"
else
    record_fail "C3.2: check_active didn't return false after release"
fi

echo "--- C4: 持久化文件存在 ---"
if [ -f "$HOME/.ion/agent/active-pipelines.json" ]; then
    record_pass "C4: active-pipelines.json exists"
else
    record_fail "C4: active-pipelines.json missing"
fi

# ── Cleanup ─────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
kill $SERVE_PID 2>/dev/null
cleanup_serve
rm -rf "$TEST_REPO"
rm -f .ion/monitors/*.json
rm -f /tmp/heal_*.json /tmp/heal_ci.log

# ── Summary ─────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Self-Heal CI Summary"
echo "=========================================="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "=========================================="

[ "$FAIL" = "0" ]
