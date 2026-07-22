# Self-Evolution — A→B Architecture Overview

> **Status: Production.** Battle-tested end-to-end. This is the definitive design document.

---

## 1. One-Line Summary

**A orchestrates, B writes code, A merges. A never touches code.**

A (host coordinator ION instance) drives B (container developer ION instance) to edit source, run CI, and self-verify. B's passing changes flow back to A via bind-mount + git merge. A itself is sandboxed: `edit`/`write` are `disallowed_tools`, and `CommandGuard` blocks every host-side mutation path (`sed -i`, `cat >`, `python3 -c`, etc.).

---

## 2. Architecture Diagram

```
 ZCode (user / CI)
   │
   │  ion --host --agent evolver "add fn to global_memory"
   │  (human-level task handed to A)
   ▼
┌──────────────────────────────────────────────────────┐
│  A — Host Coordinator (agent role: evolver)          │
│                                                      │
│  • git worktree add        (isolate workspace)       │
│  • container run           (spawn B + compile ion)   │
│  • container exec B dev    (drive code change)       │
│  • container exec B check  (B runs its own CI)       │
│  • gate: U+FFFD/build/test (reject or accept)        │
│  • git merge worktree      (pull B's commit back)    │
│  • container stop + prune  (cleanup)                 │
│                                                      │
│  A NEVER: edit, write, sed, cargo build on host src  │
└───────────────┬──────────────────────────────────────┘
                │  container exec ... ion --agent developer "..."
                ▼
┌──────────────────────────────────────────────────────┐
│  B — Container Developer (agent role: developer)     │
│                                                      │
│  • Full ION instance: ion binary + LLM + tools       │
│  • read / edit / bash (git, cargo check, cargo test) │
│  • git commit in /workspace (bind-mount = host WT)   │
│  • Does NOT know it's in a container                 │
└───────────────┬──────────────────────────────────────┘
                │  commits land in host worktree instantly
                ▼
        A merges → master → (optional) GitHub PR
```

---

## 3. The Three Orchestration Modes

| Mode | Script | Concurrency | Mechanism | Best For |
|------|--------|-------------|-----------|----------|
| **Serial** | `evolve_self.sh` | 1 B at a time | Sequential `for` loop; full per-task build+test | Reliability, deep verification |
| **Concurrent** | `evolve_concurrent.sh` | N B workers | `bash &` backgrounding + per-task worktree subdirs (`/workspace/wt-N`) | Throughput on independent files |
| **Native** | `evolve_native.sh` | coordinator-driven | ION's own `spawn_worker` + `resume_worker` (no bash `&`) | Pure multi-agent: coordinator → developer → reviewer |

**Serial** is the safe default. **Concurrent** trades some isolation for speed (N isolated git repos inside one container). **Native** is the "ION evolving itself with its own primitives" ideal — the coordinator agent orchestrates developer and reviewer as child workers.

All three share the same bootstrapping (`evolve.sh`): worktree → container → compile `ion` + `ion-worker`.

---

## 4. The 6 Gate Checks

Every change B makes must pass all six gates before A merges. Failure on any gate triggers rollback (or, for reviewer rejection, an auto-fix loop).

| # | Gate | Tool / Command | Fail Action |
|---|------|----------------|-------------|
| 1 | **U+FFFD scan** | `grep -c $'\xef\xbf\xbd' <file>` | Drive B to self-fix, max 2 attempts |
| 2 | **Cargo.toml integrity** | `diff` worktree vs. project | Hard reject — external dep changes forbidden |
| 3 | **Reviewer approval** | `reviewer` agent → `APPROVE` / `REQUEST_CHANGES` | `resume_worker` developer with feedback |
| 4 | **cargo build** | `cargo build --bin ion` | Rollback file, skip task |
| 5 | **cargo test** | `cargo test --lib` | Rollback file, skip task |
| 6 | **clippy** | `cargo clippy` | Warnings logged; errors block merge |

Gates 1–3 run *before* syncing to the main repo. Gates 4–6 run *after* sync, on the host. A file that fails any post-sync gate is reverted with `git checkout --`.

---

## 5. Volume Cache (The "V Scheme")

Apple Container is a Linux VM with no persistent build cache by default — first compile takes 10–20 minutes. `evolve.sh` mounts two named volumes to warm-start every subsequent container:

```bash
container run ... \
  -v ion-cargo-cache:/root/.cargo/registry \   # crate registry + source
  -v ion-target-cache:/workspace/target         # compiled artifacts
```

| Run | cargo registry | target/ | Total build time |
|-----|----------------|---------|------------------|
| 1st (cold) | empty | empty | ~15 min |
| 2nd (warm) | cached | cached | ~30 sec |
| 3rd+ (hot) | cached | cached | ~15 sec |

> **Caveat:** Apple Container volumes are *exclusive* — only one container can mount a given volume at a time. For parallel runs, use **bind mounts** (`-v /tmp/cache:/root/.cargo/registry`) instead of named volumes, or run concurrent workers *inside* a single container (the `evolve_concurrent.sh` approach).

---

## 6. Key Lessons (Top 5)

Hard-won from the full `EVOLVER_LESSONS_LEARNED.md` — these are the non-negotiables:

- **English-only comments.** Non-ASCII (Chinese) characters get corrupted into U+FFFD by some LLMs, which silently breaks `edit` tool pattern matching. The U+FFFD gate exists because of this.
- **GLM-5.2 > DeepSeek for UTF-8 stability.** GLM-5.2 (`zai` provider) produces cleaner byte output; DeepSeek occasionally mangles multi-byte chars. Default `MODEL=glm-5.2`.
- **Apple Container volume is exclusive.** Named volumes cannot be shared across concurrent containers. Use bind mounts or single-container-multi-worktree for parallelism.
- **`evolve.sh` must compile `--bin ion-worker`** (not just `--bin ion`). The native mode spawns workers via `ion-worker`; omitting it causes silent spawn failures.
- **Reviewer reject → `resume_worker` for auto-fix loop.** The reviewer agent returns `REQUEST_CHANGES`; the coordinator feeds that back to the developer via `resume_worker`. Max 2 rounds before giving up — prevents infinite fix cycles.

---

## 7. GitHub PR Flow

For changes destined for the remote (not just local master), `evolve_pr.sh` extends the pipeline with GitHub:

```
B writes code (container)
   │
   ▼
Gate checks pass (U+FFFD, build, test)
   │
   ▼
A creates feature branch:  git checkout -b evolve/<timestamp>
   │
   ▼
A commits + pushes:        git push origin evolve/<timestamp>
   │
   ▼
A opens PR:                gh pr create --base master --head evolve/<timestamp>
   │   (PR body includes task desc, changed files, test count, model)
   ▼
A auto-merges:             gh pr merge --merge --delete-branch
   │   (tests already passed locally → safe to auto-merge)
   ▼
A returns to master:       git checkout master && git pull
```

The PR body is auto-generated with verification proof (gate results, test count, model/provider used), providing an audit trail for every self-evolved commit.

---

## 8. Related Documents

| Document | Purpose |
|----------|---------|
| [EVOLVER_LESSONS_LEARNED.md](EVOLVER_LESSONS_LEARNED.md) | Full problem log (11 issues + solutions) |
| [WATCHDOG_DUAL_VERSION.md](WATCHDOG_DUAL_VERSION.md) | Safe hot-reload of A after merge |
| [WORKFLOW_GATE.md](WORKFLOW_GATE.md) | Kernel delivery verification framework |
| [APPLE_CONTAINER_EXTENSION.md](APPLE_CONTAINER_EXTENSION.md) | Apple Container integration design |
| [TEAM_ORCHESTRATION.md](TEAM_ORCHESTRATION.md) | Multi-agent spawn/resume primitives |
| [CLI_ARCHITECTURE.md](CLI_ARCHITECTURE.md) | ION CLI structure (agents, tools, workers) |
| [../guides/CLI_USAGE.md](../guides/CLI_USAGE.md) | End-user CLI reference |
| [../guides/DEPLOY_ARCH.md](../guides/DEPLOY_ARCH.md) | Deployment topology |

### Key Source Files

| File | Role |
|------|------|
| `scripts/evolve.sh` | Bootstrap: worktree + container + compile |
| `scripts/evolve_self.sh` | Serial batch orchestrator |
| `scripts/evolve_concurrent.sh` | Concurrent (N parallel B workers) |
| `scripts/evolve_native.sh` | Native (coordinator + spawn_worker) |
| `scripts/evolve_pr.sh` | GitHub PR flow |
| `scripts/init-evolve-container.sh` | Standalone container init |
| `scripts/Dockerfile.evolve` | Rust toolchain image |
| `examples/agents/evolver.md` | A's agent definition |
| `examples/agents/developer.md` | B's agent definition |
| `examples/agents/reviewer.md` | Code review agent |
| `src/command_guard.rs` | Host-side mutation blockade |
