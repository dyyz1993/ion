# ION offline export assets

These files are vendored into ION so `ion --export` remains a standalone,
offline feature and never reads another source checkout at runtime.

The initial HTML/CSS/JavaScript renderer snapshot was adapted from the
MIT-licensed pi coding agent export renderer:

- Copyright (c) 2025 Mario Zechner
- License: MIT
- Source: `packages/coding-agent/src/core/export-html`

ION owns subsequent integration changes, flow metadata, Timeline behavior,
entry coverage, folding, accessibility and security hardening in this copy.

The `vendor/` directory contains the offline browser builds used by that
renderer (`marked` and `highlight.js`), under their respective upstream
licenses.
