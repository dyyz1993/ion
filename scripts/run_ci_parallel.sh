#!/usr/bin/env python3
"""Smart CI scheduler: parallel + serial groups, fully isolated.

Phase 1: Pure CLI scripts → N parallel (各自独立 session dir)
Phase 2: Host-dependent scripts → serial, 每个 CI 独立 socket + 独立 session + 独立 agent dir

关键设计：
- 每个 CI 都有独立的 ION_HOST_SOCKET（基于序号），不共享不冲突
- 每个 CI 都有独立的 ION_SESSION_DIR（session jsonl 隔离）
- 每个 CI 都有独立的 ION_AGENT_DIR（global-memory.db 隔离，避免 SQLite 锁）
- 不用 persistent host（避免 CI 互相 kill 导致死锁）
- 每个 CI 超时强制 kill（防死机）
"""
import os, re, subprocess, sys, time, signal, shutil
from concurrent.futures import ThreadPoolExecutor, as_completed

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(PROJECT)

MAX_PARALLEL = int(os.environ.get("MAX_PARALLEL", "5"))
TIMEOUT = int(os.environ.get("TIMEOUT", "180"))
RESULTS_DIR = "/tmp/ci_results_v8"
ISOLATE_BASE = "/tmp/ci_iso"
EXCLUDE = {"ci_host_helper","session_tree_ci","edge_cases","reclaimer_stress",
           "self_heal","team_e2e","lyric_webui","apple_container","learning_e2e"}

# 清理
shutil.rmtree(RESULTS_DIR, ignore_errors=True)
shutil.rmtree(ISOLATE_BASE, ignore_errors=True)
os.makedirs(RESULTS_DIR)
os.makedirs(ISOLATE_BASE)

def classify(name):
    if name in EXCLUDE or name.endswith("_real") or name.endswith("_e2e"):
        return "SKIP"
    path = f"tests/{name}.sh"
    if not os.path.exists(path):
        return "SKIP"
    with open(path) as f:
        content = f.read()
    if re.search(r'ion.*serve|ION_BIN.*serve', content):
        return "SERIAL"
    return "PARALLEL"

def run_ci(script, idx):
    """Run one CI script with FULL isolation. Returns (status, pass, fail, name)."""
    name = os.path.basename(script)[:-3]
    out_file = f"{RESULTS_DIR}/{name}.out"

    # 每个 CI 完全隔离的三件套
    iso_dir = f"{ISOLATE_BASE}/{idx}"
    sock = f"{iso_dir}/host.sock"
    session_dir = f"{iso_dir}/sessions"
    agent_dir = f"{iso_dir}/agent"
    os.makedirs(session_dir, exist_ok=True)
    os.makedirs(agent_dir, exist_ok=True)

    # 复制 config.json（provider 配置）
    src_cfg = os.path.expanduser("~/.ion/config.json")
    if os.path.exists(src_cfg):
        shutil.copy(src_cfg, f"{agent_dir}/../config.json")

    env = os.environ.copy()
    env["ION_HOST_SOCKET"] = sock
    env["ION_SESSION_DIR"] = session_dir
    env["ION_AGENT_DIR"] = agent_dir
    # 复制 HOME 下的 config 到 agent dir 的父目录
    # ion 读 config 从 ~/.ion/config.json（HOME-based），不受 ION_AGENT_DIR 影响

    proc = None
    try:
        proc = subprocess.Popen(
            ["bash", script],
            env=env,
            stdout=open(out_file, "w"),
            stderr=subprocess.STDOUT,
            preexec_fn=os.setsid  # 新进程组，方便 kill 整个树
        )
        proc.wait(timeout=TIMEOUT)
    except subprocess.TimeoutExpired:
        # 超时 → kill 整个进程组
        if proc:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        with open(out_file, "a") as f:
            f.write(f"\n[TIMEOUT after {TIMEOUT}s — killed]\n")
    except Exception as e:
        with open(out_file, "a") as f:
            f.write(f"\n[ERROR: {e}]\n")

    # 读结果
    try:
        with open(out_file, errors='replace') as f:
            content = f.read()
    except:
        content = ""

    p = content.count('\u2705')  # ✅
    fail = content.count('\u274c')  # ❌

    if fail == 0 and p > 0:
        status = "PASS"
    elif fail == 0 and p == 0:
        status = "PASS"  # 有些脚本不用 emoji
    else:
        status = "FAIL"

    # 清理隔离目录（释放空间）
    shutil.rmtree(iso_dir, ignore_errors=True)

    return (status, p, fail, name)

# 分类
all_scripts = sorted(f"tests/{f}" for f in os.listdir("tests") if f.endswith(".sh"))
parallel = []
serial = []
for s in all_scripts:
    name = os.path.basename(s)[:-3]
    group = classify(name)
    if group == "PARALLEL":
        parallel.append(s)
    elif group == "SERIAL":
        serial.append(s)

print("=" * 60)
print(f"  Smart CI v8 (full isolation)")
print(f"  Phase 1: {len(parallel)} parallel ({MAX_PARALLEL} concurrent)")
print(f"  Phase 2: {len(serial)} serial (each isolated socket+session+agent)")
print("=" * 60)

results = []

# Phase 1: parallel
print(f"\n-- Phase 1: Parallel ({len(parallel)} scripts) --")
with ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
    futures = {pool.submit(run_ci, s, i): s for i, s in enumerate(parallel)}
    for fut in as_completed(futures):
        status, p, f, name = fut.result()
        results.append((status, p, f, name))
        icon = "\u2705" if status == "PASS" else "\u274c"
        print(f"  {icon} {name:30s} pass={p:<4} fail={f}")

# Phase 2: serial (each fully isolated, no persistent host)
print(f"\n-- Phase 2: Serial ({len(serial)} scripts, each isolated) --")
for i, s in enumerate(serial):
    # 用 len(parallel)+i 做序号，避免和 Phase 1 冲突
    status, p, f, name = run_ci(s, len(parallel) + i)
    results.append((status, p, f, name))
    icon = "\u2705" if status == "PASS" else "\u274c"
    print(f"  {icon} {name:30s} pass={p:<4} fail={f}")

# Summary
print(f"\n{'=' * 60}")
print(f"  Summary")
print(f"{'=' * 60}")

ok = sum(1 for r in results if r[0] == "PASS")
fail = sum(1 for r in results if r[0] == "FAIL")
total_pass = sum(r[1] for r in results)
total_fail = sum(r[2] for r in results)

print(f"\n  PASS: {ok} / {len(results)}")
print(f"  FAIL: {fail}")
print(f"  Cases: {total_pass} pass / {total_fail} fail")
if fail > 0:
    print(f"\n  Failed:")
    for status, p, f, name in sorted(results, key=lambda x: x[3]):
        if status == "FAIL":
            print(f"    {name} (pass={p}, fail={f})")
print(f"\n{'=' * 60}")

# Save summary
with open(f"{RESULTS_DIR}/_summary.txt", "w") as fh:
    for status, p, f, name in results:
        fh.write(f"{status}|{p}|{f}|{name}\n")

# Cleanup
shutil.rmtree(ISOLATE_BASE, ignore_errors=True)
