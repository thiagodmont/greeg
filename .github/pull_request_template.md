## What and why

<!-- One focused change. Link the issue it closes, if any. -->

## Checklist

- [ ] `cargo test --workspace --locked`, `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` pass
- [ ] A bug fix comes with a regression test that fails without it
- [ ] No valid match is dropped silently; scan and index results agree
- [ ] A performance or output-size change includes measurements against a release build (see `references/BENCH.md`)
- [ ] Docs are updated, and any CLI or index-format change is called out above
