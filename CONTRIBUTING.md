# Contributing

Issues and pull requests are welcome.

## Reporting a wrong or missing result

greeg must never drop a valid match silently, so these reports matter most.
Include:

- `greeg --version` and your OS
- the exact command, and the `rg` command it stands for
- the output of `greeg <query> --stats` and `greeg doctor`
- if you can, a small repository or file that reproduces it

Security problems go through private reporting instead; see
[SECURITY.md](.github/SECURITY.md).

## Making a change

1. Read the [README](README.md) and the relevant part of
   [ARCHITECTURE.md](references/ARCHITECTURE.md) before changing behavior.
2. Keep one focused change per pull request. Name the branch
   `<type>/<short-slug>`, where the type is `feature`, `bugfix`, `hotfix` or
   `chore` and the slug is lowercase kebab-case (for example
   `bugfix/glob-precedence`).
3. For a bug fix, add a regression test that fails before the fix. Check scan
   and index parity, and stdout, stderr and exit status when they matter. A
   defect you reproduce but don't fix goes in
   `crates/greeg/tests/known_gaps.rs` as an ignored test.
4. For a performance or output-size change, measure it against a release
   build on the same corpus and flags, following
   [BENCH.md](references/BENCH.md), and put the numbers in the pull request.
5. Keep Rust 1.90 and macOS/Linux support. Say so in the pull request when
   the CLI or the index format changes, and update the docs.

## Checks

Run these before opening a pull request; CI runs them too:

```bash
cargo test --workspace --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

[DEVELOPMENT.md](references/DEVELOPMENT.md) covers benchmarks, parity runs,
fuzzing and the CI workflows.

## License

By contributing, you agree that your contribution is licensed under the
[MIT license](LICENSE-MIT).
