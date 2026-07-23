# Contributing to Ion

Thanks for your interest in improving Ion!

## 1. Reporting Bugs

```bash
gh issue create --title "[bug] short description" \
  --label bug \
  --body "Steps to reproduce, expected vs actual, ion version, OS."
```

Include: steps to reproduce, expected vs actual, `ion --version`, OS.

## 2. Requesting Features

```bash
gh issue create --title "[feat] short description" \
  --label enhancement \
  --body "Motivation, proposed API, alternatives considered."
```

## 3. Dev Environment Setup

Requires Rust stable and Git.

```bash
git clone https://github.com/dyyz1993/ion.git
cd ion
cargo build --bin ion --bin ion-worker --bin agent-demo
./target/debug/ion --version   # verify build
```

## 4. Running Tests

```bash
cargo test --lib               # library unit tests
cargo test -p ion-provider     # provider package
cargo test --tests             # integration tests
```

## 5. Submitting Pull Requests

1. Branch from `master`:
   ```bash
   git checkout master && git pull
   git checkout -b feat/my-feature
   ```
2. Make focused commits.
3. Push and open a PR:
   ```bash
   git push -u origin feat/my-feature
   gh pr create --title "feat: short summary" --body "What and why."
   ```
4. CI must pass before merge.

## 6. Code Style

- **ALL comments in English.** No non-English comments.
- **No U+FFFD** (replacement character). The gate rejects commits containing it.
- **Clippy clean** — `cargo clippy --lib -- -D warnings` must yield zero warnings.
- **Formatted** — run `cargo fmt`; `cargo fmt -- --check` must pass.
- Keep functions small; prefer clear code over clever code.

## 7. Self-Evolution Workflow (Optional)

The repo ships an AI self-evolution pipeline (`scripts/evolve_pr.sh`) that
proposes code changes and opens PRs automatically.

```bash
bash scripts/evolve.sh                 # start container + compile
bash scripts/evolve_pr.sh "task desc"  # B writes code, gate runs, PR opens
```

Gate enforces: no U+FFFD, `cargo build` + `cargo test` must pass. PRs go
through the same CI as human PRs. Optional — human contributors skip this.

## 8. CI Requirements

Every PR must pass **PR Gate** (`.github/workflows/pr-gate.yml`, strict):

| Check          | Command                                          |
|----------------|--------------------------------------------------|
| Build          | `cargo build --bin ion --bin ion-worker --bin agent-demo` |
| Clippy         | `cargo clippy --lib -- -D warnings`              |
| Format check   | `cargo fmt -- --check`                           |
| Unit tests     | `cargo test --lib`                               |
| Provider tests | `cargo test -p ion-provider`                     |
| Integration    | `cargo test --tests`                             |

Run these locally before pushing. Fix failures rather than disabling checks.

---

Questions? Open a `question` labeled issue. Happy hacking!
