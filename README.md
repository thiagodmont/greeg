# greeg

A grep for coding agents. greeg accepts ripgrep's flags and regex dialect,
answers from a persistent per-repository index instead of walking the tree,
classifies every hit with syntax information, ranks by usefulness and shapes
the answer to a token budget with an honest account of what was left out.

```
greeg get_queryset                 # ranked hits, definitions first, ~600 tokens
greeg -w respond -l                # files only, rg-shaped, pipe-safe
greeg 'fn poll_read' -t rs -C 3    # ripgrep flags work as they do in rg
greeg createSourceFile --json      # ripgrep JSON Lines + kind/symbol/facets/footer
greeg respond --budget 0           # unlimited, path order, byte-for-byte rg parity
```

Warm queries on a 60k-file tree take 3–10 ms plus a freshness check (about
12 ms with FSEvents on macOS, 40 ms with a stat pass), against 0.8–3 s for
ripgrep. Numbers, protocol and caveats are in [`docs/BENCH.md`](docs/BENCH.md).

## Install

Requirements: macOS (arm64, x86_64) or Linux (x86_64, aarch64). ripgrep is
not required.

**Homebrew** (tap published with each release):

```
brew install thiagodmont/greeg/greeg
```

**Release tarball** (binary + man page, sha256 alongside):

```
v=0.3.0; t=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
curl -sSL https://github.com/thiagodmont/greeg/releases/download/v$v/greeg-$v-$t.tar.gz | tar xz
sudo install greeg-$v-$t/greeg /usr/local/bin/
```

**From source** (Rust 1.90 or newer):

```
cargo install --git https://github.com/thiagodmont/greeg greeg
# or, from a checkout:
cargo install --path crates/greeg
```

Check the installation:

```
greeg --version
greeg doctor            # index location, freshness mode, languages, disk use
```

## Set up for Claude Code

```
greeg hook claude --dry-run    # show what would change
greeg hook claude              # install the hook and the skill file
```

This adds a `PreToolUse` hook for `Bash` in `~/.claude/settings.json` that
rewrites plain `rg` and `grep` invocations to `greeg` (flags mapped, `--include`
→ `-g`, grep basic regexps translated), and writes
`~/.claude/skills/greeg/SKILL.md` so the agent knows the verbs. Commands
greeg cannot reproduce exactly (`-v`, `-o`, `--files`, `-m`, redirections,
globs, expansions, multiple `-e`) are left untouched. If you use `Bash(rg:*)`
allow rules, add `Bash(greeg:*)` next to them. `greeg hook claude --uninstall`
removes both files' entries.

Other agents can call `greeg` directly; it prints to stdout, exits 0 on hits,
1 on none and 2 on error, exactly like ripgrep.

## Is it worth it? `greeg stats`

Off by default. `greeg stats enable` (or `GREEG_STATS=1` in one shell) makes
the hook record every `rg`/`grep` call it rewrote and every greeg run record
its wall time and output size. `greeg stats` then prints latency and token
distributions (avg, min, p50, p95, p99, max) for rewritten searches, direct
searches and symbol verbs. `greeg stats replay` runs the original `rg`/`grep`
commands and their greeg rewrites side by side, on the same machine and the
same tree, and the report adds the counterfactual and the savings line:

```
greeg stats enable
# ... work with the agent for a while ...
greeg stats replay --runs 3          # on a quiet machine
greeg stats --since 7d --repo .      # --json for machine output, --verbose to list commands
```

The records hold search patterns and paths, which is why this is opt-in.
They live under the user cache dir (`~/Library/Caches/greeg/stats` on
macOS, `~/.cache/greeg/stats` elsewhere), mode 0600, never leave the machine,
rotate at 16 MB, and `greeg stats clear` deletes them. Output is never
stored, only its size and token estimate. Claude Code truncates Bash output
at 30 000 characters, so rg tokens are reported raw and capped at that size;
the savings use the capped figure. `--cap N`, `greeg stats enable --cap N` or
`stats_cap = N` in `~/.config/greeg/config.toml` change it. `GREEG_STATS=0`
overrides the config file.

## How it works

The first query in a repository is answered by a ripgrep-speed scan while a
trigram index is built in the background (under `~/Library/Caches/greeg/` on
macOS, `$XDG_CACHE_HOME/greeg/` elsewhere; override with `GREEG_INDEX_DIR`).
Later queries open only candidate files. Edits are picked up per query by a
stat pass or, on macOS, the FSEvents log, and applied as delta segments;
`.gitignore` changes trigger a rebuild.

The index also holds every definition (tree-sitter for Python, TypeScript,
JavaScript, Rust and Kotlin; a regex extractor elsewhere), comment and string
spans, resolved imports (`tsconfig.json` `paths`/`baseUrl` and workspace
packages included) and an import-graph PageRank. Edited files keep their
rank and their edges: a delta resolves imports against the base. Every hit is classified
(`def`, `call`, `import`, `type`, `member`, `ident`, `doc`, `comment`,
`string`) and carries its enclosing symbol. Test, vendored, generated,
minified and mock files are demoted, never hidden, and the footer always says
what was cut. A bare identifier is answered as a whole word: matches inside a
longer identifier (`_get_queryset_methods` for `get_queryset`) are named on a
`related` line instead of taking answer lines, unless nothing matches the whole
word. `--budget 0`, `-l` and `-c` keep ripgrep's match set exactly.

```
greeg index                 # build now instead of on first query
greeg index --status        # manifest: files, generation, deltas
greeg index --check         # run a freshness check and apply it
greeg pat --fresh none      # trust the index (back-to-back calls, benchmarks)
greeg pat --no-index        # scan the tree like ripgrep
```

## Symbol verbs

```
greeg def JoinHandle              # where it is defined: ranked, signature, doc line
greeg refs Semaphore              # references grouped by kind (call, type, import, …)
greeg callers spawn_blocking --depth 2
greeg impls Future                # implementations and subclasses
greeg outline tokio/src/sync/oneshot.rs
greeg map tokio/src/sync          # important files and directories by PageRank
greeg impact get_queryset         # what breaks: WILL / MAY BREAK / REVIEW
greeg SpawnBlocking               # no hit → split tokens → spawn_blocking
greeg -w Foo --precise            # exact call/type/member kinds from the syntax tree
```

All verbs accept the search flags (`--budget`, `--json`, `--no-tests`,
`--root`, …) and answer in a few milliseconds from the index. Session memory
(per agent process, or `--session ID`) avoids repeating context already shown
and biases ranking toward recently seen files; `--no-session` turns it off.

## Flags

ripgrep-compatible: `-i -S -s -w -x -F -U -n -l -c -A -B -C -g -t -T -e -j
--no-ignore --hidden -u -uu --max-columns --max-filesize --json --sort path`
(cosmetic flags such as `-N -H --color --no-heading --column` are accepted and
ignored).

greeg: `--budget N` (tokens, default 2000, 0 = unlimited) · `--mode
files|outline|content|block` · `--kind def,call,...` · `--chain` · `--near
PATH` · `--no-tests --no-vendored --no-generated --all` · `--per-file N` ·
`--no-ladder` · `--fresh auto|none|stat|fsevents` · `--no-index` ·
`--index-dir DIR` · `--stats` · `--precise` · `--session ID` · `--no-session`

Output formats are specified in [`docs/OUTPUT.md`](docs/OUTPUT.md); the
on-disk index in [`docs/FORMAT.md`](docs/FORMAT.md); design and plan in
[`docs/DESIGN.md`](docs/DESIGN.md) and [`docs/PLAN.md`](docs/PLAN.md).

## Extra languages

Drop a directory into `~/.config/greeg/lang/<name>/` (or `$GREEG_LANG_DIR`)
with `spec.toml` (name, extensions, comment and string delimiters),
`grammar.so`/`grammar.dylib` (the tree-sitter parser, `cc -shared -fPIC -O2
-I src src/parser.c [src/scanner.c]`) and `tags.scm` using greeg's captures
(`@def.function`, `@def.class`, …, `@name`, `@supers`, `@noncode.comment`,
`@noncode.string`, `@import`). `greeg lang check DIR` validates it. See
`docs/DESIGN.md` §11.

## Robustness

A panic in the index path, a corrupt component or a truncated mmap all end in
a scan-mode answer plus a background rebuild; writers hold a lock and publish
atomically, readers never lock. `bench/soak.py MINUTES GREEG CORPUS...` runs
randomized queries and edits against ripgrep.

## Development

```
cargo build --release && cargo test --workspace && cargo clippy --all-targets -- -D warnings
python3 bench/bench.py fetch tokio ktor django          # pinned corpora into $GREEG_BENCH_CACHE
python3 bench/parity.py $GREEG_BENCH_CACHE target/release/greeg   # rg parity: table + matrix
python3 bench/bench.py speed --corpora tokio,ktor,django # hyperfine protocol vs grep / rg
python3 bench/bench.py oracle tokio django TypeScript-5.9 # SCIP oracle
python3 bench/bench.py report                            # docs/BENCH.md
```

CI runs build, tests, clippy, a binary-size gate, the parity suite and the
small-corpus speed protocol on macOS and Linux; the nightly workflow adds the
kernel benchmarks, the oracle and a soak. Releases are built by
`.github/workflows/release.yml` on a `v*` tag, with a Homebrew formula
rendered from `homebrew/greeg.rb.in`.

## License

MIT or Apache-2.0, at your option.
