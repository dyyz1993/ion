---
name: code-review
description: Perform a thorough code review. Use this skill when asked to review code, audit changes, check PRs, or validate implementation quality. Covers correctness, security, performance, maintainability, and tests.
---

# Code Review Skill

This skill gives any agent the ability to perform **systematic code reviews**. Loaded when the user (or another agent) asks for code review, PR audit, or quality validation.

## When to Use

Load this skill when:
- User says "review this code" / "audit these changes" / "check the PR"
- Coordinator delegates review task to you
- A merge request / PR needs validation before publish
- Post-implementation verification (after developer finishes a task)
- Periodic quality audit (e.g., weekly review of recent commits)

## ⚠️ Critical Rules

1. **First action: identify what changed** (`git diff`, `git log`, file list)
2. **Review per checklist** (below) — do not skip categories
3. **Be specific**: cite file:line for each issue, never vague claims
4. **Distinguish severity** (BLOCKER / MAJOR / MINOR / NIT)
5. **Suggest fixes** for BLOCKER/MAJOR; comment only for MINOR/NIT
6. **Do not edit code yourself** unless explicitly asked — review outputs a report

---

## Workflow (6 steps)

### Step 1: Identify the scope of changes

```bash
# For PR review
git diff origin/main...HEAD --stat
git log origin/main..HEAD --oneline

# For uncommitted changes
git status --short
git diff --stat

# For a specific commit range
git diff <sha1>..<sha2> --stat
```

Record:
- Files changed (count, categories: src/test/docs/config)
- Lines added/removed
- Commit messages (look for "WIP", "TODO", "fix typo" = potential issues)

### Step 2: Categorize files by review depth

| Category | Depth | Why |
|----------|-------|-----|
| Core logic (src/, lib/) | Full review | Bugs hurt users |
| Tests (tests/, *_test.rs) | Sample review | Verify coverage of new code |
| Docs (README, *.md) | Skim | Check accuracy |
| Config (Cargo.toml, .json) | Check deps | New deps = supply chain risk |
| Generated (*.lock, target/) | Skip | Not human-readable |

### Step 3: Review per checklist (do not skip categories)

#### ✅ Correctness (BLOCKER-level)

- [ ] Logic matches intent (read commit message + code together)
- [ ] Edge cases handled (empty input, null, max value, concurrency)
- [ ] Off-by-one / boundary errors
- [ ] Race conditions (shared state, async ordering)
- [ ] Resource leaks (file handles, connections, locks not released)
- [ ] Error propagation (Result/Option properly chained, not swallowed)

#### ✅ Security (BLOCKER-level)

- [ ] Input validation (paths, sizes, types — no trust on user input)
- [ ] SQL injection (parameterized queries, no string concat)
- [ ] Command injection (no shell=True with user data; use safe APIs)
- [ ] Path traversal (canonicalize, reject `..`)
- [ ] Secret handling (no hardcoded keys, no logging of secrets)
- [ ] Authentication/authorization checks present where needed
- [ ] Dependencies vetted (no unknown/unmaintained crates)

#### ✅ Performance (MAJOR-level)

- [ ] Algorithm complexity reasonable (no O(n²) on hot path)
- [ ] No unnecessary allocations (Vec in loop, clone of large structs)
- [ ] I/O batched (no DB query in loop)
- [ ] Caching where appropriate
- [ ] Async not blocking (no `std::sync::Mutex` in async context)

#### ✅ Maintainability (MINOR-level)

- [ ] Naming clear (no `tmp1`, `data2`, single-letter vars in long scope)
- [ ] Functions focused (one responsibility, < 50 lines ideally)
- [ ] Comments explain WHY, not WHAT (WHAT is the code's job)
- [ ] No magic numbers (extract constants)
- [ ] Dead code removed (no `#[allow(dead_code)]` masking real issues)
- [ ] Module structure logical (related code together)

#### ✅ Tests (MAJOR-level)

- [ ] New code has tests
- [ ] Tests cover happy path AND edge cases
- [ ] Tests are deterministic (no flaky time/random/order dependencies)
- [ ] Test names describe the scenario (`test_x_when_y_returns_z`)
- [ ] No tests commented out / `.ignore` without explanation

#### ✅ Style (NIT-level)

- [ ] Consistent with surrounding code (indent, naming, idioms)
- [ ] Lints pass (`cargo clippy`, `rustfmt --check`)
- [ ] No trailing whitespace / unnecessary blank lines
- [ ] Import order follows project convention

### Step 4: Classify each finding

| Severity | Meaning | Action |
|----------|---------|--------|
| **BLOCKER** | Must fix before merge (security bug, crash, data loss) | "Request changes" |
| **MAJOR** | Should fix before merge (perf, missing tests, fragile logic) | "Request changes" |
| **MINOR** | Nice to fix (naming, comment, refactor) | "Approve with comments" |
| **NIT** | Style preference (whitespace, import order) | "Approve" |

### Step 5: Write the review report

Use this exact template:

```markdown
# Code Review: <PR/commit/branch description>

**Verdict**: APPROVE | REQUEST_CHANGES | NEEDS_DISCUSSION
**Files reviewed**: <count>
**Findings**: <blocker_count> blocker, <major_count> major, <minor_count> minor, <nit_count> nit

## Summary

<2-3 sentences: what was changed, overall quality, recommendation>

## BLOCKER findings

### B1: <short title>
- **Location**: `src/foo.rs:42`
- **Issue**: <what's wrong>
- **Suggested fix**:
```rust
// before
<bad code>

// after
<fixed code>
```
- **Why**: <why this is a blocker>

## MAJOR findings
(same format as BLOCKER)

## MINOR findings
(brief one-liners)

## NITs
(inline `file:line: comment`)

## Positive notes

<what was done well — be genuine, not flattering>

## Test coverage assessment

- New code paths: <list>
- Covered: ✅ / ❌
- Missing scenarios: <list>
```

### Step 6: Deliver verdict and next steps

End with clear action items:

```
APPROVE → "LGTM, merging is safe. Optional: address MINOR/NIT in follow-up."
REQUEST_CHANGES → "Please address BLOCKER (B1, B2) and MAJOR (M1) before re-review."
NEEDS_DISCUSSION → "Need clarification on <X> before I can judge correctness."
```

---

## Common bug patterns (checklist shortcuts)

### Rust-specific

- [ ] `.unwrap()` / `.expect()` on Result/Option (use `?` or `match`)
- [ ] `panic!()` in library code (should return Result)
- [ ] `clone()` on Arc/Rc (defeats sharing purpose)
- [ ] Integer overflow (use `checked_add` for untrusted input)
- [ ] `as` casts that truncate (`u64 as u32`)
- [ ] Holding `MutexGuard` across `.await` (causes deadlocks)
- [ ] `Vec::with_capacity(0)` or unbounded growth (use `VecDeque` if front-pop)

### Async Rust

- [ ] `.unwrap()` on `tokio::spawn` handle (panic propagates silently)
- [ ] Forgetting `Move` closure in `tokio::spawn`
- [ ] `Rc` shared across async tasks (use `Arc`)
- [ ] `std::sync::Mutex` in async context (use `tokio::sync::Mutex`)
- [ ] `.await` while holding `RefCell::borrow` (panic on double borrow)

### File I/O

- [ ] No path canonicalization before writing (path traversal)
- [ ] No size limit on user-supplied input (memory exhaustion)
- [ ] `unwrap()` on `std::fs::read` (file may not exist)
- [ ] Not closing file handles explicitly (Rust Drop handles this, but be aware)

---

## Failure modes (do NOT make these mistakes)

### Error 1: vague findings

```
❌ "This code looks fragile"
✅ "src/parser.rs:42 — `tokens[i+1]` without bounds check. If `tokens.len() == 1`, this panics."
```

### Error 2: review WHAT not WHY

```
❌ "This function does X" (the code already says that)
✅ "This function does X, but the commit message says Y. Intent unclear."
```

### Error 3: skip categories

```
❌ "I only checked correctness, didn't look at security"
✅ Always run all 6 categories (correctness / security / performance / maintainability / tests / style)
```

### Error 4: edit code during review

```
❌ Reviewer fixes the bug directly
✅ Reviewer reports the bug + suggested fix; author applies (or doesn't, with justification)
```

### Error 5: BLOCKER inflation

```
❌ Mark a naming issue as BLOCKER
✅ Reserve BLOCKER for: crashes / data loss / security holes / wrong behavior
```

---

## Examples

### Example: minimal review

For a small PR (1-2 files, < 50 lines), skip the full template. Just output:

```markdown
**Verdict**: APPROVE
**Issues**:
- src/foo.rs:10 — MINOR — `unwrap()` could panic on empty input. Suggest: `data.first().ok_or_else(|| ...)?`
- src/foo.rs:15 — NIT — trailing whitespace

Overall: solid logic, tests cover the cases. LGTM.
```

### Example: deep review

For large PRs (10+ files, architecture changes), use the full template with explicit B1/M1/M2 sections.

For security-sensitive changes (auth, crypto, file I/O), expand the security checklist section even if no findings — show that you checked each item.

---

## Reference

- ION's reviewer agent: `examples/agents/reviewer.md`
- Architecture decisions: `docs/design/`
- Project conventions: `AGENTS.md`
