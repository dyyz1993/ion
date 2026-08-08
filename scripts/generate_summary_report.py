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
        # Multi-turn format: fp_id|status|pass|fail|elapsed|ext_id|name|turns
        # Old format: sid|status|pass|fail|elapsed|html
        sid = parts[0]
        status = parts[1]
        tpass = parts[2]
        tfail = parts[3]
        elapsed = parts[4]
        # Detect format: if parts[5] starts with "EXT-" it's multi-turn format
        if parts[5].startswith("EXT-"):
            ext_id = parts[5]
            html = f"{sid}_{ext_id}.html"
        else:
            html = os.path.basename(parts[5]) if parts[5] else ""
        results.append({
            "sid": sid,
            "status": status,
            "pass": int(tpass) if tpass.isdigit() else 0,
            "fail": int(tfail) if tfail.isdigit() else 0,
            "elapsed": elapsed,
            "html": html,
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
    """生成紧凑汇总 HTML - 表格布局 + hover tooltip + 模块过滤"""
    npass = sum(1 for r in results if r["status"] == "PASS")
    nfail = sum(1 for r in results if r["status"] == "FAIL")
    nerror = sum(1 for r in results if r["status"] == "ERROR")
    total = len(results)
    rate = npass * 100 // total if total else 0
    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M")

    # Group by module
    by_mod = defaultdict(list)
    for r in results:
        ext_num = r["sid"].split("-")[1] if "-" in r["sid"] else "?"
        by_mod[ext_num].append(r)

    ext_names = {"02":"global-memory","03":"dev_server","04":"file_snapshot","05":"lsp",
                 "06":"hooks","07":"goal_supervisor","08":"MonitorExt","09":"bash",
                 "10":"Memory v0.1","11":"rules-engine","12":"learning","13":"permission",
                 "14":"file-approval","15":"context-index","16":"SessionIndex","17":"ContextReclaimer",
                 "18":"file-time-guard","19":"PlanExt","20":"ToolLoopDetector",
                 "22":"auto-session-title","23":"WorkflowExt","24":"streaming"}

    # Build filter buttons
    filter_btns = '<button class="filter-btn active" onclick="filterExt(\'ALL\')">全部</button>'
    for ext in sorted(by_mod):
        name = ext_names.get(ext, ext)
        mp = sum(1 for r in by_mod[ext] if r["status"] == "PASS")
        mt = len(by_mod[ext])
        filter_btns += f'<button class="filter-btn" onclick="filterExt(\'EXT-{ext}\')">EXT-{ext} {name}</button>'

    # Build table rows
    rows_html = ""
    for r in results:
        sid = r["sid"]
        desc = descs.get(sid, {})
        report = load_report_details(sid)
        html_path = os.path.join(REPORT_DIR, r["html"]) if r["html"] else ""
        tool_stats = extract_tool_stats(html_path)
        summary = summarize_scenario(r, desc, report, tool_stats)
        # Clean summary for tooltip (remove HTML tags)
        import html as html_mod
        tooltip = html_mod.escape(summary.replace("<b>","").replace("</b>",""))[:200]

        ext_num = sid.split("-")[1] if "-" in sid else "?"
        ext_name = ext_names.get(ext_num, ext_num)
        status_icon = {"PASS": "✅", "FAIL": "❌", "ERROR": "⚠️"}.get(r["status"], "?")
        checks = report.get("checks", [])
        check_summary = " ".join(
            {"PASS":"🟢","FAIL":"🔴","INFO":"⚪"}.get(c.get("status",""),"⚪") + c["id"]
            for c in checks[:8]
        )

        # Tool summary for hover
        tool_str = ", ".join(f"{k}×{v}" for k,v in tool_stats.items() if not k.startswith("_"))

        rows_html += f'''<tr class="fp-row" data-ext="EXT-{ext_num}" title="{tooltip}">
          <td class="col-status">{status_icon}</td>
          <td class="col-fp">{sid}</td>
          <td class="col-ext">EXT-{ext_num}</td>
          <td class="col-name">{desc.get("name", "")}</td>
          <td class="col-metrics">{r["pass"]}/{r["pass"]+r["fail"]}</td>
          <td class="col-time">{r["elapsed"]}s</td>
          <td class="col-turns">{desc.get("name","") and tool_str or ""}</td>
          <td class="col-checks">{check_summary}</td>
          <td class="col-html">{f'<a href="{r["html"]}">📄</a>' if r["html"] else "-"}</td>
        </tr>'''

    # Module summary rows
    mod_rows = ""
    for ext in sorted(by_mod):
        mod_results = by_mod[ext]
        mp = sum(1 for r in mod_results if r["status"] == "PASS")
        mt = len(mod_results)
        name = ext_names.get(ext, ext)
        badge = "✅" if mp == mt else "⚠️"
        mod_rows += f'<span class="mod-chip" onclick="filterExt(\'EXT-{ext}\')">{badge} EXT-{ext} {name} {mp}/{mt}</span>'

    return f'''<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8">
<title>ION 验收报告 {timestamp}</title>
<style>
* {{ margin:0; padding:0; box-sizing:border-box; }}
body {{ font-family:-apple-system,"PingFang SC",monospace; background:#0d1117; color:#c9d1d9; font-size:13px; }}
.header {{ background:#161b22; padding:16px 24px; border-bottom:1px solid #30363d; position:sticky; top:0; z-index:100; }}
.header h1 {{ font-size:18px; color:#58a6ff; margin-bottom:4px; }}
.header .stats {{ display:flex; gap:20px; font-size:13px; }}
.header .stat {{ color:#8b949e; }}
.header .stat b {{ color:#c9d1d9; font-size:15px; }}
.header .stat.pass b {{ color:#3fb950; }}
.header .stat.fail b {{ color:#f85149; }}
.header .stat.rate b {{ color:#58a6ff; }}
.filters {{ padding:8px 24px; background:#161b22; border-bottom:1px solid #30363d; display:flex; gap:6px; flex-wrap:wrap; }}
.filter-btn {{ background:#21262d; color:#8b949e; border:1px solid #30363d; border-radius:6px; padding:3px 10px; font-size:11px; cursor:pointer; transition:all 0.15s; }}
.filter-btn:hover {{ background:#30363d; color:#c9d1d9; }}
.filter-btn.active {{ background:#1f6feb; color:white; border-color:#1f6feb; }}
.mod-chips {{ padding:8px 24px; display:flex; gap:6px; flex-wrap:wrap; }}
.mod-chip {{ background:#21262d; color:#8b949e; border:1px solid #30363d; border-radius:12px; padding:2px 10px; font-size:11px; cursor:pointer; }}
.mod-chip:hover {{ background:#30363d; }}
table {{ width:100%; border-collapse:collapse; }}
th {{ background:#161b22; color:#8b949e; font-weight:600; text-align:left; padding:6px 12px; font-size:11px; text-transform:uppercase; border-bottom:1px solid #30363d; position:sticky; top:90px; }}
td {{ padding:4px 12px; border-bottom:1px solid #21262d; font-size:12px; }}
tr:hover {{ background:#161b22; }}
.col-status {{ width:30px; text-align:center; }}
.col-fp {{ width:70px; color:#58a6ff; font-weight:600; }}
.col-ext {{ width:60px; color:#8b949e; }}
.col-name {{ min-width:120px; }}
.col-metrics {{ width:50px; text-align:center; color:#3fb950; }}
.col-time {{ width:50px; text-align:right; color:#8b949e; }}
.col-turns {{ max-width:150px; color:#8b949e; font-size:11px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
.col-checks {{ max-width:200px; font-size:10px; }}
.col-html {{ width:30px; text-align:center; }}
.col-html a {{ text-decoration:none; font-size:14px; }}
.fp-row {{ cursor:pointer; }}
.fp-row.hidden {{ display:none; }}
</style></head><body>
<div class="header">
  <h1>ION Extension Validation Report</h1>
  <div class="stats">
    <span class="stat">Total: <b>{total}</b></span>
    <span class="stat pass">PASS: <b>{npass}</b></span>
    <span class="stat fail">FAIL: <b>{nfail}</b></span>
    <span class="stat">ERROR: <b>{nerror}</b></span>
    <span class="stat rate">Rate: <b>{rate}%</b></span>
    <span class="stat" style="margin-left:auto">{timestamp}</span>
  </div>
</div>
<div class="filters">{filter_btns}</div>
<div class="mod-chips">{mod_rows}</div>
<table>
  <thead><tr>
    <th></th><th>FP ID</th><th>Module</th><th>Feature</th><th>Checks</th><th>Time</th><th>Tools</th><th>Details</th><th>HTML</th>
  </tr></thead>
  <tbody>{rows_html}</tbody>
</table>
<script>
function filterExt(ext) {{
  document.querySelectorAll('.filter-btn').forEach(b => b.classList.remove('active'));
  event.target.classList.add('active');
  document.querySelectorAll('.fp-row').forEach(row => {{
    if (ext === 'ALL' || row.dataset.ext === ext) {{
      row.classList.remove('hidden');
    }} else {{
      row.classList.add('hidden');
    }}
  }});
}}
</script>
</body></html>'''

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
