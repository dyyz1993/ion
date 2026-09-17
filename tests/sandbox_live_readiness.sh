#!/usr/bin/env bash
# sandbox_live_readiness.sh — K5 三件真机验证「零准备演练」就绪脚本
#
# 目的：把"等授权窗口跑真机验证"的环境准备成本压到零——平时 --check/--plan 零副作用摸底，
#       授权窗口 --run 双重确认后一键执行。
#
# 用法:
#   sandbox_live_readiness.sh --check            就绪检查（零副作用：不 ssh、不写盘、
#                                                不读 auth.json、不打印任何密钥）
#   sandbox_live_readiness.sh --plan             打印三个套件的步骤/影响面/预计时长
#   sandbox_live_readiness.sh --run <suite>      真机执行（需 SANDBOX_LIVE_CONFIRM=YES
#                                                + 终端交互输入 yes，双重确认缺一不可）
#   suite ∈ {stateless | pump-e2e | policy-rpc | all}
#
# 套件 → 执行体映射:
#   stateless   → tests/sandbox_stateless_ci.sh        （K5 待办① 五步试炼）
#   pump-e2e    → tests/sandbox_live_drill.sh pump-e2e （K5 待办② 真沙盒审批泵）
#   policy-rpc  → tests/sandbox_live_drill.sh policy-rpc（K5 待办③ sandbox_policy RPC 真机设置）
#
# 端点环境变量（--check 探测 / --run 原样透传给执行体；缺省=生产 win38，🔴 注意）:
#   RW_HOST / RW_PORT / RW_USER / RW_KEY / RW_BIN / RW_NAME / RW_WRAPPER / ION_BIN
#
# 退出码: 0 成功/SKIP · 2 拒绝或参数错误 · 3 端点不可达（跑了也白跑）· 其他=执行体退出码
set -u
DIR_SELF="$(cd "$(dirname "$0")" && pwd)"
ION_BIN="${ION_BIN:-$DIR_SELF/../target/debug/ion}"
MODE="${1:---help}"

have() { command -v "$1" >/dev/null 2>&1; }

tcp_probe() { # tcp_probe <host> <port> — 纯 TCP 层探测，绝不 ssh
  nc -z -G 3 "$1" "$2" >/dev/null 2>&1
}

# ── 生效沙盒清单（ION_REMOTE_WORKERS env 优先 > ~/.ion/config.json 只读；绝不打印 key 字段值）──
collect_sandboxes() { # 输出: name|user@hostname:port|worker_bin|approval_policy|notes_count，每行一条；首行 "SOURCE=<env|config|none>"
  python3 - <<'PYEOF'
import json, os, sys

def sanitize(name, sb):
    user = sb.get('user', '')
    host = sb.get('hostname', '')
    port = sb.get('port', 22)
    dest = f"{user}@{host}:{port}" if user else f"{host}:{port}"
    wbin = os.path.basename(sb.get('worker_bin') or '/usr/local/bin/ion')
    pol = sb.get('approval_policy') or 'default'
    notes = len(sb.get('notes') or [])
    key = sb.get('key') or ''
    keydisp = os.path.basename(os.path.expanduser(key)) if key else '(默认密钥)'
    print(f"{name}|{dest}|{wbin}|{pol}|{notes}|{keydisp}")

env_json = os.environ.get('ION_REMOTE_WORKERS', '').strip()
if env_json:
    try:
        m = json.loads(env_json)
        print("SOURCE=env")
        for n, sb in m.items():
            sanitize(n, sb)
    except Exception as e:
        print(f"SOURCE=env-invalid ({e})")
else:
    cfg_path = os.path.expanduser('~/.ion/config.json')
    try:
        cfg = json.load(open(cfg_path))
        m = cfg.get('remote_workers') or {}
        if m:
            print("SOURCE=config (~/.ion/config.json, 只读)")
            for n, sb in m.items():
                sanitize(n, sb)
        else:
            print("SOURCE=none")
    except Exception:
        print("SOURCE=none")
PYEOF
}

# ── LLM 就绪（suite stateless / pump-e2e 需要；只报布尔，绝不打印密钥值）──
llm_ready() { # 输出: "yes|no <原因>"
  python3 - <<'PYEOF'
import json, os
cfg_path = os.path.expanduser('~/.ion/config.json')
try:
    cfg = json.load(open(cfg_path))
except Exception:
    print("no ~/.ion/config.json 不存在或不可解析"); raise SystemExit
model = cfg.get('default_model') or ''
prov = cfg.get('default_provider') or ''
if not model:
    print("no config.json 无 default_model"); raise SystemExit
has_key = False
auth_a = os.path.expanduser('~/.ion/auth.json')
auth_b = os.path.expanduser('~/.ion/agent/auth.json')
if os.path.exists(auth_a) or os.path.exists(auth_b):
    has_key = True
else:
    for p in (cfg.get('providers') or {}).values():
        if isinstance(p, dict) and p.get('api_key'):
            has_key = True; break
if has_key:
    print(f"yes default={prov}/{model}（密钥存在，值不展示）")
else:
    print("no 有 default_model 但未见 auth.json / providers.*.api_key")
PYEOF
}

# ── file-snapshot 就绪（suite pump-e2e 需要；写审批的触发器）──
fs_ready() {
  python3 - <<'PYEOF'
import json, os
try:
    cfg = json.load(open(os.path.expanduser('~/.ion/config.json')))
    ext = (cfg.get('extensions') or {}).get('file-snapshot') or {}
    print("yes" if ext.get('enabled') else "no config.json 未开 extensions.file-snapshot.enabled（写文件不会触发 ApprovalRequest）")
except Exception:
    print("no ~/.ion/config.json 不存在或不可解析")
PYEOF
}

# ── 套件依赖判定: readiness <suite> → 输出 "READY" 或 "MISSING|缺项1；缺项2" ──
suite_gaps() {
  local suite="$1" gaps=()
  [ -x "$ION_BIN" ] || gaps+=("ion 二进制未构建（$ION_BIN 不存在，先 cargo build --bin ion 或设 ION_BIN）")
  have nc    || gaps+=("缺 nc（TCP 探测用）")
  have ssh   || gaps+=("缺 ssh")
  have python3 || gaps+=("缺 python3")
  have jq    || gaps+=("缺 jq")
  if [ -n "${RW_KEY:-}" ] && [ ! -f "${RW_KEY/#\~/$HOME}" ]; then
    gaps+=("RW_KEY 指向的密钥文件不存在: $RW_KEY")
  fi
  local host="${RW_HOST:-192.168.0.38}" port="${RW_PORT:-2222}"
  if ! tcp_probe "$host" "$port"; then
    gaps+=("端点 $host:$port TCP 不可达（跑也会整组 SKIP）")
  fi
  case "$suite" in
    stateless|pump-e2e)
      local llm; llm=$(llm_ready)
      [ "${llm%% *}" = "yes" ] || gaps+=("LLM 就绪: ${llm#yes }")
      ;;
  esac
  if [ "$suite" = "pump-e2e" ]; then
    local fs; fs=$(fs_ready)
    [ "${fs%% *}" = "yes" ] || gaps+=("file-snapshot: ${fs#no }")
  fi
  if [ ${#gaps[@]} -eq 0 ]; then
    echo "READY"
  else
    local joined=""
    local g
    for g in "${gaps[@]}"; do
      joined+="${joined:+；}${g}"
    done
    echo "MISSING|${joined}"
  fi
}

drill_endpoint_line() {
  local host="${RW_HOST:-192.168.0.38}" port="${RW_PORT:-2222}" user="${RW_USER:-root}"
  local key="${RW_KEY:-}"
  local src="（缺省值）"
  [ -n "${RW_HOST:-}" ] && src="（RW_* env 覆盖）"
  local keydisp="(默认密钥/agent)"
  [ -n "$key" ] && keydisp="$(basename "$key")"
  echo "  端点: ${user}@${host}:${port}  密钥: ${keydisp}  来源: ${src}"
}

do_check() {
  echo "════════ 沙盒真机演练就绪检查（零副作用：未 ssh / 未写盘 / 未读 auth.json）════════"
  echo ""
  echo "── 1. 依赖工具 ──"
  local t
  for t in nc ssh python3 jq; do
    if have "$t"; then echo "  ✅ $t: $(command -v "$t")"; else echo "  ❌ $t: 缺失"; fi
  done
  echo ""
  echo "── 2. ion 二进制 ──"
  if [ -x "$ION_BIN" ]; then
    echo "  ✅ $ION_BIN"
    local v; v=$("$ION_BIN" --version 2>/dev/null | head -1)
    [ -n "$v" ] && echo "     版本: ${v}（远端 worker_bin 版本需一致，probe 时强校验）"
  else
    echo "  ❌ $ION_BIN 不存在/不可执行（cargo build --bin ion，或 ION_BIN=... 指向已构建产物）"
  fi
  echo ""
  echo "── 3. 演练端点（--run 将注入的目标） ──"
  drill_endpoint_line
  local host="${RW_HOST:-192.168.0.38}" port="${RW_PORT:-2222}"
  if [ -z "${RW_HOST:-}" ]; then
    echo "  🔴 当前是缺省值 = 生产 win38。换非生产端点: RW_HOST=... RW_PORT=... RW_USER=... $0 --run <suite>"
  fi
  if tcp_probe "$host" "$port"; then
    echo "  ✅ TCP 可达: ${host}:${port}（仅端口探测，未 ssh）"
  else
    echo "  ❌ TCP 不可达: ${host}:${port}（授权窗口未开/机器未上电/网络不通）"
  fi
  echo ""
  echo "── 4. 生效沙盒池定义 ──"
  collect_sandboxes | {
    local first; IFS= read -r first
    case "$first" in
      SOURCE=env-invalid*) echo "  ❌ ${first#SOURCE=}";;
      SOURCE=env*) echo "  来源: ${first#SOURCE=}（ION_REMOTE_WORKERS env 优先）";;
      SOURCE=config*) echo "  来源: ${first#SOURCE=}";;
      *) echo "  （无任何沙盒定义——演练会经 RW_* env 现场注入，不影响本项）";;
    esac
    local line
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      local name dest wbin pol notes keyd
      IFS='|' read -r name dest wbin pol notes keyd <<< "$line"
      echo "  · $name → $dest  bin=$wbin  policy=$pol  notes=$notes  key=$keyd"
    done
  }
  echo ""
  echo "── 5. LLM 就绪（套件 stateless / pump-e2e 需要；policy-rpc 隔离 HOME 不需要） ──"
  local llm; llm=$(llm_ready)
  if [ "${llm%% *}" = "yes" ]; then echo "  ✅ ${llm#yes }"; else echo "  ❌ ${llm#no }"; fi
  echo ""
  echo "── 6. file-snapshot 就绪（套件 pump-e2e 需要；写审批触发器） ──"
  local fs; fs=$(fs_ready)
  if [ "${fs%% *}" = "yes" ]; then echo "  ✅ 已启用"; else echo "  ❌ ${fs#no }"; fi
  echo ""
  echo "════════ 套件结论 ════════"
  local s verdict
  for s in stateless pump-e2e policy-rpc; do
    verdict=$(suite_gaps "$s")
    if [ "$verdict" = "READY" ]; then
      echo "  ✅ $s: 可执行 → $0 --run $s"
    else
      echo "  ❌ $s: 未就绪，缺 ${verdict#MISSING|}"
    fi
  done
  echo ""
  echo "执行体: tests/sandbox_stateless_ci.sh（①）/ tests/sandbox_live_drill.sh pump-e2e（②）/ policy-rpc（③）"
  echo "看步骤与影响面: $0 --plan"
  return 0
}

suite_plan() { # suite_plan <suite> — 单套件的步骤/影响面/时长（--run 拒绝时也复用这段）
  case "$1" in
    stateless)
      cat <<'EOF'
      套件① stateless（tests/sandbox_stateless_ci.sh — 无状态五步试炼）
        步骤: ① env 注入单端点起隔离 host（llm_bridge=true）② create_worker 真任务
              （真 LLM，远端写 /tmp 时间戳文件）③ ssh 精确 kill -9 远端 worker
              ④ 断言会话回流 Mac（ToolResult + Assistant）⑤ host 直读 get_session_messages
        影响面: 远端 /tmp 新增 1 个文件 + 远端被杀的只有本套件自己的 worker；
              本机产生 1 个真实会话文件（真 LLM 一次往返）
        预计时长: 2-4 分钟
EOF
      ;;
    pump-e2e)
      cat <<'EOF'
      套件② pump-e2e（tests/sandbox_live_drill.sh pump-e2e — 真沙盒审批泵）
        步骤: ① 真实 HOME 起 host，env 注入 auto_approve 沙盒 ② create_worker 真实写文件
              （触发 ApprovalRequest）③ 审批泵自动 review_approve_all ④ 断言
              SandboxAutoApproved 事件（含 workerId + 放行明细）⑤ review_pending 归零
        影响面: 远端 /tmp 新增 1 个文件；本机产生 1 个真实会话文件（真 LLM 一次往返）；
              审批在无人值守下被自动放行（这正是被验证的行为）
        前置: ~/.ion/config.json 已开 extensions.file-snapshot.enabled
        预计时长: 2-4 分钟
EOF
      ;;
    policy-rpc)
      cat <<'EOF'
      套件③ policy-rpc（tests/sandbox_live_drill.sh policy-rpc — sandbox_policy RPC 真机设置）
        步骤: ① 隔离 HOME 起 host（不碰真实 ~/.ion，无需 LLM）② list_sandboxes 视图
              ③ sandbox_probe 真 ssh 体检（可达 + 远端 --version 对齐）④ GET→SET
              auto_approve→回读 override→非法值拒绝→list 同步→SET 回 default
        影响面: 远端仅被 ssh 执行只读 --version；本机零持久化（隔离 HOME 用完即删）
        预计时长: ~30 秒
EOF
      ;;
  esac
}

do_plan() {
  cat <<'EOF'
════════ 沙盒真机验证「授权窗口演练」计划（K5 待办三件）════════

执行前提（详见 --check）: ion 二进制已构建、端点 TCP 可达、LLM 配置就绪（①②）、
file-snapshot 已开（②）。全部缺省端点 = 生产 win38（192.168.0.38:2222），
换非生产端点: RW_HOST=... RW_PORT=... RW_USER=... 前缀即可。

EOF
  suite_plan stateless
  echo ""
  suite_plan pump-e2e
  echo ""
  suite_plan policy-rpc
  cat <<'EOF'

════════ 执行方式与安全门 ════════

  $0 --run <suite>     # suite ∈ stateless | pump-e2e | policy-rpc | all
  双重确认缺一不可:
    1) 环境变量 SANDBOX_LIVE_CONFIRM=YES
    2) 终端交互输入 yes（非交互环境一律拒绝——防止脚本/CI 误触真机）
  缺确认时: 只打印将要做的事（端点/脚本/影响面），绝不执行。

  风险与红线:
    · 默认端点即生产 win38——开发机直跑 = 真 ssh 生产沙盒，确认前想清楚
    · 本脚本 cleanup 只 kill 自己记录的 PID，绝不宽泛 pkill
    · ③ 的 ssh 仅只读 --version；① 的 ssh 仅精确 kill 本套件自己的 worker
    · ①② 会消耗真实 LLM 配额（一次往返，可用便宜模型档）
EOF
}

confirm_interactive() { # 第二重确认：必须真人从终端输入 yes
  local ans=""
  local endpoint="${RW_HOST:-192.168.0.38}:${RW_PORT:-2222}"
  if [ -t 0 ]; then
    printf "🔴 即将对真机端点 %s 执行套件【%s】。输入 yes 确认执行 / 其他任意键取消: " "$endpoint" "$1"
    read -r ans
  else
    # 无 tty 的 stdin：尝试 /dev/tty（重定向失败静默——CI/非交互环境直接拒绝）
    if { printf "🔴 即将对真机端点 %s 执行套件【%s】。输入 yes 确认执行 / 其他任意键取消: " "$endpoint" "$1" > /dev/tty; } 2>/dev/null \
       && { read -r ans < /dev/tty; } 2>/dev/null; then
      :
    else
      echo "❌ 拒绝执行: 当前非交互环境（无终端），双重确认无法完成。"
      echo "   这是防误触设计——真机套件必须由人在终端前确认。"
      return 1
    fi
  fi
  [ "$ans" = "yes" ]
}

refuse_run() { # refuse_run <suite> <原因> — 打印将要做的事，绝不执行
  local suite="$1" reason="$2"
  echo "════════ 拒绝执行（${reason}）════════"
  echo ""
  echo "将要做的事（确认补齐后才会真正执行）:"
  drill_endpoint_line
  suite_plan "$suite"
  echo ""
  echo "确认方式（两步都齐才执行）:"
  echo "  1) export SANDBOX_LIVE_CONFIRM=YES"
  echo "  2) 在交互终端运行: $0 --run ${suite}，并输入 yes"
  return 2
}

do_run() {
  local suite="${1:-}"
  local script="" args=()
  case "$suite" in
    stateless)  script="$DIR_SELF/sandbox_stateless_ci.sh"; args=();;
    pump-e2e)   script="$DIR_SELF/sandbox_live_drill.sh"; args=(pump-e2e);;
    policy-rpc) script="$DIR_SELF/sandbox_live_drill.sh"; args=(policy-rpc);;
    all)        :;; # 下面特判
    *) echo "用法: $0 --run <stateless|pump-e2e|policy-rpc|all>"; return 2;;
  esac

  # 端点预检：不可达直接拒绝（执行体也只会整组 SKIP，没必要走确认流程）
  local host="${RW_HOST:-192.168.0.38}" port="${RW_PORT:-2222}"
  if ! tcp_probe "$host" "$port"; then
    echo "❌ 拒绝执行: 端点 $host:$port TCP 不可达（执行体会整组 SKIP，等授权窗口/机器上电后再来）。"
    return 3
  fi

  if [ "$suite" = "all" ]; then
    echo "════════ --run all：三套件将依次执行（①→②→③） ════════"
    suite_plan stateless; echo ""
    suite_plan pump-e2e; echo ""
    suite_plan policy-rpc
    if [ "${SANDBOX_LIVE_CONFIRM:-}" != "YES" ]; then
      echo ""
      echo "❌ 拒绝执行: 缺第一重确认 SANDBOX_LIVE_CONFIRM=YES（all 与单套件一样需要双重确认）"
      echo "   确认方式: export SANDBOX_LIVE_CONFIRM=YES 后在交互终端重跑并输入 yes"
      return 2
    fi
    confirm_interactive "all" || { echo ""; echo "❌ 拒绝执行: 终端二次确认未通过"; return 2; }
    local rc=0 worst=0 s
    for s in stateless pump-e2e policy-rpc; do
      echo ""
      echo "════════ 执行 $s ════════"
      case "$s" in
        stateless)  bash "$DIR_SELF/sandbox_stateless_ci.sh"; rc=$?;;
        pump-e2e)   bash "$DIR_SELF/sandbox_live_drill.sh" pump-e2e; rc=$?;;
        policy-rpc) bash "$DIR_SELF/sandbox_live_drill.sh" policy-rpc; rc=$?;;
      esac
      [ "$rc" -gt "$worst" ] && worst=$rc
    done
    echo ""
    echo "════════ all 完成（最差退出码=${worst}） ════════"
    return "$worst"
  fi

  # 单套件：第一重确认（env）
  if [ "${SANDBOX_LIVE_CONFIRM:-}" != "YES" ]; then
    refuse_run "$suite" "缺第一重确认 SANDBOX_LIVE_CONFIRM=YES"
    return 2
  fi
  # 第二重确认（终端交互）
  echo "将要做的事:"
  drill_endpoint_line
  suite_plan "$suite"
  echo ""
  if ! confirm_interactive "$suite"; then
    echo ""
    refuse_run "$suite" "终端二次确认未通过（未输入 yes）"
    return 2
  fi
  echo ""
  echo "════════ 确认齐备，执行 $suite → $(basename "$script") ${args[*]:-} ════════"
  echo ""
  bash "$script" "${args[@]}"
}

case "$MODE" in
  --check) do_check;;
  --plan)  do_plan;;
  --run)   shift || true; do_run "${1:-}";;
  *)
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    echo ""
    echo "（无参数 = 打印本帮助）"
    [ "$MODE" = "--help" ] || [ "$MODE" = "-h" ] && exit 0
    echo "未知模式: $MODE"
    exit 2
    ;;
esac
