# Secret Detector

## Overview
Two-layer secret detection and redaction system. Prevents sensitive credentials from leaking to LLM prompts or memory storage. Aligned with pi's `secret-detector.ts` implementation.

## Detected Patterns

### Known Format Patterns (13 types)
- **AWS_ACCESS_KEY**: `AKIA[0-9A-Z]{16}`
- **AWS_SECRET_KEY**: `aws_secret_access_key=` followed by 40-char value
- **ANTHROPIC_KEY**: `sk-ant-[A-Za-z0-9_-]{20,}`
- **OPENAI_KEY**: `sk-[A-Za-z0-9]{20,}` (excluding sk-ant-)
- **GITHUB_TOKEN**: `gh[posu]_[A-Za-z0-9]{36}`
- **GITLAB_TOKEN**: `glpat-[A-Za-z0-9_-]{20}`
- **PEM_KEY**: `-----BEGIN [TYPE] PRIVATE KEY-----`
- **JWT**: Three base64 segments with dots (`eyJ...`)
- **BEARER_TOKEN**: `Bearer [token]`
- **DB_CONNECTION**: `postgres://`, `mysql://`, `mongodb://`, `redis://`
- **GENERIC_SECRET**: `api_key=`, `secret=`, `token=`, `password=` with 16+ chars
- **SLACK_TOKEN**: `xox[baprs]-[A-Za-z0-9-]{10,}`
- **GOOGLE_KEY**: `AIza[0-9A-Za-z_-]{35}`

### High-Entropy Detection
Shannon entropy ≥4.5 bits/char for strings 24-500 chars. Filters out common paths, URLs, code keywords. Only matches base64/hex strings without spaces.

## Integration with LearningExtension
The detector runs before content is sent to external systems:
1. **LLM Prompts**: Scrub user input before sending to LLM
2. **Memory Storage**: Redact secrets before persisting conversations
3. **Log Output**: Prevent secrets from appearing in logs

Redacted format: `[REDACTED:label]` — preserves secret type information for downstream LLM understanding.

## Config Options
No runtime configuration. Detection rules are hardcoded for security and consistency:
- Entropy threshold: 4.5 bits/char
- Minimum length: 24 characters
- Maximum length: 500 characters

## API
- `detect_secrets(text: &str) -> Vec<DetectedSecret>`: Returns all detected secrets with type and redacted form
- `redact_secrets(text: &str) -> String`: Replaces secrets with redacted placeholders