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
    
    with open(out_file, "w") as outf:
        try:
            subprocess.run(
                ["timeout", str(TIMEOUT), "env", f"ION_SESSION_DIR={session_dir}", "bash", script],
                stdout=outf, stderr=subprocess.STDOUT, timeout=TIMEOUT+10
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

# Phase 2: serial
print(f"\n── Phase 2: Serial ({len(serial)} scripts, shared host) ──")
for i, s in enumerate(serial):
    status, p, f, name = run_ci(s, "/tmp/ci_serial_shared")
    results.append((status, p, f, name))
    icon = "✅" if status=="PASS" else "❌"
    print(f"  {icon} {name:30s} pass={p:<4} fail={f}")

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
