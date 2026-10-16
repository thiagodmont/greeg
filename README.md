# greeg

**Code search built for coding agents.** It speaks ripgrep's flags, answers
from an index, puts definitions first, and fits its ranked answers in a token
budget, so your agent reads the hits that matter instead of a wall of matches.

[![CI](https://github.com/thiagodmont/greeg/actions/workflows/ci.yml/badge.svg)](https://github.com/thiagodmont/greeg/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/thiagodmont/greeg)](https://github.com/thiagodmont/greeg/releases)
[![license](https://img.shields.io/badge/license-MIT-blue)](#license)

## The problem

Your agent runs `rg isIdentifier` on the TypeScript compiler. ripgrep does its
job: **774 lines, 18,987 tokens**. Claude Code truncates tool output at 30,000
characters, so the agent sees about a third of it, and the definitions it was
looking for may sit past the cut.

greeg answers the same question in **666 tokens** (o200k; its footer shows
greeg's own estimate), definitions first:

```console
$ greeg isIdentifier
definitions (4 of 6)
src/compiler/factory/nodeTests.ts
  318  export function isIdentifier(node: Node): node is Identifier {
src/compiler/parser.ts
  2318  function isIdentifier(): boolean {  ‹ Parser
src/compiler/scanner.ts
  71  isIdentifier(): boolean;  ‹ Scanner
tests/cases/compiler/reverseMappedUnionInference.ts  [test]
  24  declare function isIdentifier(node: unknown): node is Identifier;
  +2 test definitions (--all)
…
14/767 hits · 8/105 files · 4 files demoted (17) · skipped 2 huge · ~708 tokens
```

Between the definitions and the footer, greeg summarizes the other 753 hits:
their kinds, where they live, the top call sites with the function each sits
in, 122 imports folded into one line, and similar names. The footer says what
was left out and which flag brings it back. Nothing is silently dropped.

## Why use it

**Faster.** The index means a query opens only the files that could match.
On TypeScript 5.9 (74k files, 368 MB):

| query | `grep` | `rg` | `rg -j4` | **greeg** |
|---|---:|---:|---:|---:|
| `createSourceFile` | 4.29 s | 2.42 s | 672 ms | **30 ms** |
| `node` (30,445 matches) | 4.52 s | 2.33 s | 882 ms | **43 ms** |

**Right more often.** Asked "where is this symbol defined?", greeg's first
result is correct far more often, scored against SCIP ground truth from
`rust-analyzer`, `scip-python` and `scip-typescript`:

| corpus | **greeg** | `rg -nw` | `grep` |
|---|---:|---:|---:|
| TypeScript 5.9 | **96 %** | 28 % | 47 % |
| django | **100 %** | 24 % | 29 % |
| tokio | **96 %** | 17 % | 19 % |

**Fewer tokens.** The broader the search, the bigger the saving: 28× on the
search above. A narrow one-file search costs about the same as grep, a few
tokens more for the path and symbol context. You can
[measure it on your own work](#is-it-actually-helping).

Protocols, raw data and caveats: [`references/BENCH.md`](references/BENCH.md).

## Install

macOS (arm64, x86_64) or Linux (x86_64, aarch64). ripgrep is not required.

```bash
brew install thiagodmont/greeg/greeg
```

<details>
<summary>Other ways to install</summary>

**Release archive** (binary and man page; checksums and a provenance
attestation alongside, see [verifying an archive](references/USAGE.md#verifying-a-release-archive)):

```bash
v=0.11.0; t=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
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
greeg hook claude       # Claude Code   (--dry-run to preview, --uninstall to remove)
greeg hook codex        # Codex 0.131+  (then run /hooks in Codex to trust it)
```

This installs a hook and a skill file. The agent keeps writing `rg` or
`grep -r`; the hook sends those searches to greeg (also inside `;`/`&&`
lists), and the skill teaches the agent greeg's symbol commands. **You don't
change how you prompt.** A rewritten `grep -r` picks files as rg does: it
skips ignored, hidden and binary files, which grep searches, and says so on
stderr. Commands the hook can't translate exactly (most pipelines,
redirections, grep without `-r`, …) run with the original tool;
`greeg hook explain 'grep -rn foo src'` shows what it would do.

> If you use `Bash(rg:*)` allow rules, add `Bash(greeg:*)` next to them. If
> another hook also rewrites `rg` (RTK, for example), only one can win; for
> RTK, add `exclude_commands = ["grep", "rg"]` under `[hooks]` in its config.

Any other agent can call `greeg` directly: it prints to stdout and exits `0` on
exact hits, `1` when there are none (it may still print relaxed, discovery-only
results), `2` on error. When the reader of its output leaves (`| head`), it ends
quietly by SIGPIPE, as grep does (shell status 141).

## Use it

Search like ripgrep:

```bash
greeg get_queryset                  # ranked hits, definitions first
greeg 'fn poll_read' -t rs -C 3     # ripgrep flags behave as in rg
greeg -w respond -l                 # files only, pipe-safe
greeg respond --budget 0            # every match, in path order
greeg createSourceFile --json       # typed JSON Lines for tools
```

Or ask about symbols, which grep can't:

```bash
greeg def JoinHandle                # where it's defined, with signature and doc
greeg refs Semaphore                # references grouped by kind
greeg callers spawn_blocking        # the functions that call it
greeg impls Future                  # implementations and subclasses
greeg impact get_queryset           # files likely affected by a change
greeg show src/sync/oneshot.rs:340  # the whole definition around a line
greeg outline src/sync/oneshot.rs   # the shape of a file
greeg map tokio/src/sync            # the most important files, by import graph
```

Ranked searches and symbol commands fit 2,000 tokens by default. Change it per
call (`--budget 5000`, `--budget 0` for everything) or once (`greeg budget
high`); `-l`, `-c` and `--json=rg` are never budgeted. The
[usage guide](references/USAGE.md) covers every flag, the JSON formats and
matching rules.

## How it works

- **No setup.** The first query scans like ripgrep while an index is built in
  the background. The index lives in your cache directory, never in the repo.
- **Always fresh.** Each query checks for edits (a `stat` pass, or FSEvents on
  macOS) and reads changed files directly, so answers include your latest edits.
- **Understands code.** Every hit is classified from syntax (definition, call,
  import, type, comment, …) and tagged with the symbol it sits in. Python,
  TypeScript/JavaScript, Rust and Kotlin get full syntax support; other
  languages get regex-based definitions, and you can add a tree-sitter grammar.
- **Ranks, then budgets.** Definitions outrank calls, source outranks tests,
  central files outrank leaves. Tests and generated files are demoted, never
  hidden, and the footer always says what was cut.
- **Fails safe.** A damaged or outdated index is rebuilt in the background,
  and the query is answered by a scan instead. If `--budget 0` has already
  streamed part of its output, greeg stops with an error instead of a partial
  answer that looks complete.

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
savings vs rg/grep · 145 replayed queries · same machine and tree
                              rg/grep        greeg        saved
  tokens, total                86,258       66,310       19,948  23%
  tokens, typical query           311          318          -30
  time, total                  4.64 s       866 ms       3.77 s  81%
```

Most savings come from a few broad searches; a typical one-file search costs
about the same. More in [`references/STATS.md`](references/STATS.md).

## Documentation

- [Usage guide](references/USAGE.md): every flag, budgets, JSON, the index, sessions, the hook
- [JSON output](references/JSON.md): `--json`, `--json=rg`, `--capabilities`, `--explain`
- [Architecture](references/ARCHITECTURE.md): how the index, ranking and freshness work
- [Benchmarks](references/BENCH.md): every measurement, with protocol and raw data
- [Statistics](references/STATS.md): measuring greeg on your own traffic
- [Development](references/DEVELOPMENT.md): building, testing and benchmarking

Issues and pull requests are welcome. For a wrong or missing result, include
the output of `greeg <query> --stats` and `greeg doctor`.

## License

MIT. See [`LICENSE-MIT`](LICENSE-MIT).
