#!/usr/bin/env python3
"""generate_summary_report.py — 从验收结果生成汇总 HTML 报告

用法: python3 scripts/generate_summary_report.py [--report-dir /tmp/ext_parallel_reports]

读每个场景的 result + report.json + HTML，提取：
- 场景名、模块、状态、指标通过率
- 场景描述（从 ext_scenarios.sh 的场景定义提取）
- LLM 实际行为（工具调用统计、耗时）
- 验收详情（哪些指标 PASS/FAIL/INFO）

生成一个单页 index.html 汇总，每行展开有场景总结。
"""
import json, os, sys, glob, re
from collections import defaultdict
from datetime import datetime

REPORT_DIR = sys.argv[sys.argv.index("--report-dir") + 1] if "--report-dir" in sys.argv else "/tmp/ext_parallel_reports"
SCENARIOS_FILE = os.path.join(os.path.dirname(__file__), "ext_scenarios.sh")

def load_scenario_descriptions():
    """从 ext_scenarios.sh 提取场景 ID → (name, ext_id) 映射"""
    descs = {}
    if not os.path.exists(SCENARIOS_FILE):
        return descs
    with open(SCENARIOS_FILE) as f:
        for line in f:
            # Format: "XX-SY|EXT-XX|场景名|prompt...|setup|metrics"
            m = re.match(r'\s*"(\d+-S\d+)\|(EXT-\d+)\|([^|]+)\|', line)
            if m:
                sid, ext_id, name = m.groups()
                descs[sid] = {"name": name, "ext_id": ext_id}
    return descs

def load_results():
    """读所有 .result 文件"""
    results = []
    for rf in sorted(glob.glob(os.path.join(REPORT_DIR, "*.result"))):
        with open(rf) as f:
            line = f.read().strip()
        parts = line.split("|")
        if len(parts) < 6:
            continue
        sid, status, tpass, tfail, elapsed, html = parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]
        results.append({
            "sid": sid,
            "status": status,
            "pass": int(tpass) if tpass.isdigit() else 0,
            "fail": int(tfail) if tfail.isdigit() else 0,
            "elapsed": elapsed,
            "html": os.path.basename(html) if html else "",
        })
    return results

def load_report_details(sid):
    """读单个场景的 report.json，返回指标详情"""
    rj = os.path.join(REPORT_DIR, f"{sid}_report.json")
    if not os.path.exists(rj):
        return {"checks": [], "passed": 0, "failed": 0}
    try:
        with open(rj) as f:
            return json.load(f)
    except:
        return {"checks": [], "passed": 0, "failed": 0}

def extract_tool_stats(html_path):
    """从 HTML 提取工具调用统计"""
    if not html_path or not os.path.exists(html_path):
        return {}
    try:
        with open(html_path, encoding='utf-8', errors='replace') as f:
            dom = f.read()
    except:
        return {}
    tools = {}
    for tool in ["bash", "read", "write", "edit", "grep", "memory_save", "memory_search",
                  "goal_set", "goal_refine", "plan_enter", "plan_add", "plan_exit",
                  "extension_rpc", "get_background_process", "kill_process"]:
        count = len(re.findall(rf'tool-header[^>]*>{tool}', dom, re.I))
        if count > 0:
            tools[tool] = count
    # HTML size as proxy for richness
    tools["_html_size"] = os.path.getsize(html_path) if os.path.exists(html_path) else 0
    return tools

def summarize_scenario(result, desc, report, tool_stats):
    """生成单个场景的自然语言总结"""
    sid = result["sid"]
    status = result["status"]
    name = desc.get("name", "?") if desc else "?"
    ext_id = desc.get("ext_id", "") if desc else ""
    elapsed = result["elapsed"]
    npass = result["pass"]
    nfail = result["fail"]
    total = npass + nfail

    parts = []

    # Status summary
    if status == "PASS":
        parts.append(f"<b>PASS</b> — {name}。{npass}/{total} 项指标全部通过")
    elif status == "FAIL":
        failed_checks = [c for c in report.get("checks", []) if c.get("status") == "FAIL"]
        fail_names = ", ".join(c["id"] for c in failed_checks[:3])
        parts.append(f"<b>FAIL</b> — {name}。{npass}/{total} 通过，{nfail} 项未过（{fail_names}）")
    else:
        parts.append(f"<b>ERROR</b> — {name}。执行异常（可能 session 导出失败）")

    # What LLM did (tool stats)
    tool_summary = ", ".join(f"{k}×{v}" for k, v in tool_stats.items() if not k.startswith("_"))
    if tool_summary:
        parts.append(f"LLM 调用工具：{tool_summary}")

    # Elapsed
    if elapsed and elapsed != "0":
        parts.append(f"耗时 {elapsed}s")

    # Key findings from checks
    ext_checks = [c for c in report.get("checks", []) if c["id"].startswith(sid.split("-")[0] + "-")]
    if ext_checks:
        passed_names = [c["name"] for c in ext_checks if c["status"] == "PASS"]
        if passed_names:
            parts.append(f"模块行为验证：{'; '.join(passed_names[:3])}")

    return "。".join(parts) + "。"

def generate_html(results, descs):
    """生成汇总 HTML"""
    npass = sum(1 for r in results if r["status"] == "PASS")
    nfail = sum(1 for r in results if r["status"] == "FAIL")
    nerror = sum(1 for r in results if r["status"] == "ERROR")
    total = len(results)
    rate = npass * 100 // total if total else 0
    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M")

    # Group by module
    by_mod = defaultdict(list)
    for r in results:
        ext = r["sid"].split("-")[0]
        by_mod[ext].append(r)

    # Module summary
    mod_rows = []
    for ext in sorted(by_mod):
        mod_results = by_mod[ext]
        mp = sum(1 for r in mod_results if r["status"] == "PASS")
        mf = sum(1 for r in mod_results if r["status"] == "FAIL")
        me = sum(1 for r in mod_results if r["status"] == "ERROR")
        mt = len(mod_results)
        ext_name = {"02":"global-memory","03":"dev_server","04":"file_snapshot","05":"lsp",
                     "06":"hooks","07":"goal_supervisor","08":"MonitorExtension","09":"bash",
                     "10":"MemoryExt-v0.1","11":"rules-engine","12":"learning","13":"permission",
                     "14":"file-approval","15":"context-index","16":"SessionIndex","17":"ContextReclaimer",
                     "18":"file-time-guard","19":"PlanExtension","20":"ToolLoopDetector","21":"internal_agent",
                     "22":"auto-session-title","23":"WorkflowExtension","24":"streaming"}.get(ext, ext)
        rate_m = mp * 100 // mt if mt else 0
        badge = "✅" if mf == 0 and me == 0 else "⚠️" if mp > 0 else "❌"
        mod_rows.append(f'<tr><td>{badge} EXT-{ext}</td><td>{ext_name}</td><td>{mp}/{mt}</td><td>{rate_m}%</td></tr>')

    # Scenario rows with summaries
    scenario_rows = []
    for r in results:
        sid = r["sid"]
        desc = descs.get(sid, {})
        report = load_report_details(sid)
        html_path = os.path.join(REPORT_DIR, r["html"]) if r["html"] else ""
        tool_stats = extract_tool_stats(html_path)
        summary = summarize_scenario(r, desc, report, tool_stats)
        status_class = f"status-{r['status']}"
        html_link = f'<a href="{r["html"]}">{r["html"] or "-"}</a>' if r["html"] else "-"

        # Detailed checks (expandable)
        checks_html = ""
        for c in report.get("checks", []):
            icon = {"PASS": "✅", "FAIL": "❌", "INFO": "ℹ️"}.get(c.get("status",""), "❓")
            checks_html += f'<span class="check {c.get("status","").lower()}">{icon} {c["id"]}: {c["name"]}</span>'

        scenario_rows.append(f'''
        <div class="scenario-card {status_class}">
          <div class="scenario-header">
            <span class="sid">{sid}</span>
            <span class="status-badge {status_class}">{r["status"]}</span>
            <span class="metrics">{r["pass"]}/{r["pass"]+r["fail"]}</span>
            <span class="elapsed">{r["elapsed"]}s</span>
            <a class="html-link" href="{r["html"]}">📄 HTML</a>
          </div>
          <div class="summary">{summary}</div>
          <div class="checks">{checks_html}</div>
        </div>''')

    html = f'''<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8">
<title>ION 扩展全量验收报告 {timestamp}</title>
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{ font-family: -apple-system, "PingFang SC", sans-serif; background: #f6f8fa; color: #24292e; line-height: 1.6; }}
.container {{ max-width: 1200px; margin: 0 auto; padding: 40px 20px; }}
h1 {{ font-size: 28px; margin-bottom: 8px; }}
.meta {{ color: #586069; margin-bottom: 24px; }}
.overview {{ display: flex; gap: 16px; margin-bottom: 32px; flex-wrap: wrap; }}
.stat-card {{ background: white; padding: 20px 28px; border-radius: 10px; box-shadow: 0 2px 8px rgba(0,0,0,0.08); text-align: center; min-width: 120px; }}
.stat-card .num {{ font-size: 2.2em; font-weight: 700; }}
.stat-card .label {{ color: #586069; font-size: 0.9em; }}
.stat-pass .num {{ color: #28a745; }}
.stat-fail .num {{ color: #cb2431; }}
.stat-error .num {{ color: #d73a49; }}
.stat-rate .num {{ color: #0366d6; }}

h2 {{ font-size: 20px; margin: 32px 0 12px; border-bottom: 2px solid #e1e4e8; padding-bottom: 8px; }}

table {{ width: 100%; border-collapse: collapse; background: white; border-radius: 8px; overflow: hidden; box-shadow: 0 1px 4px rgba(0,0,0,0.06); }}
th, td {{ padding: 10px 14px; text-align: left; border-bottom: 1px solid #e1e4e8; }}
th {{ background: #f1f8ff; font-weight: 600; }}

.scenario-card {{ background: white; border-radius: 8px; padding: 16px 20px; margin-bottom: 12px; box-shadow: 0 1px 3px rgba(0,0,0,0.06); border-left: 4px solid #e1e4e8; }}
.scenario-card.status-PASS {{ border-left-color: #28a745; }}
.scenario-card.status-FAIL {{ border-left-color: #cb2431; }}
.scenario-card.status-ERROR {{ border-left-color: #d73a49; }}
.scenario-header {{ display: flex; align-items: center; gap: 12px; margin-bottom: 8px; }}
.sid {{ font-weight: 700; font-size: 1.1em; min-width: 60px; }}
.status-badge {{ padding: 2px 10px; border-radius: 12px; font-size: 0.85em; font-weight: 600; }}
.status-badge.status-PASS {{ background: #dcfce7; color: #166534; }}
.status-badge.status-FAIL {{ background: #fee2e2; color: #991b1b; }}
.status-badge.status-ERROR {{ background: #fce7f3; color: #9d174d; }}
.metrics {{ color: #586069; font-size: 0.9em; }}
.elapsed {{ color: #586069; font-size: 0.85em; }}
.html-link {{ margin-left: auto; color: #0366d6; text-decoration: none; font-size: 0.9em; }}
.html-link:hover {{ text-decoration: underline; }}
.summary {{ font-size: 0.95em; color: #24292e; margin-bottom: 8px; }}
.checks {{ display: flex; flex-wrap: wrap; gap: 6px; }}
.check {{ font-size: 0.8em; padding: 2px 8px; border-radius: 4px; background: #f6f8fa; }}
.check.pass {{ color: #166534; }}
.check.fail {{ color: #991b1b; background: #fef2f2; }}
.check.info {{ color: #586069; }}
</style></head><body>
<div class="container">
  <h1>ION 扩展全量验收报告</h1>
  <div class="meta">{timestamp} | 模型 glm-5.2/zai | 5 并发 × 3 Wave DAG 调度</div>

  <div class="overview">
    <div class="stat-card"><div class="num">{total}</div><div class="label">总场景</div></div>
    <div class="stat-card stat-pass"><div class="num">{npass}</div><div class="label">通过</div></div>
    <div class="stat-card stat-fail"><div class="num">{nfail}</div><div class="label">失败</div></div>
    <div class="stat-card stat-error"><div class="num">{nerror}</div><div class="label">错误</div></div>
    <div class="stat-card stat-rate"><div class="num">{rate}%</div><div class="label">通过率</div></div>
  </div>

  <h2>模块通过率总览</h2>
  <table>
    <tr><th>模块</th><th>名称</th><th>通过</th><th>率</th></tr>
    {''.join(mod_rows)}
  </table>

  <h2>场景详情（{total} 个）</h2>
  {''.join(scenario_rows)}
</div>
</body></html>'''

    return html

def main():
    descs = load_scenario_descriptions()
    results = load_results()
    if not results:
        print("No results found in", REPORT_DIR)
        sys.exit(1)

    html = generate_html(results, descs)
    out_path = os.path.join(REPORT_DIR, "index.html")
    with open(out_path, 'w', encoding='utf-8') as f:
        f.write(html)

    npass = sum(1 for r in results if r["status"] == "PASS")
    nfail = sum(1 for r in results if r["status"] == "FAIL")
    print(f"Generated: {out_path}")
    print(f"  {npass} PASS / {nfail} FAIL / {len(results)} total ({npass*100//len(results)}%)")

if __name__ == "__main__":
    main()
