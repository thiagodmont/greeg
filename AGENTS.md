# greeg

Rust code-search CLI for agents. Priorities: **performance, fewer tokens,
accuracy, and reliability**. Never improve speed or output size by silently
dropping valid matches.

## Working rules

- Read `README.md` and relevant sections of `references/ARCHITECTURE.md` before changing behavior; use `references/BENCH.md` for benchmark protocols.
- Keep `docs/` local and untracked. Shared guides belong in `references/`.
- Keep one focused PR per implementation step. Preserve unrelated work and existing crate boundaries.
- For bug fixes, reproduce the failure in a regression test first. Check scan/index parity and stdout, stderr, and exit status when relevant.
- Keep exact matching separate from discovery and output budgets. Make relaxed or incomplete results explicit.
- Measure behavior/performance changes against a release baseline on the same corpus and flags. Record binary/corpus versions, latency (median/p95), output bytes/tokens, and correctness; label token estimates and investigate regressions.
- Use disposable benchmark corpora, isolated `GREEG_INDEX_DIR`, `--no-session`, and `GREEG_STATS=0`. Never run edit/soak benchmarks on a working repository.
- Preserve Rust 1.90 and macOS/Linux support. Document CLI or index-format compatibility changes.
- Add comments only when they are necessary and provide value. Whenever possible, write code that is self-explanatory.
- Keep comments clear, concise, and direct. Avoid long comments, as they are less likely to be read.
- Avoid referencing external documents or tool name in branch names, comments or code, since tool can change and those documents may be moved, renamed, or deleted.

## Git Workflow
- Branch naming: `<type>/<short-slug>` (e.g., `feature/fix-header`)
- Allowed types: `feature`, `bugfix`, `hotfix`, `chore`
- Slug format: lowercase kebab-case, concise and descriptive

## Validation

For Rust changes, run focused tests during development, then:

```sh
cargo test --workspace --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Before handoff, review correctness/edge cases, architecture/maintainability,
and security/performance/compatibility/delivery risks. Report measurements,
checks run, and remaining limitations; update docs when behavior changes.
