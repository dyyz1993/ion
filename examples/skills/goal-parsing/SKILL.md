---
name: goal-parsing
description: Decompose a text objective into a structured goal plan with execution steps, verification checks, and acceptance criteria
context: inject
---

# Goal Parsing Skill

You are a goal planner. When given a coding objective in natural language, decompose it into a structured plan.

## Your Task

Given an objective, produce a JSON object with:

### execution_steps
An ordered array of steps the agent should follow:
```json
[
  {"id": "step_1", "description": "create the data structures", "deliverable": "struct definitions in src/lib.rs"},
  {"id": "step_2", "description": "implement core logic", "deliverable": "pub fn implementations"},
  {"id": "step_3", "description": "add unit tests", "deliverable": "#[cfg(test)] mod tests with 3+ test cases"},
  {"id": "step_4", "description": "verify compilation", "deliverable": "cargo build succeeds with 0 warnings"}
]
```

Rules:
- 3-7 steps is ideal
- Each step should be concrete and actionable
- Steps should build on each other (data structures → logic → tests → verification)
- The last step should always be verification (build/test)

### checks
Verification checks (shell commands, exit 0 = pass):
```json
[
  {"name": "cargo_build", "check_type": "ci", "rationale": "must compile", "command": "cargo build --lib 2>&1 | tail -1", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true},
  {"name": "cargo_test", "check_type": "ci", "rationale": "tests pass", "command": "cargo test --lib 2>&1 | tail -1", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true},
  {"name": "has_main_function", "check_type": "contingency", "rationale": "required by objective", "command": "grep -q 'pub fn' src/lib.rs", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true}
]
```

Rules:
- Always include cargo_build + cargo_test as CI checks
- Add objective-specific checks (grep for required functions/symbols)
- Commands must be shell-safe and exit 0 on success

### acceptance_criteria
Human-readable "done" conditions:
```json
[
  "The game compiles and runs with cargo run",
  "Win detection works for rows, columns, and diagonals",
  "At least 3 unit tests exist and pass"
]
```

## Output Format

Output ONLY the JSON object, no markdown fences, no explanation:

```json
{
  "execution_steps": [...],
  "checks": [...],
  "acceptance_criteria": [...]
}
```

## Examples

Objective: "implement a stack data structure with push, pop, and peek"

```json
{
  "execution_steps": [
    {"id": "step_1", "description": "define Stack struct with Vec storage", "deliverable": "pub struct Stack<T> in src/lib.rs"},
    {"id": "step_2", "description": "implement push, pop, peek methods", "deliverable": "impl<T> Stack<T> with 3 pub fn"},
    {"id": "step_3", "description": "add unit tests for all methods", "deliverable": "#[test] functions covering push/pop/peek + edge cases"},
    {"id": "step_4", "description": "verify compilation and tests", "deliverable": "cargo build + cargo test pass"}
  ],
  "checks": [
    {"name": "cargo_build", "check_type": "ci", "rationale": "must compile", "command": "cargo build --lib 2>&1 | tail -1", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true},
    {"name": "cargo_test", "check_type": "ci", "rationale": "tests pass", "command": "cargo test --lib 2>&1 | tail -1", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true},
    {"name": "has_push", "check_type": "contingency", "rationale": "push method required", "command": "grep -q 'pub fn push' src/lib.rs", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true},
    {"name": "has_pop", "check_type": "contingency", "rationale": "pop method required", "command": "grep -q 'pub fn pop' src/lib.rs", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true},
    {"name": "has_peek", "check_type": "contingency", "rationale": "peek method required", "command": "grep -q 'pub fn peek' src/lib.rs", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true}
  ],
  "acceptance_criteria": [
    "Stack supports push, pop, and peek operations",
    "All methods handle empty-stack edge cases",
    "At least 3 unit tests pass"
  ]
}
```
