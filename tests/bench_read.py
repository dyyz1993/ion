#!/usr/bin/env python3
"""B5: 400MB 合成会话基准 — fast path vs 旧路径"""
import json, os, socket, time, urllib.request, sys

SOCK = os.path.expanduser("~/.ion/host.sock")
TMP = "/tmp/ion-bench-400mb"
os.makedirs(TMP, exist_ok=True)
BIG = f"{TMP}/bench_session.jsonl"

# ── 生成 ~400MB 合成会话（20000 轮，每轮 user+assistant+toolResult+大工具结果）──
if not os.path.exists(BIG) or os.path.getsize(BIG) < 400e6:
    print("生成 400MB 合成会话…")
    lines = []
    lines.append(json.dumps({"type":"session","version":3,"id":"bench_400mb","cwd":TMP,"timestamp":"2026-01-01T00:00:00.000Z"}))
    parent = "bench_400mb"
    big_tool_result = "x" * 8000  # 8KB per tool result
    for i in range(20000):
        uid = f"u{i}"
        lines.append(json.dumps({"type":"message","id":uid,"parentId":parent,"timestamp":f"2026-01-01T00:{i//60:02d}:{i%60:02d}.000Z","message":{"User":{"content":[{"Text":{"text":f"用户消息 {i}：请执行任务 {i}"}}],"role":"user"}}}))
        parent = uid
        aid = f"a{i}"
        lines.append(json.dumps({"type":"message","id":aid,"parentId":parent,"timestamp":f"2026-01-01T00:{i//60:02d}:{i%60:02d}.001Z","message":{"Assistant":{"content":[{"ToolCall":{"id":f"tc{i}","name":"bash","arguments":{"command":f"echo {i}"}}}],"role":"assistant"}}}))
        parent = aid
        tid = f"t{i}"
        lines.append(json.dumps({"type":"message","id":tid,"parentId":parent,"timestamp":f"2026-01-01T00:{i//60:02d}:{i%60:02d}.002Z","message":{"ToolResult":{"content":[{"Text":{"text":big_tool_result}}],"tool_call_id":f"tc{i}","tool_name":"bash","is_error":False,"role":"tool"}}}))
        parent = tid
        fid = f"f{i}"
        lines.append(json.dumps({"type":"message","id":fid,"parentId":parent,"timestamp":f"2026-01-01T00:{i//60:02d}:{i%60:02d}.003Z","message":{"Assistant":{"content":[{"Text":{"text":f"任务 {i} 完成"}}],"role":"assistant"}}}))
        parent = fid
    with open(BIG, "w") as f:
        f.write("\n".join(lines) + "\n")
    del lines

size_mb = os.path.getsize(BIG) / 1e6
print(f"合成会话: {size_mb:.0f}MB ({sum(1 for _ in open(BIG))} 行)")

# ── RPC helper ──
def rpc(method, params, timeout=60):
    s = socket.socket(socket.AF_UNIX); s.connect(SOCK)
    rid = f"bench_{time.time()}"
    s.sendall(json.dumps({"id":rid,"method":method,"params":params}).encode()+b"\n")
    buf = b""
    s.settimeout(timeout)
    while True:
        d = s.recv(1<<20)
        if not d: break
        buf += d
        for line in buf.split(b"\n"):
            try:
                m = json.loads(line)
                if m.get("id") == rid:
                    s.close()
                    return m
            except: pass
    s.close()
    return None

# ── 基准 ──
def bench(name, method, params, runs=3):
    times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        r = rpc(method, params)
        dt = (time.perf_counter() - t0) * 1000
        times.append(dt)
    avg = sum(times) / len(times)
    ok = r and r.get("success")
    total = r.get("data",{}).get("totalCount","?")
    msgs = len(r.get("data",{}).get("messages",[])) if "messages" in r.get("data",{}) else len(r.get("data",{}).get("turns",[]))
    print(f"  {name:30s} avg={avg:8.1f}ms  ok={ok}  total={total}  items={msgs}  times={[f'{t:.0f}' for t in times]}")
    return avg

session_param = {"session": BIG}

print(f"\n═══ 400MB 会话基准（{size_mb:.0f}MB）═══")
print("── fast path（FileIndex 索引 + 按需 read_at）──")
t1 = bench("get_messages 尾部 N=50", "get_session_messages", {**session_param, "limit": 50})
t2 = bench("get_messages 首次(建索引)", "get_session_messages", {**session_param, "limit": 50}, runs=1)
t3 = bench("get_messages head N=50", "get_session_messages", {**session_param, "limit": 50, "from": "head"})
t4 = bench("list_turns N=50", "list_session_turns", {**session_param, "limit": 50})

print(f"\n首次调用（索引构建）: {t2:.0f}ms")
print(f"后续命中（索引缓存）: get_messages={t1:.1f}ms  head={t3:.1f}ms  turns={t4:.1f}ms")
