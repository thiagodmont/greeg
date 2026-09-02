# greeg

A grep for coding agents. Accepts ripgrep's flags, returns ranked, syntax-aware,
budgeted results instead of unranked lines.

```
greeg get_queryset                 # ranked hits, definitions first, ~2k tokens
greeg -w respond --mode files      # files with counts
greeg "fn poll_read" -t rs --mode outline
greeg createSourceFile --json      # ripgrep JSON Lines + kind/symbol/facets/footer
greeg respond --budget 0           # unlimited, path order: byte-for-byte rg parity
```

Every hit is classified (`def`, `call`, `import`, `type`, `member`, `ident`,
`doc`, `comment`, `string`) and carries its enclosing symbol chain. Tests,
vendored, generated and minified files are demoted, never hidden, and the
footer always says what was cut. When a query is broad the first answer is a
facet summary (by kind, area, language, flag) plus the definitions, so the
next query can be narrow.

On first use in a repository greeg answers by scanning and builds a trigram
index in the background (`~/Library/Caches/greeg/<repo>-<hash>/` on macOS,
`$XDG_CACHE_HOME/greeg/` elsewhere). Later queries open only candidate files:
about 12 ms instead of a second on a 66k-file tree. Edits are picked up per
query by a stat pass, or on macOS by the FSEvents log, and applied as delta
segments without a rebuild.

```
greeg index                 # build now (otherwise it happens on first query)
greeg index --status        # manifest: files, generation, deltas
greeg index --check         # run a freshness check and apply it
greeg pat --fresh none      # trust the index (benchmarks, back-to-back calls)
greeg pat --no-index        # scan the tree like ripgrep
```

The index also holds every definition (tree-sitter for Python, Rust,
JavaScript, TypeScript, Kotlin; regex fallback elsewhere), comment/string
spans, resolved imports and an import-graph PageRank, so hits are classified
from stored tables and symbol questions are answered directly:

```
greeg def JoinHandle              # where is it defined, ranked, with signature and doc
greeg refs Semaphore              # references grouped by kind (call, type, import, …)
greeg callers spawn_blocking --depth 2
greeg impls Future                # implementations / subclasses
greeg outline tokio/src/sync/oneshot.rs
greeg map tokio/src/sync          # important files and directories by PageRank
greeg impact get_queryset         # what breaks: WILL / MAY BREAK / REVIEW
greeg SpawnBlocking               # no hit → split tokens → spawn_blocking
greeg -w Foo --precise            # exact call/type/member kinds from the syntax tree
```

Session memory (per agent process, or `--session ID`) drops context that was
already shown, biases ranking toward recently seen files and flags repeated
queries. `--no-session` turns it off.

Status: M6 (scan mode, trigram index, symbols, verbs, hardening, distribution, benchmark suite). Design, plan and format in `docs/`.

## Install

```
cargo install --path crates/greeg        # from this checkout
cargo build --release && ./target/release/greeg --help
greeg man > greeg.1                       # man page
```

Release tarballs for macOS (arm64, x86_64) and Linux (x86_64, aarch64) are
built by `.github/workflows/release.yml` on a `v*` tag, with a Homebrew
formula rendered from `homebrew/greeg.rb.in` (see `homebrew/README.md`).

## Agents

```
greeg hook claude --dry-run    # show what would be installed
greeg hook claude              # Claude Code: rewrite rg/grep Bash calls to greeg + a skill file
greeg doctor                   # index health, freshness mode, languages, disk use
```

The hook (`greeg hook run`) rewrites only plain `rg`/`grep` invocations
(first pipeline segment, no expansions or globs); `-v`, `-o`, `--files` and
other semantics greeg does not have are left alone.

## Extra languages

Drop a directory into `~/.config/greeg/lang/<name>/` (or `$GREEG_LANG_DIR`)
with `spec.toml` (name, extensions, comment and string delimiters),
`grammar.so`/`grammar.dylib` (the tree-sitter parser: `cc -shared -fPIC -O2
-I src src/parser.c [src/scanner.c]`) and `tags.scm` using greeg's captures
(`@def.function`, `@def.class`, …, `@name`, `@supers`, `@noncode.comment`,
`@noncode.string`, `@import`). `greeg lang check DIR` validates it and
reports coverage. Extra languages get index-time symbols and `-t <name>`;
see `docs/DESIGN.md` §11.

## Robustness

A panic in the index path, a corrupt component or a truncated mmap (SIGBUS)
all end in a scan-mode answer plus a background rebuild; nothing is ever
repaired in place. `GREEG_DEBUG_PANIC=1` / `GREEG_DEBUG_SIGBUS=1` inject
those faults; `bench/soak.py MINUTES GREEG CORPUS...` runs randomized
queries and edits against `rg`.

## Flags

ripgrep-compatible: `-i -S -s -w -x -F -U -n -l -c -A -B -C -g -t -T --no-ignore --hidden -j --max-columns --max-filesize --json`

greeg: `--budget N` (tokens, default 2000, 0 = unlimited) · `--mode files|outline|content|block` ·
`--kind def,call,...` · `--near PATH` · `--no-tests --no-vendored --no-generated --all` ·
`--per-file N` · `--no-ladder` · `--fresh auto|none|stat|fsevents` · `--no-index` · `--index-dir DIR` · `--stats` ·
`--precise` · `--session ID` · `--no-session` · `-e PATTERN` (a pattern that looks like a verb)

Verbs: `def NAME [--from FILE] [--def-kind K]` · `refs NAME` · `callers NAME [--depth N]` · `impls NAME` · `outline FILE` · `map [DIR]` · `impact NAME` · `index [--status|--check|--phase1]` · `doctor` · `man` · `hook claude` · `lang check DIR`

## Bench

```
python3 bench/parity.py CORPORA_DIR target/release/greeg   # (path,line) parity with rg
sh bench/timing.sh CORPORA_DIR target/release/greeg         # hyperfine vs rg -j1/-j4/-jN
python3 bench/edits.py CORPUS_DIR target/release/greeg      # edit bursts, adds, deletes, renames vs rg
python3 bench/bench.py fetch                                # pinned corpora (bench/corpora.toml) into ~/.cache/greeg-bench
python3 bench/bench.py speed --corpora tokio,ktor,django    # hyperfine protocol vs grep / rg, index build, RSS
python3 bench/bench.py oracle tokio django TypeScript-5.9   # SCIP oracle: Acc@k, reference recall, classification, context
python3 bench/bench.py gate speed|kernels                   # regression gates against bench/baselines/<host>
python3 bench/bench.py report                               # docs/BENCH.md
cargo bench -p greeg-index --bench kernels                  # criterion kernels (gram, plan, lexer, extract, postings)
```
