# greeg

**Code search built for coding agents.** Your agent searches with `rg` and
`grep` and gets a wall of matches. greeg gives it a ranked answer instead:
definitions first, and small enough to read.

[![CI](https://github.com/thiagodmont/greeg/actions/workflows/ci.yml/badge.svg)](https://github.com/thiagodmont/greeg/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/thiagodmont/greeg)](https://github.com/thiagodmont/greeg/releases)
[![license](https://img.shields.io/badge/license-MIT-blue)](#license)

One search, `isIdentifier` in the TypeScript compiler:

| command | what the agent gets |
|---|---|
| `rg isIdentifier` | 774 lines, **18,987 tokens**. Claude Code cuts Bash output at 30,000 characters, so the agent sees about a third of it. |
| `greeg isIdentifier` | **548 tokens**, 35× fewer: the definitions first, then a summary of every other hit. |

- **Fewer tokens.** A broad search fits a token budget instead of flooding the
  context. A narrow one-file search costs about the same as grep.
- **Definitions first.** Ask where a symbol is defined (`greeg def NAME`) and
  the first result is right 96 % to 100 % of the time on three open-source
  repositories, against 37 % to 75 % for the `rg` search an agent would type
  ([benchmarks](#benchmarks)).
- **Fast.** It answers from an index it builds and keeps fresh by itself. On
  the TypeScript compiler (74k files), a search takes tens of milliseconds
  where `rg` takes over a second ([benchmarks](#benchmarks)).
- **Nothing hidden.** Every answer ends by saying what was left out and which
  flag brings it back.
- **More than grep.** Callers, implementations, the impact of a change, and
  questions in plain words ([examples](#use-it)).

Works with Claude Code and Codex through a hook, and with any agent that can
run a command. Python, TypeScript/JavaScript, Rust and Kotlin get full syntax
support; other languages get text search, ranking and budgets.

## Install

```bash
brew install thiagodmont/greeg/greeg
```

macOS (arm64, x86_64) or Linux (x86_64, aarch64; glibc 2.35+). ripgrep is not
required.

<details>
<summary>Other ways to install</summary>

**Release archive** (binary and man page; checksums and a provenance
attestation alongside, see [verifying an archive](references/USAGE.md#verifying-a-release-archive)):

```bash
v=0.14.1; t=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
curl -sSL https://github.com/thiagodmont/greeg/releases/download/v$v/greeg-$v-$t.tar.gz | tar xz
sudo install greeg-$v-$t/greeg /usr/local/bin/
```

**From source** (Rust 1.90 or newer):

```bash
cargo install --git https://github.com/thiagodmont/greeg greeg
```

</details>

## Connect your agent

```bash
greeg hook claude       # Claude Code
greeg hook codex        # Codex 0.131+  (then run /hooks in Codex to trust it)
```

This installs two things, and **you don't change how you prompt**:

- a **hook** that sends the `rg` and `grep -r` shell commands your agent runs
  to greeg; a command it can't translate exactly runs with the original tool.
- a **skill file** that teaches the agent greeg's symbol commands.

Add `--dry-run` to preview the install, or `--uninstall` to remove it.
`greeg hook explain 'grep -rn foo src'` shows what the hook would do with a
command. After you upgrade greeg, run the hook command again to update the
skill; `greeg doctor` says when it is outdated, and a skill you edited is kept
as it is.

Two things to check:

- **Allow rules.** If you use `Bash(rg:*)` allow rules, add `Bash(greeg:*)`
  next to them.
- **Other rewriting hooks.** If another hook also rewrites `rg` (RTK, for
  example), only one can win. For RTK, add `exclude_commands = ["grep", "rg"]`
  under `[hooks]` in its config.

<details>
<summary>What the hook rewrites, and what it leaves alone</summary>

- **Rewritten:** `rg` and `grep -r`, also inside `;`/`&&` lists and before
  `| head`. A `grep -rn` or `grep -rl` is also rewritten before other readers
  such as `| sort`.
- **Left to the original tool:** most pipelines, redirections, grep without
  `-r`, and every search while a nonempty `RIPGREP_CONFIG_PATH` is set.
- **How a rewritten `grep -r` differs from grep:** it searches hidden files,
  as grep does, but not version-control data (`.git/` and the like). It skips
  ignored and binary files, which grep searches, and says so on stderr.

The full rules are in the [usage guide](references/USAGE.md#agent-hook-details).

</details>

<details>
<summary>Other agents and scripts: output and exit codes</summary>

Any other agent can call `greeg` directly. It prints to stdout.

| exit | meaning |
|---:|---|
| `0` | exact hits |
| `1` | no exact hits (it may still print relaxed, discovery-only results) |
| `2` | error |

When the reader of its output leaves (`| head`), greeg ends quietly by
SIGPIPE, as grep does (shell status 141).

</details>

## What your agent sees

```console
$ greeg isIdentifier
definitions (2 of 4)
src/compiler/factory/nodeTests.ts
  318  export function isIdentifier(node: Node): node is Identifier {
src/compiler/parser.ts
  2318  function isIdentifier(): boolean {  ‹ Parser
  +2 test definitions (--all)

isIdentifier  774 hits · 109 files · def 19 call 466 member 2 ident 281 comment 6 · test 19
areas  src/compiler 322  src/services/codefixes 140  src/compiler/transformers 107  src/services 107  …

top hits
scripts/dtsBundler.mjs
  310 call  if (ts.isIdentifier(node) && ts.isPartOfTypeNode(node)) {  ‹ verifyMatchingSymbols
src/compiler/binder.ts
  2640 call  return isIdentifier(node) && (node.escapedText === "eval" || node.escapedText === "arguments");  ‹ createBinder.isEvalOrArgumentsIdentifier
  …
related  isIdentifierText 16  isIdentifierPart 6  isIdentifierStart 6  isThisIdentifier 5

12/774 hits · 4/109 files · 5 files demoted (19) · skipped 9 binary, 2 huge · ~573 tokens
next: --kind def | -g 'src/compiler/**' | --no-tests
```

The definitions come first. Then a summary of all 774 hits: their kinds, where
they live, the top call sites with the function each sits in, and similar
names. The footer says what was left out, and `next:` says which flag narrows
the search. Nothing is silently dropped. (The `~573` in the footer is greeg's
own token estimate; the o200k tokenizer counts 548.)

## Use it

Search like ripgrep:

```bash
greeg get_queryset                  # ranked hits, definitions first
greeg 'fn poll_read' -t rs -C 3     # ripgrep flags behave as in rg
greeg -w respond -l                 # files only, pipe-safe
greeg respond --budget 0            # every match, in path order
greeg createSourceFile --json       # typed JSON Lines for tools
```

Ask about symbols, which grep can't:

```bash
greeg def JoinHandle                # where it's defined, with signature and doc
greeg refs Semaphore                # references grouped by kind
greeg callers spawn_blocking        # the functions that call it
greeg impls Future                  # implementations and subclasses
greeg impact get_queryset           # files likely affected by a change
greeg impact --diff                 # what your uncommitted changes touch, and every use to update
greeg show src/sync/oneshot.rs:340  # the whole definition around a line
greeg outline src/sync/oneshot.rs   # the shape of a file
greeg map tokio/src/sync            # the most important files, by import graph
```

Or ask in plain words when you don't know the name:

```bash
greeg find where retries back off   # files ranked for the question, with their matching definitions
```

Answers fit 2,000 tokens by default. Change it per call (`--budget 5000`,
`--budget 0` for everything) or once (`greeg budget high`). The
[usage guide](references/USAGE.md) covers every flag, the JSON formats and the
matching rules.

## Benchmarks

**Tokens.** The `isIdentifier` example at the top is the TypeScript compiler
at v5.9.2, counted with the o200k tokenizer, greeg 0.14.1 at its default
2,000-token budget (2026-11-24).

**Finding the definition.** How often the first result is the definition,
scored against compiler-grade indexes (SCIP) for each repository, with greeg
0.14.1 (2026-11-24). `greeg` is `greeg def NAME`; `rg-def` is `rg -n` with
the language's definition regex, the search an agent types when it wants the
definition; `rg -nw` and `grep` are plain word searches. About 75 names per
repository, so each figure is ±10 points.

| repository | **greeg** | `rg-def` | `rg -nw` | `grep` |
|---|---:|---:|---:|---:|
| TypeScript 5.9 | **96 %** | 37 % | 28 % | 47 % |
| django | **100 %** | 75 % | 29 % | 29 % |
| tokio | **96 %** | 68 % | 16 % | 19 % |

**Speed** on the TypeScript compiler 5.9 (74k files, 368 MB). `rg` and `grep`
print every match; `greeg` prints the ranked 2,000-token answer an agent reads,
and `greeg --budget 0` prints every match, like rg. Measured with greeg 0.3.0
(2026-09-06) on an Apple M4 Pro, warm page cache.

| query | `grep` | `rg` | `rg -j4` | **greeg** | `greeg --budget 0` |
|---|---:|---:|---:|---:|---:|
| `createSourceFile` (97 matches) | 4.29 s | 2.42 s | 672 ms | **30 ms** | 35 ms |
| `node` (30,445 matches) | 4.52 s | 2.33 s | 882 ms | **43 ms** | 67 ms |

Protocols, raw data and caveats: [speed](references/BENCH.md#speed) and
[accuracy](references/BENCH.md#quality-scip-oracle) in `references/BENCH.md`.

## How it works

- **No setup.** The first query scans like ripgrep while an index is built in
  the background. The index lives in your cache directory, never in the repo.
- **Always fresh.** Each query checks for edits and reads changed files
  directly, so answers include your latest edits.
- **Understands code.** Every hit is classified from syntax (definition, call,
  import, type, comment, …) and tagged with the symbol it sits in. Other
  languages can be added with a tree-sitter grammar; see the
  [usage guide](references/USAGE.md#flags-and-output-formats).
- **Ranks, then budgets.** Definitions outrank calls, source outranks tests,
  central files outrank leaves. Tests and generated files are demoted, never
  hidden, and the footer always says what was cut.
- **Fails safe.** A damaged or outdated index is rebuilt in the background,
  and the query is answered by a scan instead.

Design and internals: [`references/ARCHITECTURE.md`](references/ARCHITECTURE.md).

## Is it actually helping?

Measure it on your own repositories, against your agent's real searches. It's
off by default and nothing leaves your machine:

```bash
greeg stats enable
# ... work with your agent for a while ...
greeg stats replay --runs 3     # re-runs the original rg/grep commands side by side
greeg stats                     # the comparison
```

```
savings vs rg/grep · 145 replayed queries · same machine and tree · rg output capped at 30,000 bytes
                              rg/grep        greeg        saved
  tokens, total                86,258       66,310       19,948  23%
  tokens, typical query           311          318          -30  greeg smaller on 37 queries, larger on 108
  time, total                  4.64 s       866 ms       3.77 s  81%
  time, typical query          3.4 ms       6.1 ms      -2.3 ms  greeg faster on 53 queries, slower on 92
  typical = median; on those rows `saved` is the median of the per-query differences
  the 3 largest token wins hold 19,754 tokens; the other 142 queries net 194
  the 3 largest time wins hold 3.25 s; the other 142 queries net 517 ms
```

Most savings come from a few broad searches; a typical one-file search costs
about the same. More in [`references/STATS.md`](references/STATS.md).

## Documentation

- [Usage guide](references/USAGE.md): every flag, budgets, JSON, the index, sessions, the hook, environment variables
- [JSON output](references/JSON.md): `--json`, `--json=rg`, `--capabilities`, `--explain`
- [Architecture](references/ARCHITECTURE.md): how the index, ranking and freshness work
- [Benchmarks](references/BENCH.md): every measurement, with protocol and raw data
- [Statistics](references/STATS.md): measuring greeg on your own traffic
- [Development](references/DEVELOPMENT.md): building, testing and benchmarking

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
For a wrong or missing result, include the output of `greeg <query> --stats`
and `greeg doctor`. Report security problems privately, as
[SECURITY.md](.github/SECURITY.md) describes.

## License

MIT. See [`LICENSE-MIT`](LICENSE-MIT).
