#!/usr/bin/env python3
"""validate_html.py — HTML 导出硬性校验（9 项指标）

用法：python3 scripts/validate_html.py <html_file> [--chrome <path>]
输出：JSON 格式的校验结果 + 退出码（0=全过，1=有失败）
"""
import base64
import json
import os
import re
import subprocess
import sys


def load_html(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def decode_session_data(html):
    """从 base64 script 解码 session data"""
    m = re.search(r'<script[^>]*>([A-Za-z0-9+/=]{100,})</script>', html)
    if not m:
        return None
    try:
        return json.loads(base64.b64decode(m.group(1)).decode("utf-8"))
    except Exception:
        return None


def render_dom(html_path, chrome_path):
    """用 chrome headless 渲染 HTML，返回 DOM 文本"""
    if not chrome_path or not os.path.exists(chrome_path):
        return ""
    try:
        result = subprocess.run(
            [chrome_path, "--headless", "--disable-gpu", "--no-sandbox",
             "--virtual-time-budget=8000", "--dump-dom", f"file://{os.path.abspath(html_path)}"],
            capture_output=True, text=True, timeout=30
        )
        return result.stdout
    except Exception:
        return ""


def validate(html_path, chrome_path=""):
    """执行 9 项硬性校验，返回 results dict"""
    results = {"checks": [], "passed": 0, "failed": 0}

    def check(metric_id, name, passed, detail=""):
        results["checks"].append({
            "id": metric_id, "name": name,
            "status": "PASS" if passed else "FAIL",
            "detail": detail
        })
        if passed:
            results["passed"] += 1
        else:
            results["failed"] += 1

    # M1: HTML 存在且 > 100KB
    exists = os.path.exists(html_path)
    size = os.path.getsize(html_path) if exists else 0
    check("M1", "HTML 文件存在且 > 100KB", exists and size > 102400,
          f"size={size} bytes" if exists else "文件不存在")

    if not exists:
        results["html_path"] = html_path
        return results

    html = load_html(html_path)
    data = decode_session_data(html)
    dom = render_dom(html_path, chrome_path)

    # M2: ION Version 含 git hash
    version = ""
    if data:
        version = data.get("header", {}).get("ionVersion", "")
    check("M2", "ION Version 含 git hash", "+" in version and len(version) > 10,
          f"version={version}")

    # M3: 对话流完整（user + assistant 消息）
    user_count = dom.count('class="user-message"')
    asst_count = dom.count('class="assistant-message"')
    check("M3", "对话流完整（user + assistant ≥ 1）",
          user_count >= 1 and asst_count >= 1,
          f"user-message={user_count}, assistant-message={asst_count}")

    # M4: 无渲染 bug
    bugs = 0
    for pattern in ["exit undefined", "exit_code=None", "exit_code=Some",
                    "(exit undefined)", "exit_code=unknown"]:
        bugs += dom.count(pattern)
    check("M4", "无渲染 bug（exit undefined / None / Some / unknown）",
          bugs == 0, f"bug count={bugs}")

    # M5: 无模板残留（只检查渲染后可见区域，排除 <script> 标签内的 JS 源码）
    import re as _re
    dom_no_script = _re.sub(r'<script[^>]*>[\s\S]*?</script>', '', dom)
    md_blocks = _re.findall(r'class="markdown-content"[^>]*>([\s\S]*?)</div>', dom_no_script)
    leak_in_md = sum(1 for block in md_blocks if "safeMarkedParse" in block or "${" in block)
    check("M5", "无模板字符串残留（渲染区 markdown-content 内）", leak_in_md == 0,
          f"leak in markdown={leak_in_md}, total md blocks={len(md_blocks)}")

    # M6: toolCall 可见（tool-execution 或 tool-header class）
    tool_exec = dom.count('class="tool-execution"')
    tool_header = dom.count('class="tool-header"')
    tool_total = tool_exec + tool_header
    check("M6", "toolCall 可见（tool-execution + tool-header）",
          tool_total >= 1, f"tool-execution={tool_exec}, tool-header={tool_header}")

    # M7: bash_result 格式正确（如果有 bash_result 的话）
    bash_result_count = dom.count("bash_result")
    if bash_result_count > 0:
        has_bid_attr = bool(re.search(r'bid="\w+"', dom))
        has_exit_attr = bool(re.search(r'exit="[\w]+"', dom))
        check("M7", "bash_result 格式正确（bid + exit 属性）",
              has_bid_attr and has_exit_attr,
              f"bash_result={bash_result_count}, bid_attr={has_bid_attr}, exit_attr={has_exit_attr}")
    else:
        check("M7", "bash_result 格式正确", True,
              "无 bash_result（该扩展不涉及 bash）")

    # M8: 截断正确（如果有截断的话）
    truncated_count = len(re.findall(r'truncated \d+ bytes', dom))
    if truncated_count > 0:
        # 检查截断后有尾部内容（不是只截头）
        has_tail = bool(re.search(r'truncated \d+ bytes\]\.\.\..{10,}', dom, re.DOTALL))
        check("M8", "截断正确（头尾保留）", has_tail,
              f"truncated={truncated_count}, has_tail={has_tail}")
    else:
        check("M8", "截断正确", True, "无截断（输出未超长）")

    # M9: 时间戳存在（cmd_run 路径可能全相同，这是已知限制）
    # 只要求时间戳字段存在且非空，不强制要求不同值
    ts_present = 0
    if data:
        for e in data.get("entries", []):
            ts = e.get("timestamp", "")
            if ts:
                ts_present += 1
    check("M9", "时间戳存在（entries 有 timestamp 字段）",
          ts_present >= 2,
          f"entries with timestamp={ts_present}")

    results["html_path"] = html_path
    results["html_size"] = size
    return results


def print_report(results):
    """打印人类可读报告"""
    print(f"\n{'='*60}")
    print(f"  HTML 校验报告: {results.get('html_path', '?')}")
    print(f"  文件大小: {results.get('html_size', 0)} bytes")
    print(f"  通过: {results['passed']} / 失败: {results['failed']}")
    print(f"{'='*60}\n")

    for c in results["checks"]:
        icon = "✅" if c["status"] == "PASS" else "❌"
        print(f"  {icon} {c['id']} {c['name']}")
        if c["detail"]:
            print(f"     {c['detail']}")
        print()

    if results["failed"] > 0:
        print(f"❌ 校验失败：{results['failed']} 项未通过")
    else:
        print(f"✅ 全部通过：{results['passed']} 项硬性指标")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("用法: python3 validate_html.py <html_file> [--chrome <path>]")
        sys.exit(1)

    html_file = sys.argv[1]
    chrome = ""
    if "--chrome" in sys.argv:
        idx = sys.argv.index("--chrome")
        chrome = sys.argv[idx + 1] if idx + 1 < len(sys.argv) else ""

    results = validate(html_file, chrome)
    print_report(results)

    # 输出 JSON 到 stderr（给脚本解析用）
    print(json.dumps(results, ensure_ascii=False), file=sys.stderr)

    sys.exit(1 if results["failed"] > 0 else 0)
