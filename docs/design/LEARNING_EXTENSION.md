# Learning Extension Design

## Overview

Orchestrates automatic skill distillation from completed sessions. Wraps the memory processing pipeline with secret redaction, fast filtering, and LLM-based skill extraction. Aligns with pi's `extensions/learning/index.ts`.

## Key Structs

- **`LearningExtension`**: Worker-level extension with optional `ApiRegistry` and `Model` for LLM calls
- **`LearningDecision`**: Decision result with flags for memory extraction and skill distillation, plus metrics

## Lifecycle Hooks

- **`on_session_shutdown`**: Triggered when session ends
  - Reads `session_id` from context (avoids race conditions)
  - Derives `project_name` from CWD basename
  - Fires async distillation via `tokio::spawn` (non-blocking)
  - Memory extraction handled separately by `MemoryExtension`

## Skill Distillation Flow

1. **Resolve session**: Search `~/.ion/agent/sessions/*/` for `<session_id>.jsonl` or `session.jsonl` with matching `id` header
2. **Extract messages**: Parse JSONL, handle both `type:"message"` and `customType:"message"` variants
3. **Filter**: Skip if `< 4 messages`, `< 300 chars`, or no write operations (write/edit/bash)
4. **Redact secrets**: Strip API keys/tokens before LLM via `secret_detector::redact_secrets`
5. **LLM extraction**: Send capped transcript (8000 chars) to LLM with system prompt for Markdown skill
6. **Validate**: Skip if LLM returns `NO_SKILL` or empty output
7. **Write skill**: Save to `~/.ion/agent/skills/distilled-<slug>.md` with provenance header

## Config Options

- **`MIN_CONTENT_LEN`**: 300 chars minimum for extraction
- **`MIN_MESSAGES`**: 4 messages minimum
- **`WORK_TOOLS`**: Tools indicating real work (write, edit, bash, grep, find, etc.)
- **`SKIP_WORDS`**: Greeting-only words to skip extraction (ok, thanks, done, etc.)

## Design Principles

- **Best-effort**: All errors logged, never block session exit
- **Idempotent**: Existing skills not overwritten (user edits preserved)
- **DRY**: `analyze_session()` shared between extension and distillation