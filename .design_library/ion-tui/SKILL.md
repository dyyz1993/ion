---
name: ion-tui-design
description: Use this skill to generate well-branded terminal/TUI interfaces for ION. Contains colors, type, fonts, assets, and UI kit for prototyping developer-dashboard UIs with a terminal aesthetic.
user-invocable: true
---
# ION TUI Design Skill

Read the `README.md` file within this skill, and explore the other available files.

If creating visual artifacts, copy assets out and create static HTML files. If working on production code, read the rules here to become an expert in designing with this brand.

## Quick map

- `README.md` — brand context, content fundamentals, visual foundations (read first)
- `colors_and_type.css` — drop-in CSS variables for colors, type, radius, shadow, spacing (dark-only theme)
- `css.json` — structured token understanding source
- `components/index.json` — component index + cross-component patterns
- `components.css` — aggregated component CSS with DOM anatomy comments
- `preview/` — 6 HTML cards illustrating each TUI component
- `library-consumption.json` — recommended downstream read order

## Essentials at a glance

- Brand accent is cyan `#00d4aa` (`--color-accent`), brightening to `#00ffcc` on hover; it is the sole saturated hero color across an otherwise deep-navy `#0d1117` field.
- Corners stay sharp on TUI surfaces — radius is effectively 0px by default; `--radius-sm` (2px) / `--radius-md` (4px) / `--radius-lg` (6px) exist only for rare pill/dropdown edges.
- Spacing scale is compact and 4px-rooted: `--space-1` 0.25rem → `--space-8` 2rem, with `--panel-padding` fixed at `--space-3` (0.75rem) for high information density.
- Typography is monospace-only — `--font-mono` resolves to `'JetBrains Mono', 'Fira Code', 'SF Mono', monospace`; body sits at `--text-base` 0.8125rem (13px) / weight 400 / line-height 1.5.
- Voice is Chinese-first, technical, and terse; status is expressed through colored Unicode dots rather than words, and no emoji appears in UI copy.
- Shadows are border-defined, not ambient: `--shadow-panel` is `0 0 0 1px var(--color-border)`; the only glow is `--shadow-glow` (`0 0 12px` accent-dim), reserved for active focus.
- The signature ION quirk is keyboard-driven focus — a blinking block cursor `█` marks the active input, and `--color-border-accent` outlines the focused surface.

## Components

| Slug | Name | Key Insight |
|------|------|-------------|
| status-badge | Status Badge | Colored Unicode dot (`●`) + uppercase micro label, sharp-cornered, no fill — pure text indicator in JetBrains Mono. |
| panel | TUI Panel | 1px `--color-border` ring defines the edge (no drop shadow); header bar is `--color-bg-elevated` with monospace title at `--text-sm`. |
| log-stream | Log Stream | Monospace stream at `--text-sm` (12px) / line-height 1.5, left-gutter ANSI-style color coding, no wrapping for terminal fidelity. |
| worker-card | Worker Card | Dense grid cell with sharp corners; state shown via status-badge dot, accent border only when active (`--shadow-glow`). |
| key-hint | Key Hint | Inline `<kbd>`-style chip, 1px border, `--text-xs` 11px uppercase, JetBrains Mono — reads as terminal notation. |
| input-prompt | Input Prompt | Single-line monospace field with blinking `█` cursor indicator, `--shadow-glow` focus ring, sharp corners, `--color-bg-deep` fill. |
