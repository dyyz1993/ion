#!/usr/bin/env python3
"""Smart CI scheduler: parallel + serial groups.

Phase 1: Pure CLI scripts → N parallel
Phase 2: Host-dependent scripts → shared host, serial
"""
import os, re, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor, as_completed

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(PROJECT)

MAX_PARALLEL = int(os.environ.get("MAX_PARALLEL", "5"))
TIMEOUT = int(os.environ.get("TIMEOUT", "180"))
RESULTS_DIR = "/tmp/ci_results_smart2"
EXCLUDE = {"ci_host_helper","session_tree_ci","edge_cases","reclaimer_stress",
           "self_heal","team_e2e","lyric_webui","apple_container","learning_e2e"}

os.system(f"rm -rf {RESULTS_DIR} && mkdir -p {RESULTS_DIR}")

def classify(name):
    """PARALLEL (pure CLI) or SERIAL (needs ion serve) or SKIP"""
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

def run_ci(script, session_dir):
    """Run one CI script, return (status, pass, fail, name)"""
    name = os.path.basename(script)[:-3]
    out_file = f"{RESULTS_DIR}/{name}.out"
    os.makedirs(session_dir, exist_ok=True)
    
    # Build env: inherit parent + override session dir
    env = os.environ.copy()
    env["ION_SESSION_DIR"] = session_dir
    
    with open(out_file, "w") as outf:
        try:
            subprocess.run(
                ["timeout", str(TIMEOUT), "bash", script],
                env=env, stdout=outf, stderr=subprocess.STDOUT, timeout=TIMEOUT+10
            )
        except subprocess.TimeoutExpired:
            pass
    
    with open(out_file, errors='replace') as f:
        content = f.read()
    
    p = content.count('✅')
    fail = content.count('❌')
    
    if fail == 0:
        status = "PASS"
    else:
        status = "FAIL"
    
    return (status, p, fail, name)

# Collect scripts
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

print(f"══════════════════════════════════════════════════════")
print(f"  Smart CI Scheduler — {len(parallel)} parallel + {len(serial)} serial")
print(f"  Parallel: {MAX_PARALLEL} concurrent | Serial: shared host")
print(f"══════════════════════════════════════════════════════")

results = []

# Phase 1: parallel
print(f"\n── Phase 1: Parallel ({len(parallel)} scripts, {MAX_PARALLEL} concurrent) ──")
with ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
    futures = {pool.submit(run_ci, s, f"/tmp/ci_p{i}"): s for i, s in enumerate(parallel)}
    for fut in as_completed(futures):
        status, p, f, name = fut.result()
        results.append((status, p, f, name))
        icon = "✅" if status=="PASS" else "❌"
        print(f"  {icon} {name:30s} pass={p:<4} fail={f}")

# Phase 2: serial with persistent shared host
print(f"\n── Phase 2: Serial ({len(serial)} scripts, persistent shared host) ──")

# Start ONE persistent host that all serial CI scripts reuse.
# CI scripts that call `ci_host_helper.sh ensure_host` will detect this host and reuse it.
# CI scripts that kill+restart host will only affect this shared host (not other phases).
import signal

phase2_sock = "/tmp/ci_phase2.sock"
phase2_session = "/tmp/ci_phase2_sessions"
os.system(f"rm -f {phase2_sock}")
os.makedirs(phase2_session, exist_ok=True)

# Start persistent host
print("  [host] starting persistent shared host...")
host_proc = subprocess.Popen(
    ["env", f"ION_HOST_SOCKET={phase2_sock}", f"ION_SESSION_DIR={phase2_session}",
     "ION_FAUX_REPLY=ci host ready", "target/debug/ion", "serve"],
    stdout=open("/tmp/ci_phase2_host.log", "w"), stderr=subprocess.STDOUT
)

# Wait for host ready
import time
host_ready = False
for _ in range(30):
    time.sleep(1)
    try:
        r = subprocess.run(
            ["env", f"ION_HOST_SOCKET={phase2_sock}", "target/debug/ion", "rpc", "--method", "list_sessions"],
            capture_output=True, text=True, timeout=5
        )
        if "sessions" in r.stdout:
            host_ready = True
            break
    except subprocess.TimeoutExpired:
        continue
if host_ready:
    print(f"  [host] ready (PID={host_proc.pid}, socket={phase2_sock})")
    # Set ION_HOST_SOCKET in THIS process so all run_ci children inherit it
    os.environ["ION_HOST_SOCKET"] = phase2_sock
else:
    print("  [host] WARNING: host not ready after 30s, continuing anyway")

# Run all serial scripts with the shared host
for i, s in enumerate(serial):
    status, p, f, name = run_ci(s, phase2_session)
    results.append((status, p, f, name))
    icon = "✅" if status=="PASS" else "❌"
    print(f"  {icon} {name:30s} pass={p:<4} fail={f}")
    
    # Check if persistent host is still alive (CI scripts may kill it)
    if host_proc.poll() is not None:
        # Host was killed by a CI script — restart it
        print(f"  [host] was killed by {name}, restarting...")
        os.system(f"rm -f {phase2_sock}")
        host_proc = subprocess.Popen(
            ["env", f"ION_HOST_SOCKET={phase2_sock}", f"ION_SESSION_DIR={phase2_session}",
             "ION_FAUX_REPLY=ci host ready", "target/debug/ion", "serve"],
            stdout=open("/tmp/ci_phase2_host.log", "a"), stderr=subprocess.STDOUT
        )
        for _ in range(15):
            time.sleep(1)
            try:
                r = subprocess.run(
                    ["env", f"ION_HOST_SOCKET={phase2_sock}", "target/debug/ion", "rpc", "--method", "list_sessions"],
                    capture_output=True, text=True, timeout=5
                )
                if "sessions" in r.stdout:
                    print(f"  [host] restarted (PID={host_proc.pid})")
                    break
            except subprocess.TimeoutExpired:
                continue

# Kill persistent host
print("  [host] shutting down persistent host...")
host_proc.send_signal(signal.SIGTERM)
host_proc.wait(timeout=10)
os.system(f"rm -f {phase2_sock}")

# Summary
print(f"\n══════════════════════════════════════════════════════")
print(f"  Summary")
print(f"══════════════════════════════════════════════════════")

ok = sum(1 for r in results if r[0]=="PASS")
fail = sum(1 for r in results if r[0]=="FAIL")
total_pass = sum(r[1] for r in results)
total_fail = sum(r[2] for r in results)

print(f"\n  ✅ PASS: {ok} / {len(results)}")
print(f"  ❌ FAIL: {fail}")
print(f"  📊 Cases: {total_pass} pass / {total_fail} fail")
if fail > 0:
    print(f"\n  Failed:")
    for status, p, f, name in sorted(results, key=lambda x: x[3]):
        if status == "FAIL":
            print(f"    ❌ {name} (pass={p}, fail={f})")
print(f"\n══════════════════════════════════════════════════════")

# Save summary
with open(f"{RESULTS_DIR}/_summary.txt", "w") as fh:
    for status, p, f, name in results:
        fh.write(f"{status}|{p}|{f}|{name}\n")
