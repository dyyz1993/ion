#!/usr/bin/env bash
# sandbox_live_readiness_ci.sh — 沙盒真机演练就绪脚本 CI（mock 级，零 ssh、零真机、零副作用）
#
# 验证面（tests/sandbox_live_readiness.sh 的可 mock 路径全覆盖）：
#   Phase 0  语法             — 三个脚本 bash -n
#   Phase A  --plan           — 退出 0；三套件 + 确认机制可见
#   Phase B  --check 零副作用 — 隔离 HOME 下运行后 HOME 零新文件；env 沙盒清单可见；
#                               不可达端点 → 套件结论"未就绪，缺 ..."
#   Phase C  --check 判定     — 本地 TCP listener 模拟可达端点 → policy-rpc READY
#                               （隔离 HOME 无 LLM）pump-e2e 仍"未就绪"（按套件差异化判定）
#   Phase D  --run 安全门     — 缺 env 确认拒（exit 2）/ env 有但非交互拒（exit 2）/
#                               未知套件（exit 2）/ 端点不可达拒（exit 3）
#   Phase E  执行体安全路径   — drill 无参用法（exit 2）；三执行体端点不可达整组 SKIP（exit 0，
#                               不产生任何 ssh/进程）；含 sandbox_stateless_ci.sh 早退路径回归
#
# 红线：不 ssh 任何机器；不起真 host；listener 只在 127.0.0.1 且按 PID 清理；绝不 pkill。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
READY="$PROJECT_DIR/tests/sandbox_live_readiness.sh"
DRILL="$PROJECT_DIR/tests/sandbox_live_drill.sh"
STATELESS="$PROJECT_DIR/tests/sandbox_stateless_ci.sh"
PASS=0; FAIL=0
pass() { printf '  ✅ %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  ❌ %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [ "$1" -eq 0 ]; then pass "$2"; else fail "$2"; fi; }

TEST_ROOT="$(mktemp -d /tmp/ion-lr-ci-XXXXXX)"
LISTEN_PID=""
cleanup() {
    [ -n "$LISTEN_PID" ] && { kill "$LISTEN_PID" 2>/dev/null; wait "$LISTEN_PID" 2>/dev/null; }; wait "$LISTEN_PID" 2>/dev/null
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

printf '%s\n' '════════════════════════════════════════════════════'
printf '%s\n' '  Sandbox Live Readiness CI — '"$(date)"
printf '%s\n' '════════════════════════════════════════════════════'

# ── 公共夹具：隔离 HOME + 假 ion 二进制 + 假沙盒注入 ──
B_HOME="$TEST_ROOT/home"
mkdir -p "$B_HOME"
DUMMY_ION="$TEST_ROOT/ion-dummy"   # 存在 + 可执行（--check 只验存在性，不执行真 ion）
touch "$DUMMY_ION"; chmod +x "$DUMMY_ION"
FAKE_SBX='{"ci-x":{"hostname":"ci-x.invalid","user":"ci","port":2222,"approval_policy":"auto_approve","notes":["ci note"],"key":"~/.ssh/id_ed25519"}}'

echo ""
echo "═ Phase 0: 语法"
bash -n "$READY"; check $? "bash -n sandbox_live_readiness.sh"
bash -n "$DRILL"; check $? "bash -n sandbox_live_drill.sh"
bash -n "$STATELESS"; check $? "bash -n sandbox_stateless_ci.sh"

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase A: --plan（给人看的演练计划）"
# ════════════════════════════════════════════════════════
PLAN_OUT=$(HOME="$B_HOME" bash "$READY" --plan 2>&1); PR=$?
check $PR "A1 --plan 退出 0"
echo "$PLAN_OUT" | grep -q 'stateless'; check $? "A2 计划含套件① stateless"
echo "$PLAN_OUT" | grep -q 'pump-e2e'; check $? "A3 计划含套件② pump-e2e"
echo "$PLAN_OUT" | grep -q 'policy-rpc'; check $? "A4 计划含套件③ policy-rpc"
echo "$PLAN_OUT" | grep -q 'SANDBOX_LIVE_CONFIRM'; check $? "A5 计划写明双重确认机制"
echo "$PLAN_OUT" | grep -q '预计时长'; check $? "A6 计划含预计时长"

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase B: --check 零副作用 + 清单/缺项（端点=不可达 .invalid）"
# ════════════════════════════════════════════════════════
CHK_OUT=$(HOME="$B_HOME" ION_BIN="$DUMMY_ION" RW_HOST="ci-x.invalid" RW_PORT="2222" RW_USER="ci" \
  ION_REMOTE_WORKERS="$FAKE_SBX" bash "$READY" --check 2>&1); CR=$?
check $CR "B1 --check 退出 0"
echo "$CHK_OUT" | grep -q 'ci-x'; check $? "B2 env 注入沙盒 ci-x 出现在清单"
echo "$CHK_OUT" | grep -q 'SOURCE=env\|ION_REMOTE_WORKERS'; check $? "B3 清单标注来源=env"
echo "$CHK_OUT" | grep -q 'id_ed25519'; check $? "B4 密钥仅展示文件名（不打印内容）"
echo "$CHK_OUT" | grep -q 'TCP 不可达'; check $? "B5 不可达端点如实标记"
echo "$CHK_OUT" | grep -q '未就绪，缺'; check $? "B6 套件结论给出'未就绪，缺什么'"
N=$(find "$B_HOME" -mindepth 1 2>/dev/null | wc -l | tr -d ' ')
[ "$N" = "0" ]; check $? "B7 零副作用：--check 未在隔离 HOME 留下任何文件（实测 $N 个）"

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase C: --check 判定（本地 listener 模拟可达端点 → 按套件差异化）"
# ════════════════════════════════════════════════════════
LP=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
python3 -c "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',$LP)); s.listen(5); time.sleep(120)" &
LISTEN_PID=$!
sleep 1
CHK2_OUT=$(HOME="$B_HOME" ION_BIN="$DUMMY_ION" RW_HOST="127.0.0.1" RW_PORT="$LP" \
  bash "$READY" --check 2>&1); CR2=$?
check $CR2 "C1 --check（可达端点）退出 0"
echo "$CHK2_OUT" | grep -q 'TCP 可达'; check $? "C2 端点标记可达"
echo "$CHK2_OUT" | grep -q '✅ policy-rpc: 可执行'; check $? "C3 policy-rpc 判定可执行（隔离 HOME 无需 LLM）"
echo "$CHK2_OUT" | grep -q '❌ pump-e2e: 未就绪'; check $? "C4 pump-e2e 判定未就绪（隔离 HOME 无 LLM 配置）"
echo "$CHK2_OUT" | grep -q '❌ stateless: 未就绪'; check $? "C5 stateless 判定未就绪（同上）"
{ kill "$LISTEN_PID" 2>/dev/null; wait "$LISTEN_PID" 2>/dev/null; }; LISTEN_PID=""

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase D: --run 安全门（可达端点 + 本地 listener；绝不进入执行体）"
# ════════════════════════════════════════════════════════
python3 -c "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',$LP)); s.listen(5); time.sleep(120)" &
LISTEN_PID=$!
sleep 1

D1_OUT=$(RW_HOST=127.0.0.1 RW_PORT="$LP" HOME="$B_HOME" bash "$READY" --run stateless </dev/null 2>&1); D1=$?
[ "$D1" = "2" ]; check $? "D1 缺 env 确认 → 拒绝（exit 2，实测 ${D1}）"
echo "$D1_OUT" | grep -q '拒绝执行'; check $? "D2 拒绝输出明示'拒绝执行'"
echo "$D1_OUT" | grep -q 'sandbox_stateless_ci.sh'; check $? "D3 拒绝时打印将执行的脚本（将要做的事）"
echo "$D1_OUT" | grep -q 'SANDBOX_LIVE_CONFIRM=YES'; check $? "D4 拒绝时给出确认补齐方法"

D2_OUT=$(RW_HOST=127.0.0.1 RW_PORT="$LP" HOME="$B_HOME" SANDBOX_LIVE_CONFIRM=YES \
  bash "$READY" --run stateless </dev/null 2>&1); D2=$?
[ "$D2" = "2" ]; check $? "D5 env 已确认但非交互 → 仍拒（exit 2，实测 ${D2}）"
echo "$D2_OUT" | grep -q '非交互'; check $? "D6 拒绝原因=非交互环境（防脚本/CI 误触真机）"
echo "$D2_OUT" | grep -q 'bash\|执行体\|确认'; check $? "D7 未出现执行成功的迹象"

D3_OUT=$(RW_HOST=127.0.0.1 RW_PORT="$LP" HOME="$B_HOME" bash "$READY" --run bogus </dev/null 2>&1); D3=$?
[ "$D3" = "2" ]; check $? "D8 未知套件 → 用法报错（exit 2，实测 ${D3}）"

D4_OUT=$(RW_HOST=127.0.0.1 RW_PORT=1 HOME="$B_HOME" SANDBOX_LIVE_CONFIRM=YES \
  bash "$READY" --run policy-rpc </dev/null 2>&1); D4=$?
[ "$D4" = "3" ]; check $? "D9 端点不可达 → 确认前即拒（exit 3，实测 ${D4}）"
{ kill "$LISTEN_PID" 2>/dev/null; wait "$LISTEN_PID" 2>/dev/null; }; LISTEN_PID=""

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase E: 执行体安全路径（不可达端点整组 SKIP；无 ssh 无进程残留）"
# ════════════════════════════════════════════════════════
E1_OUT=$(bash "$DRILL" </dev/null 2>&1); E1=$?
[ "$E1" = "2" ]; check $? "E1 drill 无套件参数 → 用法（exit 2，实测 ${E1}）"
echo "$E1_OUT" | grep -q 'sandbox_stateless_ci.sh'; check $? "E2 用法提示指向套件①脚本"

E2_OUT=$(RW_HOST=127.0.0.1 RW_PORT=1 HOME="$B_HOME" bash "$DRILL" policy-rpc </dev/null 2>&1); E2=$?
[ "$E2" = "0" ]; check $? "E3 drill policy-rpc 不可达 → 整组 SKIP（exit 0，实测 ${E2}）"
echo "$E2_OUT" | grep -q 'SKIP'; check $? "E4 SKIP 明示（绝不盲试 ssh）"

E3_OUT=$(RW_HOST=127.0.0.1 RW_PORT=1 HOME="$B_HOME" bash "$DRILL" pump-e2e </dev/null 2>&1); E3=$?
[ "$E3" = "0" ]; check $? "E5 drill pump-e2e 不可达 → 整组 SKIP（exit 0，实测 ${E3}）"

E4_OUT=$(RW_HOST=127.0.0.1 RW_PORT=1 HOME="$B_HOME" bash "$STATELESS" </dev/null 2>&1); E4=$?
[ "$E4" = "0" ]; check $? "E6 stateless 不可达 → 整组 SKIP（exit 0，实测 ${E4}；ION_SESSION_DIR 改动后早退回归）"

# ════════════════════════════════════════════════════════
echo ""
echo "═ Phase F: drill policy-rpc 全链 mock（fake-ion 模拟 serve/rpc，锁死真机路径断言逻辑）"
# ════════════════════════════════════════════════════════
# fake-ion：实现 drill 用到的三面（--version / serve 绑 unix socket / rpc 的
# list_sandboxes + sandbox_probe + sandbox_policy GET/SET），语义对齐 §3.4 真实响应形状。
# 策略覆盖记录在 FAKE_ION_STATE 文件——每次 rpc 都是新进程，靠它维持"写后回读"一致性。
FAKE_ION="$TEST_ROOT/fake-ion"
FAKE_STATE="$TEST_ROOT/fake-state"
cat > "$FAKE_ION" <<'PYEOF'
#!/usr/bin/env python3
import json, os, socket, sys, time

args = sys.argv[1:]
state_file = os.environ["FAKE_ION_STATE"]
name = os.environ["FAKE_ION_NAME"]
sb = json.loads(os.environ.get("ION_REMOTE_WORKERS", "{}")).get(name, {})

def emit(obj):
    print(json.dumps(obj)); sys.exit(0)

if args and args[0] == "--version":
    print("fake-ion 1.0.0"); sys.exit(0)

if args and args[0] == "serve":
    s = socket.socket(socket.AF_UNIX)
    s.bind(os.environ["ION_HOST_SOCKET"])
    s.listen(1)
    t = 0
    while t < 900 and os.path.exists(os.environ["ION_HOST_SOCKET"]):
        time.sleep(1); t += 1
    sys.exit(0)

if args and args[0] == "rpc":
    method = None; params = {}
    i = 1
    while i < len(args):
        if args[i] == "--method": method = args[i + 1]; i += 2
        elif args[i] == "--params": params = json.loads(args[i + 1]); i += 2
        else: i += 1
    override = None
    if os.path.exists(state_file):
        override = open(state_file).read().strip() or None
    birth = sb.get("approval_policy") or "default"
    if method == "list_sandboxes":
        pol = override or birth
        dest = os.environ.get("FAKE_ION_DEST") or f"{sb.get('user','')}@{sb.get('hostname','')}:{sb.get('port',22)}"
        emit({"success": True, "data": {"sandboxes": [{
            "name": name,
            "dest": dest,
            "health": "Unknown", "version": None,
            "approvalPolicy": pol, "notes": sb.get("notes", [])}]}})
    if method == "sandbox_probe":
        if params.get("name") != name:
            emit({"success": False, "error": "unknown sandbox"})
        emit({"success": True, "data": {"status": {"name": name, "health": "Reachable", "version": "0.4.1-fake"}}})
    if method == "sandbox_policy":
        key = params.get("host")
        if key != name:
            emit({"success": False, "error": "unknown sandbox"})
        want = params.get("policy")
        if want is None:
            emit({"success": True, "data": {"scope": "host", "key": key, "profile": birth,
                                            "effective": override or birth, "override": override}})
        if want not in ("auto_approve", "default"):
            emit({"success": False, "error": f"unknown policy '{want}' (expected auto_approve | default)"})
        open(state_file, "w").write(want)
        emit({"success": True, "data": {"scope": "host", "key": key, "policy": want, "effective": want}})
emit({"success": False, "error": "unknown command"})
PYEOF
chmod +x "$FAKE_ION"

python3 -c "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',$LP)); s.listen(5); time.sleep(120)" &
LISTEN_PID=$!
sleep 1
F_SBX='{"ci-x":{"hostname":"ci-x.invalid","user":"ci","port":2222,"notes":["ci note"]}}'
F_OUT=$(RW_HOST=127.0.0.1 RW_PORT="$LP" RW_USER="ci" RW_NAME="ci-x" HOME="$B_HOME" ION_BIN="$FAKE_ION" \
  FAKE_ION_STATE="$FAKE_STATE" FAKE_ION_NAME="ci-x" FAKE_ION_DEST="ci@127.0.0.1:${LP}" \
  ION_REMOTE_WORKERS="$F_SBX" \
  bash "$DRILL" policy-rpc </dev/null 2>&1); F1=$?
kill "$LISTEN_PID" 2>/dev/null; wait "$LISTEN_PID" 2>/dev/null; LISTEN_PID=""
check $F1 "F1 drill policy-rpc（mock 全链）退出 0（实测 ${F1}）"
echo "$F_OUT" | grep -q 'FAIL=0'; check $? "F2 断言零失败（FAIL=0）"
N_OK=$(echo "$F_OUT" | grep -c '✅' | tr -d ' ')
[ "${N_OK:-0}" -ge 15 ]; check $? "F3 至少 15 项断言全过（实测 ${N_OK} 项 ✅）"
echo "$F_OUT" | grep -q 'B3 远端 worker_bin 版本回读'; check $? "F4 probe 版本回读断言被真实执行"
echo "$F_OUT" | grep -q '✅ D2'; check $? "F5 SET→GET override 链路成立"
echo "$F_OUT" | grep -q '✅ F1'; check $? "F6 覆盖回滚断言被真实执行"

echo ""
echo "════════════════════════════════════════════════════"
echo "结果: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
