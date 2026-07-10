# ION TUI Design System

A design system reconstruction of **ION TUI** — the terminal-style dashboard for ION, an AI Agent orchestration platform written in Rust. The system is purpose-built for high-density, keyboard-driven developer tooling where every pixel earns its place and every glyph reads like shell output.

## Source

- **Origin:** An existing `.design` project (ION TUI 仪表板 dashboard), extracted from its `colors_and_type.css` token spec.
- **Theme:** Dark-only — no light counterpart exists or is planned.
- **Typography:** Monospace-only (JetBrains Mono primary); no sans-serif anywhere, including UI chrome.

## What this design system covers

- **Foundations** — Deep navy background scale (6 stops), cyan brand accent `#00d4aa`, monospace 7-step type scale (11–20px), sharp-corner geometry (`border-radius: 0` default), 1px border-driven structure.
- **Components** — 6 TUI components: status-badge, panel, log-stream, worker-card, key-hint, input-prompt.
- **Dashboard kit** — Terminal dashboard layout primitives (240px sidebar, 300px right panel, 40px header, 28px status bar).

---

## CONTENT FUNDAMENTALS

### Voice & tone

Chinese-first, technical, clipped. UI copy reads like terminal output, not marketing prose — short noun phrases, no softening particles, no exclamation marks. Status words stay uppercase English (`BUSY` / `IDLE` / `ERROR` / `STREAMING`) even when embedded in Chinese sentences, because they map 1:1 to machine states and grep cleanly in logs. Worker identifiers, tool names, and file paths are always verbatim English/ASCII. Chinese appears only in human-facing descriptions and prompts. There is no emoji decoration — the only emoji permitted are the four status dots (🟢 ⚪ 🔴 🟡), which function as semantic glyphs, not ornamentation.

### Concrete copy examples

- **Worker status line:** `Worker-01: BUSY — 执行 prompt '分析代码结构'`
- **Error event:** `edit('src/config.rs') timeout`
- **Log tail:** `[12:04:33] spawn_worker(dev) → ok (pid 8821)`
- **Input placeholder:** `向 ION 提问...`
- **Key hint:** `[Esc] 退出  [Enter] 发送  [Space] 暂停`

### When generating copy

- Keep column labels ≤5 characters in tight data grids (e.g. `PID`, `状态`, `耗时`).
- Encode status with Unicode dots + color, never with colored pill backgrounds — pills break the TUI contract.
- Timestamps always `HH:MM:SS` 24-hour, zero-padded; durations as `12s` / `3m04s`.
- Technical terms stay English even in Chinese sentences: Worker, prompt, agent, tool, spawn, channel — translating them dilutes precision.

---

## VISUAL FOUNDATIONS

### Color

The brand accent is `#00d4aa` — a cyan-teal that reads as the "active agent green-cyan" of a terminal cursor. It is the only saturated color used for structure: active panel borders, focus rings, the blinking `█` cursor, streaming text indicators. A dimmed variant `#00d4aa33` (33% alpha) backs the glow shadow and hover washes; a bright variant `#00ffcc` is reserved exclusively for streaming cursors and active-stream highlights. The accent is never used as a fill for large surfaces — it is a signal, not a background.

The background is a six-stop deep navy scale, ascending in elevation: `#0a0e17` (deepest, app root) → `#0d1117` (primary canvas) → `#161b22` (secondary surface) → `#1c2333` (elevated) → `#21283b` (surface) → `#262f45` (hover). The jumps are small and perceptually uneven on purpose — elevation is felt more than seen, mimicking the subtle layering of stacked terminal panes. Text on the darkest stop is `#e6edf3` (primary), dropping to `#8b949e` (secondary) and `#484f58` (muted, used only for disabled/micro labels).

The semantic quartet is calibrated for dark-ground legibility: warning `#f0883e`, error `#f85149`, success `#3fb950`, info `#58a6ff`. These are GitHub-dark-derived values chosen because they hold contrast above `#0d1117` without screaming. A secondary palette — cyan `#79c0ff`, orange `#d29922`, purple `#bc8cff` — exists solely for project color-coding (Worker tags, branch labels); it is never used for status, which is the semantic quartet's exclusive job.

**Vibe:** developer cave at 2am — dark, focused, a single cyan glow where work is happening.

### Typography

**JetBrains Mono** is the primary and only typeface, loaded weights 400 / 500 / 600 / 700. There is no sans-serif escape hatch — labels, headings, body, micro-copy, and UI chrome are all monospace. The fallback stack is `'Fira Code', 'SF Mono', 'Cascadia Code', 'Consolas', 'Monaco', monospace`, ordered by glyph coverage and ligature support; `Cascadia Code` and `Consolas` cover Windows where JetBrains Mono may not be installed.

The scale runs seven steps from `11px` caption to `20px` display, anchored at `13px` body — deliberately small to maximize information density. Display `20px` and H1 `18px` carry weight 700 with `-0.01em` tracking for tightness; H2 `16px` is weight 700 with default tracking; H3 `14px` drops to weight 600. Body `13px` and mono `12px` sit at weight 400. Caption `11px` is weight 400 in secondary text color. A micro variant at `11px` applies `uppercase` + `0.05em` tracking for section eyebrows.

Line-height is bifurcated: `1.25` (tight) for data rows, headings, and anything that must stack densely; `1.5` (normal) for prose blocks and descriptions; `1.65` (relaxed) is defined but rarely used.

### Spacing

A 4px base unit (`--space-1`) governs everything, escalating through `--space-2` (8px), `--space-3` (12px), `--space-4` (16px), `--space-5` (20px), `--space-6` (24px), `--space-8` (32px). The rhythm is intentionally tight — panels stack with 1px dividers (`#21262d`), not with gaps. The default panel padding is `--space-3` (12px). Header height is locked at `40px`, status bar at `28px`, sidebar at `240px`, right panel at `300px`.

### Radius

Three tokens exist — `--radius-sm: 2px`, `--radius-md: 4px`, `--radius-lg: 6px` — but the operational default on every TUI surface is **0px**. Panels, cards, badges, inputs, and buttons are square-cornered. This is the defining TUI rule: sharp corners signal terminal heritage. The radius tokens survive only for rare soft exceptions such as focus rings; treat any non-zero radius as a documented deviation.

### Shadow / Elevation

Three shadow tokens, used sparingly:

1. **shadow-panel:** `0 0 0 1px var(--color-border)` — not a drop shadow but a 1px ring that defines panel edges. This is the default depth mechanism; static surfaces never use blurred shadows.
2. **shadow-glow:** `0 0 12px var(--color-accent-dim)` — the only blurred shadow, applied exclusively to active/focused elements to signal "work is happening here."
3. **shadow-dropdown:** `0 4px 12px rgba(0, 0, 0, 0.4)` — reserved for floating layers (dropdowns, popovers) that lift above the canvas.

The philosophy: borders carry depth at rest, glow carries attention in motion. Drop shadows on static panels are forbidden.

### Borders, Backgrounds

Default border is `1px solid #30363d` on all structural edges. Active panels swap to `#00d4aa` (accent border) to indicate focus. Stacked sections inside a panel divide with `1px solid #21262d`. Backgrounds are flat solid colors from the navy scale — no gradients, no noise textures, no glassmorphism. Hover states step exactly one stop up the elevation scale (`#21283b` → `#262f45`).

---

## Component Patterns

| Component | File | Key Insight |
|---|---|---|
| Status Badge | `components/status-badge.json` | Status is a Unicode dot + uppercase label, never a pill background — preserves the TUI text-only aesthetic. |
| TUI Panel | `components/panel.json` | Square corners + 1px `#30363d` border define the panel; active state swaps border to `#00d4aa`, no shadow added. |
| Log Stream | `components/log-stream.json` | Monospace `12px` lines at `1.25` line-height, ANSI-color-aware — reads like `tail -f`, not a feed UI. |
| Worker Card | `components/worker-card.json` | High-density card: PID, status dot, elapsed time, current tool — all on one row at `13px` body. |
| Key Hint | `components/key-hint.json` | `[Esc]` / `[Enter]` rendered as bracketed glyphs in muted text — the keyboard-driven ethos made literal. |
| Input Prompt | `components/input-prompt.json` | No send button — a blinking `█` cursor (`#00ffcc`) is the only focus indicator; submit is `[Enter]`. |

---

## Index

- `README.md` — this file (brand narrative and foundations reference)
- `colors_and_type.css` — runtime CSS variables (color, type, spacing, radius, shadow, layout)
- `css.json` — structured token data for programmatic consumption
- `components/index.json` — component registry (6 slugs)
- `components/{slug}.json` — per-component intent and variant contracts
- `components.css` — aggregated component CSS
- `preview/component-{slug}.html` — standalone HTML preview cards
- `SKILL.md` — AI-agent skill manifest and quick map

---

## Caveats / known substitutions

1. **Dark-only theme.** No light-mode tokens exist. Deriving a light counterpart would require inverting the entire six-stop navy scale and re-calibrating the cyan accent for light-ground contrast — noted as a future refinement, not a gap.
2. **Monospace constraint.** Every glyph is JetBrains Mono. If a design genuinely requires sans-serif (e.g., a marketing surface embedded in the dashboard), it breaks the TUI contract — document the exception explicitly and isolate it.
3. **Unicode emoji status dots.** Rendering of 🟢 ⚪ 🔴 🟡 varies by OS and font. JetBrains Mono covers most; on minimal terminals without emoji glyphs, fall back to text labels (`BUSY` / `IDLE` / `ERROR` / `WAIT`) — the semantic quartet is always paired with a text label anyway.
4. **Shadow tokens use `var()` references internally.** `--shadow-panel` and `--shadow-glow` reference `var(--color-border)` and `var(--color-accent-dim)` respectively; css-to-json parsers cannot resolve these to numeric values. They are documented as semantic shadows, not numeric elevation stops.
5. **Sidebar width divergence.** The source spec stated 220px, but both dashboard pages actually render at 240px. The library tokenizes `240px` as `--sidebar-width` (the dominant observed value) and notes the discrepancy here.
