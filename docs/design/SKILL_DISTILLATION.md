# Skill Distillation System

## Overview
Phase 3 background process that extracts reusable skills from completed coding sessions with write operations. Runs non-blocking via tokio::spawn when a session ends. Best-effort architecture — errors logged only, never propagated to avoid blocking session shutdown.

## 7-Step Distillation Flow
1. **Resolve session file** — Search ~/.ion/agent/sessions for session_id.jsonl (forked) or session.jsonl (main) by matching file name or header `id` field
2. **Parse messages** — Extract User/Assistant/Tool content from JSONL, accepting both cmd_run and worker message formats
3. **Filter with analyze_session** — Skip if <4 messages, <300 chars, or no write operations (shared with learning_extension)
4. **Secret redaction** — Pass messages through secret_detector::redact_secrets before LLM
5. **LLM extraction** — Prompt to distill a reusable Markdown skill (30-80 lines); returns `NO_SKILL` if session is trivial
6. **Slug generation** — Extract H1 title, convert to filesystem-safe slug via slugify()
7. **Write skill file** — Save to ~/.ion/agent/skills/distilled-{slug}.md

## Key Functions
- `run_skill_distillation()` — Entry point, async, returns Ok(Some(path)) or Ok(None) on skip
- `analyze_session()` — Pure function deciding if session worth distilling (message count, content length, write ops presence)
- `extract_block_text()` — Extracts readable text from content blocks (Text, ToolCall, ToolResult), skips Thinking blocks
- `extract_h1_title()` — Parses first "# Title" line for slug generation
- `slugify()` — Converts title to kebab-case filename (lowercase, collapse dashes, trim)
- `resolve_session_file()` — Finds session JSONL by id via dual-strategy search

## Provenance Header Format
```html
<!--
  Distilled by ION Learning Extension
  Session: {session_id}
  Project: {project_name}
  Generated: unix:{timestamp}
  Edit this file freely — it will NOT be overwritten.
-->
```

## Idempotent Mechanism
Before writing, checks if `distilled-{slug}.md` already exists. If present, skips entirely to preserve user manual edits. Skills are write-once.