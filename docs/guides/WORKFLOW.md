# ION Workflow Guide

> Complete picture of how work flows through ION from start to finish.

ION is a multi-agent system where specialized agents collaborate to deliver
code. This document describes the four core workflows that drive every task.

## 1. Bug Fix Workflow

A user-reported defect travels through the full pipeline and is verified
before the issue is closed.

```
User reports bug → GitHub Issue
    ↓
Coordinator reads Issue → spawns developer
    ↓
Developer fixes code (in worktree)
    ↓
Stage 1: Reviewer + Architect + QA review
    ↓
Stage 1.5: CI-Agent runs cargo build + test + clippy
    ↓
Stage 2: Merger merges to master
    ↓
Stage 3: PM validates fix
    ↓
Stage 4: User agent tests the fix (--continue for context)
    ↓
User confirms bug is fixed → Close Issue
```

**Key agents:** Coordinator, Developer, Reviewer, Architect, QA, CI-Agent,
Merger, PM, User Agent.

## 2. New Feature Workflow

Features are larger than bug fixes. The Coordinator splits them into
sub-tasks so multiple Developers can work in parallel.

```
User requests feature → GitHub Issue (label: enhancement)
    ↓
Coordinator reads Issue → splits into sub-tasks
    ↓
Multiple Developers work in parallel (spawn_worker wait=false)
    ↓
Stage 1: Reviewer + Architect + QA review each change
    ↓
Stage 1.5: CI-Agent verifies all changes pass CI
    ↓
Stage 2: Merger merges all branches
    ↓
Stage 3: PM validates feature completeness
    ↓
Stage 4: User agent tests the feature
    ↓
If issues found → Loop back to Developer (max 3 rounds)
```

**Why parallel?** `spawn_worker` with `wait=false` returns immediately,
letting the Coordinator fan out work and collect results later.

## 3. CI Failure Auto-Fix Flow

When CI fails, the CI-Agent diagnoses the log and delegates a fix — no
human intervention required unless it loops three times.

```
CI fails on push/PR
    ↓
CI-Agent detects failure (gh run view --log-failed)
    ↓
CI-Agent spawns developer to fix
    ↓
Developer pushes fix → CI re-runs
    ↓
CI passes → PR ready for merge
CI fails again → Loop (max 3 attempts)
CI fails 3rd time → Escalate to human
```

**Escalation rule:** after 3 failed attempts, the CI-Agent stops and opens
an issue summarizing the persistent failure for a human maintainer.

## 4. Self-Evolution Flow

ION can improve itself. A coordinator spawns a developer in an isolated
container; six quality gates must all pass before the change is merged.

```
A (coordinator) receives evolution task
    ↓
A spawns B (developer in container)
    ↓
B writes code + commits
    ↓
6 gates: U+FFFD + Cargo.toml + Reviewer + build + test + clippy
    ↓
All pass → Push feature branch → GitHub PR → Auto-merge
Any fail → Reject (or B self-fixes via resume_worker)
```

**The six gates:**

| # | Gate | Purpose |
|---|------|---------|
| 1 | U+FFFD check | No corrupted / mojibake characters |
| 2 | Cargo.toml | Version bumped, deps consistent |
| 3 | Reviewer | Code-quality + design review |
| 4 | `cargo build` | Compiles cleanly |
| 5 | `cargo test` | All tests pass |
| 6 | `cargo clippy` | No lint warnings |

If a gate fails, the developer can self-correct via `resume_worker` before
re-attempting, avoiding a full respawn.

## Stage Reference

All workflows share the same convergence stages after development:

| Stage | Owner | Responsibility |
|-------|-------|----------------|
| 1 | Reviewer / Architect / QA | Human-style review of the change |
| 1.5 | CI-Agent | Automated build + test + clippy |
| 2 | Merger | Merge approved branch to `master` |
| 3 | PM | Validate the change meets the goal |
| 4 | User Agent | End-to-end test from the user's perspective |

---

For agent-specific details, see `examples/agents/*.md`.
