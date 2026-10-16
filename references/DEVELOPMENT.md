# Development

```bash
cargo build --release
cargo test --workspace                                   # unit + CLI contract tests, no network
cargo clippy --all-targets -- -D warnings

python3 bench/bench.py fetch tokio ktor django           # pinned corpora
python3 bench/parity.py "${GREEG_BENCH_CACHE:-$HOME/.cache/greeg-bench/corpora}" target/release/greeg   # rg parity
python3 bench/bench.py speed --corpora tokio,ktor,django # hyperfine protocol
python3 bench/bench.py oracle tokio django TypeScript-5.9 # SCIP accuracy oracle
```

CI runs build, tests, clippy, a binary-size gate, the rg parity suite and the
small-corpus speed protocol. The nightly workflow adds the large-corpus
benchmarks, the accuracy oracle and a randomized soak
(`bench/soak.py MINUTES GREEG CORPUS… --seed SEED`).

The dependencies workflow (pull requests, `main`, weekly) runs
`cargo deny --locked check advisories licenses sources` against `deny.toml`, in
the root and `fuzz/` workspaces, and
fails when `Cargo.lock` or `fuzz/Cargo.lock` no longer matches its manifests.
A version bump changes both: run `cargo metadata --format-version 1
--manifest-path fuzz/Cargo.toml > /dev/null` to update the fuzz lock.

`bench/index_costs.py BASELINE CANDIDATE --corpora tokio,django --output costs.json`
compares two binaries' index build time, disk size, peak memory, open,
freshness and post-edit refresh costs, in paired randomized runs.

`bench/edits.py CORPUS GREEG`, the index-cost harness and the soak harness copy
each source into a private temporary snapshot and working directory. Dirty, untracked and ignored
files are included; edits and restores affect only the copy. Each corpus gets
its own index and isolated home/config/cache, with sessions and statistics
disabled. Relative executable paths are resolved before changing directories.
Symlinks are copied without following them and cannot be mutation targets.

Allow space for two copies of each corpus plus its index. Copying and initial
setup happen outside the soak duration; restores count toward it. Copies keep
in-tree ignore files but omit Git metadata, parent/global ignore rules and user
configuration, so these runs are not directly comparable with older in-place
runs. A Git source gets an empty repository marker for in-tree ignore handling.
Snapshots are not atomic against concurrent source edits; use a fixed source
revision for reproducible measurements. Exceptions, Ctrl-C and SIGTERM clean up
owned directories; an uncatchable kill or detached index writer can leave
private temporary files, but source corpora remain untouched.

Issues and pull requests are welcome. If you're reporting a wrong or missing
result, `greeg <query> --stats` and `greeg doctor` output are the two most
useful things to include.
