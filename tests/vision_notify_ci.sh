#!/bin/bash
# tests/vision_notify_ci.sh — 视觉输入 + 异步委派完成通知 + follow-up 队列 一键验证
#
# 覆盖（2026-08-27/28 三功能）：
#   G1 视觉输入：prompt 带 images（base64）→ JSONL 落盘 ContentBlock::Image
#      → 视觉模型答对图内容（纯色判别，不可能靠猜）
#   G2 异步委派通知：spawn_worker(wait=false) → 子 agent_end → 父收到【子任务完成】
#   G3 follow-up 队列：运行中 prompt(followUp) 排队 → 本轮结束自动消费开新轮
#
# 前置：ion serve 运行中（target/debug/ion），模型 zai/glm-5.3-flash（视觉）
# 可用 ION_CI_MODEL / ION_CI_PROVIDER 覆盖
# 用法：bash tests/vision_notify_ci.sh

set -u
ION_BIN="${ION_BIN:-$(dirname "$0")/../target/debug/ion}"
MODEL="${ION_CI_MODEL:-glm-5.3-flash}"
PROVIDER="${ION_CI_PROVIDER:-zai}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "✅ $1"; }
bad() { FAIL=$((FAIL+1)); echo "❌ $1"; }

# ── socket RPC 助手（每连接一行命令，按 id 拣包）──
rpc() { # rpc <id> <method> <session-or-empty> <params-json> [timeout]
  python3 - "$1" "$2" "$3" "$4" "${5:-150}" << 'PYEOF'
import socket, json, os, sys, time
rid, method, session, params, timeout = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], float(sys.argv[5])
cmd = {"id": rid, "method": method, "params": json.loads(params)}
if session: cmd["session"] = session
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.path.expanduser('~/.ion/host.sock'))
s.sendall((json.dumps(cmd) + "\n").encode())
s.settimeout(timeout)
buf = ""
reply = None
deadline = time.time() + timeout
while time.time() < deadline:
    try:
        chunk = s.recv(65536)
    except socket.timeout:
        break
    if not chunk: break
    buf += chunk.decode('utf-8', errors='replace')
    for line in buf.split("\n"):
        if not line.strip(): continue
        try: m = json.loads(line)
        except: continue
        if m.get("type") == "response" and m.get("id") == rid:
            reply = m; break
    if reply: break
s.close()
print(json.dumps(reply) if reply else "{}")
PYEOF
}

# ── 会话 JSONL 尾部 N 行 ──
tail_entries() { # tail_entries <sid>
  python3 - "$1" << 'PYEOF'
import glob, sys, json
sid = sys.argv[1]
fs = glob.glob(f'/Users/xuyingzhou/.ion/agent/sessions/*/{sid}.jsonl')
if not fs:
    print("[]")
else:
    print("".join(open(fs[0], errors='ignore').readlines()[-400:]) or "[]")
PYEOF
}

echo "═══ G1 视觉输入 ═══"
R=$(rpc "g1c" "create_session" "" "{\"session_id\":\"sess_ci_vis_$$\",\"agent\":\"developer\",\"model\":\"$MODEL\",\"project_path\":\"$PWD\"}")
echo "$R" | grep -qE '"success"\s*:\s*true' && ok "G1.1 create_session(视觉模型)" || bad "G1.1 create_session: $R"

# 纯红 64x64 PNG（CRC 自校验过的固定字节）
B64="iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAb0lEQVR4nO3PAQkAAAyEwO9feoshgnABdLep8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3IPanc8OLDQitxAAAAAElFTkSuQmCC"
R=$(rpc "g1p" "prompt" "sess_ci_vis_$$" "{\"text\":\"这张图片的主色是什么？只回答颜色中文名\",\"images\":[{\"data\":\"$B64\",\"mimeType\":\"image/png\"}]}" 180)
echo "$R" | grep -qE '"success"\s*:\s*true' && ok "G1.2 prompt(images) 受理" || bad "G1.2 prompt: $R"

sleep 30
ENTRIES=$(tail_entries "sess_ci_vis_$$")
echo "$ENTRIES" | grep -q '"Image"' && ok "G1.3 JSONL 落盘 Image 块" || bad "G1.3 未落盘 Image 块"
ANSWER=$(echo "$ENTRIES" | python3 -c "
import sys, json
texts = []
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try: e = json.loads(line)
    except: continue
    if e.get('type') == 'message' and 'Assistant' in e.get('message', {}):
        for b in e['message']['Assistant'].get('content', []):
            if 'Text' in b: texts.append(b['Text']['text'])
print(texts[-1] if texts else '')")
echo "$ANSWER" | grep -q "红" && ok "G1.4 模型答对主色（红）: $ANSWER" || bad "G1.4 回答异常: $ANSWER"

echo "═══ G2 异步委派完成通知 ═══"
R=$(rpc "g2c" "create_session" "" "{\"session_id\":\"sess_ci_ntf_$$\",\"agent\":\"developer\",\"model\":\"$MODEL\",\"project_path\":\"$PWD\"}")
echo "$R" | grep -qE '"success"\s*:\s*true' && ok "G2.1 create_session" || bad "G2.1: $R"
R=$(rpc "g2p" "prompt" "sess_ci_ntf_$$" "{\"text\":\"请用 spawn_worker 工具创建一个子 worker（务必传 wait=false 异步模式），任务让它'只回复两个字：完成'。spawn 之后你直接回复'已派活'结束本轮，不要调用 await_worker。\",\"behavior\":\"followUp\"}" 120)
echo "$R" | grep -qE '"success"\s*:\s*true' && ok "G2.2 父派活指令受理" || bad "G2.2: $R"

echo "  等待子完成 + 通知回灌（约 90s）…"
sleep 90
ENTRIES=$(tail_entries "sess_ci_ntf_$$")
echo "$ENTRIES" | grep -q "【子任务完成】" && ok "G2.3 父收到完成通知" || bad "G2.3 未收到通知"
RESULT=$(echo "$ENTRIES" | python3 -c "
import sys, json
found = False
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try: e = json.loads(line)
    except: continue
    if e.get('type') == 'message' and 'User' in e.get('message', {}):
        for b in e['message']['User'].get('content', []):
            if 'Text' in b and '子任务完成' in b['Text']['text']:
                print('found'); break
print('' if not found else 'found')")
[ "$RESULT" = "found" ] && ok "G2.4 通知含结果段" || bad "G2.4"

echo "═══ G3 follow-up 队列 ═══"
R=$(rpc "g3c" "create_session" "" "{\"session_id\":\"sess_ci_fu_$$\",\"agent\":\"developer\",\"model\":\"$MODEL\",\"project_path\":\"$PWD\"}")
echo "$R" | grep -qE '"success"\s*:\s*true' && ok "G3.1 create_session" || bad "G3.1: $R"
# 发一个慢任务进入 running，立刻跟 followUp
rpc "g3p1" "prompt" "sess_ci_fu_$$" "{\"text\":\"请用大约 300 字描写秋天的夜晚，慢慢写，写详细一些\",\"behavior\":\"followUp\"}" 180 > /dev/null
sleep 3
R=$(rpc "g3p2" "prompt" "sess_ci_fu_$$" "{\"text\":\"CI-FOLLOWUP-排队消息XYZ\",\"behavior\":\"followUp\"}" 30)
echo "$R" | grep -qE '"success"\s*:\s*true' && ok "G3.2 运行中 followUp 受理" || bad "G3.2: $R"

echo "  等本轮结束 + followUp 被消费（约 120s）…"
sleep 120
ENTRIES=$(tail_entries "sess_ci_fu_$$")
echo "$ENTRIES" | grep -q "CI-FOLLOWUP-排队消息XYZ" && ok "G3.3 followUp 落盘为用户消息" || bad "G3.3 未消费"
# 消费后应有新一轮 assistant 回复（在 followup user 之后）
CONSUMED=$(echo "$ENTRIES" | python3 -c "
import sys, json
seen_fu = False; after_assist = False
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try: e = json.loads(line)
    except: continue
    if e.get('type') != 'message': continue
    m = e.get('message', {})
    if 'User' in m:
        c = m['User'].get('content', [])
        t = c[0].get('Text', {}).get('text', '') if c else ''
        if 'CI-FOLLOWUP' in t: seen_fu = True
    elif 'Assistant' in m and seen_fu:
        after_assist = True
print('yes' if after_assist else 'no')")
[ "$CONSUMED" = "yes" ] && ok "G3.4 消费后开启新轮（assistant 作答）" || bad "G3.4 未开新轮"

echo "═══ 清理 ═══"
python3 - "$$" << 'PYEOF'
import json, os, glob
idx_path = os.path.expanduser('~/.ion/agent/sessions.index.json')
try:
    idx = json.load(open(idx_path))
    for k in list(idx.get('sessions', {})):
        if k.startswith(('sess_ci_vis_', 'sess_ci_ntf_', 'sess_ci_fu_')):
            del idx['sessions'][k]
    json.dump(idx, open(idx_path, 'w'), ensure_ascii=False, indent=1)
except Exception as e:
    print('索引清理失败:', e)
for pat in ('sess_ci_vis_*', 'sess_ci_ntf_*', 'sess_ci_fu_*'):
    for f in glob.glob(f'/Users/xuyingzhou/.ion/agent/sessions/*/{pat}.jsonl'):
        os.remove(f)
print('已清理 CI 会话')
PYEOF

echo "═══ 结果：$PASS 通过 / $FAIL 失败 ═══"
[ "$FAIL" = "0" ]
