#!/usr/bin/env python3
"""Build the GitHub Pages facade site from skills/ into a target directory.

Usage: python3 scripts/build_pages.py <output-dir>

Generates:
  index.html                 — hero + install + scenarios + skill cards
  skills/<name>/index.html   — rendered SKILL.md body
  skills/<name>/SKILL.md     — raw copy

Markdown rendering uses the `markdown` package when available; without it the
body degrades to an escaped <pre> block (build still succeeds).
"""
import html
import re
import shutil
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SKILLS_DIR = REPO_ROOT / "skills"
REPO_URL = "https://github.com/dyyz1993/ion"
INSTALL_CMD = "npx skills add dyyz1993/ion"

SCENARIOS = [
    ("A", "Quick Execution", 'ion "summarize this repo"',
     "Direct spawn, no host. One agent turn, run-to-completion, exit."),
    ("B", "Quick Orchestration", 'ion --host --agent coordinator "ship this feature"',
     "Temporary host + event pump to stdout. Coordinator spawns sub-agents; "
     "recursive-idle auto exit."),
    ("C", "Persistent Service", "ion serve",
     "Unix socket host for external UIs. ion rpc / ion subscribe, multi-client "
     "sync, dynamic approvals."),
]


def parse_frontmatter(text: str):
    m = re.match(r"^---\r?\n(.*?)\r?\n---\r?\n?", text, re.S)
    meta: dict[str, str] = {}
    if not m:
        return meta, text
    for line in m.group(1).splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            meta[key.strip()] = value.strip().strip("\"'")
    return meta, text[m.end():]


def render_markdown(md_text: str) -> str:
    try:
        import markdown
        return markdown.markdown(md_text, extensions=["fenced_code", "tables"])
    except ImportError:
        return f'<pre class="md-fallback">{html.escape(md_text)}</pre>'


CSS = """
:root{--bg:#0d1117;--panel:#161b22;--border:#30363d;--fg:#e6edf3;--muted:#8b949e;
--accent:#58a6ff;--green:#7ee787;--purple:#bc8cff;--code:#0a0d12}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--fg);font:16px/1.65 -apple-system,BlinkMacSystemFont,
"Segoe UI",Helvetica,Arial,sans-serif;padding:0 24px}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.wrap{max-width:960px;margin:0 auto;padding:48px 0 64px}
.badge{display:inline-block;border:1px solid var(--border);border-radius:999px;
padding:2px 12px;font-size:13px;color:var(--muted);margin-right:8px}
h1{font-size:34px;margin:16px 0 8px}
h1 .ion{color:var(--green);font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
.tagline{color:var(--muted);font-size:18px;margin-bottom:24px}
h2{font-size:22px;margin:40px 0 16px;padding-bottom:8px;border-bottom:1px solid var(--border)}
code{background:var(--code);border:1px solid var(--border);border-radius:6px;
padding:1px 6px;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}
pre{background:var(--code);border:1px solid var(--border);border-radius:8px;
padding:14px 16px;overflow-x:auto;margin:12px 0}
pre code{border:none;padding:0;background:none}
.install{background:var(--panel);border:1px solid var(--border);border-radius:10px;
padding:18px 20px;margin:20px 0}
.install code{font-size:15px;color:var(--green)}
.install .alt{color:var(--muted);font-size:14px;margin-top:8px}
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));gap:16px}
.card{background:var(--panel);border:1px solid var(--border);border-radius:10px;
padding:18px 20px;display:flex;flex-direction:column;gap:10px}
.card h3{font-size:17px;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--purple)}
.card p{color:var(--muted);font-size:14px;flex:1}
.card .meta{font-size:13px}
.scenario{background:var(--panel);border:1px solid var(--border);border-radius:10px;
padding:16px 20px;margin-bottom:12px}
.scenario .tag{display:inline-block;width:26px;height:26px;border-radius:6px;
background:var(--code);border:1px solid var(--border);color:var(--green);
text-align:center;line-height:26px;font-weight:700;margin-right:10px}
.scenario .desc{color:var(--muted);font-size:14px;margin-top:6px}
footer{margin-top:56px;padding-top:20px;border-top:1px solid var(--border);
color:var(--muted);font-size:14px}
article h1{font-size:26px}article h2{font-size:20px}article h3{font-size:17px}
article ul,article ol{margin:10px 0 10px 24px}article li{margin:4px 0}
article table{border-collapse:collapse;margin:12px 0}
article th,article td{border:1px solid var(--border);padding:6px 12px;font-size:14px}
article blockquote{border-left:3px solid var(--accent);padding-left:14px;
color:var(--muted);margin:12px 0}
.md-fallback{white-space:pre-wrap;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px}
.back{font-size:14px}
"""


def page(title: str, body: str, back: str = "") -> str:
    return (
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n"
        f"<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n"
        f"<title>{html.escape(title)}</title>\n<style>{CSS}</style>\n</head>\n<body>\n"
        f"<div class=\"wrap\">{back}{body}<footer>"
        f"ION · MIT License · <a href=\"{REPO_URL}\">github.com/dyyz1993/ion</a>"
        f"</footer></div>\n</body>\n</html>\n"
    )


def skill_card(name: str, desc: str) -> str:
    return (
        f"<div class=\"card\"><h3>{html.escape(name)}</h3>"
        f"<p>{html.escape(desc)}</p><div class=\"meta\">"
        f"<a href=\"skills/{html.escape(name)}/\">view</a> · "
        f"<a href=\"skills/{html.escape(name)}/SKILL.md\">raw</a><br>"
        f"<code>npx skills add dyyz1993/ion --skill {html.escape(name)}</code>"
        f"</div></div>"
    )


def build(out_dir: Path) -> None:
    skills = []
    for skill_md in sorted(SKILLS_DIR.glob("*/SKILL.md")):
        meta, body = parse_frontmatter(skill_md.read_text(encoding="utf-8"))
        name = meta.get("name") or skill_md.parent.name
        desc = meta.get("description", "")
        skills.append((name, desc, skill_md, body))

    out_dir.mkdir(parents=True, exist_ok=True)

    cards = "\n".join(skill_card(n, d) for n, d, _, _ in skills)
    scenarios = "".join(
        f"<div class=\"scenario\"><span class=\"tag\">{tag}</span><strong>"
        f"{html.escape(title)}</strong><br><code>{html.escape(cmd)}</code>"
        f"<div class=\"desc\">{html.escape(desc)}</div></div>"
        for tag, title, cmd, desc in SCENARIOS
    )
    intro = (
        f"<span class=\"badge\">Rust</span><span class=\"badge\">Agent Skills</span>"
        f"<span class=\"badge\">MIT</span>"
        f"<h1><span class=\"ion\">ion</span> — AI Agent Orchestration Platform</h1>"
        f"<p class=\"tagline\">Self-evolving Rust implementation aligned with pi. "
        f"Multi-agent orchestration, WASM extensions, MCP, session-tree branching.</p>"
        f"<div class=\"install\"><strong>Install any skill into your agent</strong><br>"
        f"<code>{INSTALL_CMD}</code>"
        f"<div class=\"alt\">Also: <code>gh skill install dyyz1993/ion</code> · "
        f"ION native: <code>~/.ion/agent/skills/</code> or <code>&lt;project&gt;/.ion/skills/</code></div></div>"
        f"<h2>Skills ({len(skills)})</h2><div class=\"cards\">{cards}</div>"
        f"<h2>Three usage scenarios</h2>{scenarios}"
    )
    (out_dir / "index.html").write_text(page("ION — Agent Skills", intro), encoding="utf-8")

    for name, desc, skill_md, body in skills:
        skill_dir = out_dir / "skills" / name
        skill_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(skill_md, skill_dir / "SKILL.md")
        back = f"<p class=\"back\"><a href=\"../../\">← all skills</a></p>"
        article = (
            f"<span class=\"badge\">skill</span>"
            f"<h1>{html.escape(name)}</h1>"
            f"<p class=\"tagline\">{html.escape(desc)}</p>"
            f"<div class=\"install\"><code>npx skills add dyyz1993/ion --skill {html.escape(name)}</code></div>"
            f"<article>{render_markdown(body)}</article>"
        )
        (skill_dir / "index.html").write_text(page(f"{name} — ION skill", article, back), encoding="utf-8")

    print(f"built {len(skills)} skill page(s) into {out_dir}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: build_pages.py <output-dir>")
    build(Path(sys.argv[1]))
