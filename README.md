# greeg

**A grep for coding agents.** Familiar ripgrep flags. It answers from a
persistent index, knows which hits are *definitions*, ranks them, and fits the
answer in a token budget instead of dumping every match.

[![CI](https://github.com/thiagodmont/greeg/actions/workflows/ci.yml/badge.svg)](https://github.com/thiagodmont/greeg/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/thiagodmont/greeg)](https://github.com/thiagodmont/greeg/releases)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

---

## The problem

Your agent runs `rg isIdentifier` on the TypeScript compiler. ripgrep does its
job perfectly: **774 lines, 88,147 bytes**.

Claude Code truncates tool output at 30,000 characters. So your agent reads a
third of that wall, and the definitions it was actually after sit somewhere
past the cut.

greeg answers the same question in **2,553 bytes**, and opens with them:

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
14/767 hits · 8/105 files · 4 files demoted (17) · skipped 2 huge · ~745 tokens
```

Definitions first, each with the type it belongs to. Tests demoted, still
counted.

Where the `…` is, greeg prints the shape of the other 753 hits: a kind
breakdown, the directories they live in, the top call sites with the function
each one sits in, 122 import lines collapsed to a single line, and the
near-miss names (`isIdentifierText`, `isIdentifierPart`) listed off to the side
instead of competing for space. The footer says what got left out and which
flag brings it back.

Every hit is still accounted for. The answer is **ranked**.

## Why it's worth installing

**It's faster.** The index means a query opens only the files that could match,
instead of every file in the tree. On TypeScript-5.9 (74k files, 368 MB):

| query | `grep` | `rg` | `rg -j4` | **greeg** |
|---|---:|---:|---:|---:|
| `createSourceFile` | 4.29 s | 2.42 s | 672 ms | **30 ms** |
| `node` (30,445 matches) | 4.52 s | 2.33 s | 882 ms | **43 ms** |

**It's right more often.** Scored against SCIP ground truth from
`rust-analyzer`, `scip-python` and `scip-typescript`. The question being asked:
"I wanted to know where this symbol is defined. Was the first result correct?"

| corpus | **greeg** | `rg -nw` | `grep` |
|---|---:|---:|---:|
| TypeScript-5.9 | **96 %** | 28 % | 47 % |
| django | **100 %** | 24 % | 29 % |
| tokio | **96 %** | 17 % | 19 % |

**It costs fewer tokens**, and the broader the search, the bigger the gap.
Eight identifier searches across the TypeScript compiler, `rg` output against
greeg's answer: 4×, 5×, 8×, 8×, 20×, 21×, 36×, 44× smaller.

Narrow searches land closer to even. A single-file grep actually costs you a
few tokens *more*, because greeg adds the path header, the kind column and the
enclosing symbol. You can
[measure it on your own traffic](#is-it-actually-helping) instead of taking my
word for it.

Full numbers, protocol and caveats: [`references/BENCH.md`](references/BENCH.md).

## Install

macOS (arm64, x86_64) or Linux (x86_64, aarch64). You don't need ripgrep
installed.

```bash
brew install thiagodmont/greeg/greeg
```

<details>
<summary>Other ways to install</summary>

**Release tarball** (binary + man page, sha256 alongside):

```bash
v=0.6.0; t=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
curl -sSL https://github.com/thiagodmont/greeg/releases/download/v$v/greeg-$v-$t.tar.gz | tar xz
sudo install greeg-$v-$t/greeg /usr/local/bin/
```

**From source** (Rust 1.90 or newer):

```bash
cargo install --git https://github.com/thiagodmont/greeg greeg
```

</details>

Check it:

```bash
greeg --version
greeg doctor            # index location, freshness mode, languages, disk use
```

## Point your agent at it

One command installs a hook that rewrites supported simple `rg` calls to
`greeg --matching exact`, plus a skill file so the agent knows the extra verbs.
**You don't have to change how you prompt.** The agent keeps writing `rg`, and
greeg answers eligible calls.

```bash
greeg hook claude       # Claude Code   (--dry-run to preview, --uninstall to remove)
greeg hook codex        # Codex         (then run /hooks in Codex to trust it)
```

Install/uninstall recognizes exact command handlers: `greeg hook run` (also
`--agent claude`) for Claude, and `greeg hook run --agent codex` for Codex.
Uninstall removes those handlers while preserving other handlers and entry
metadata. Prefix lookalikes, wrappers and custom commands are left alone.
Newly empty entries are removed only when they have no custom metadata and
their matcher is absent or exactly `Bash`; pre-existing empty entries stay intact.
New registrations append without moving existing entries; no-op configuration
edits preserve the original text. Invalid or unreadable settings stop the edit.
Comments on retained TOML entries and stored trust data are preserved, but
removing handlers can change positional hook IDs; review `/hooks` after changing the configuration.

Configuration changes use a synced temporary file and atomic publication. A
nonblocking lock serializes greeg writers; changed content or file identity
causes a conflict instead of overwriting a detected external edit. Config-file
symlinks, hard links and files owned by another user cannot be replaced.
Existing permissions are retained; new configurations use mode `0600`.
On macOS, metadata is copied; on Linux, configurations with extended attributes
or ACLs are left unchanged with an error. See the
[configuration update contract](references/ARCHITECTURE.md#hook-configuration-updates)
for concurrency and interruption limits.

Preservation of user-edited generated skills remains a follow-up. Configuration
and skill updates are separate operations; `--dry-run` writes neither.

The hook preserves supported search arguments and requests exact matching; text
results still use greeg's ranking and token budget. An explicit file-size flag
preserves ripgrep's requested limit (unlimited by default), so files over 4 MiB
are not silently omitted. It leaves grep variants,
JSON/stats output, explicit executable paths, pipelines, compound commands,
comments, redirections and shell expansions with the original tool. An inherited
`RIPGREP_CONFIG_PATH` also disables rewriting. See the
[hook capability matrix](references/ARCHITECTURE.md#agent-hook-contract) for the
supported subset and validation limits.

> **If you use `Bash(rg:*)` allow rules**, add `Bash(greeg:*)` next to them.
>
> **If you run another hook that rewrites `rg`/`grep`** (RTK, for example),
> only one can win: hooks run in parallel, and the last one to finish decides.
> For RTK: add `exclude_commands = ["grep", "rg"]` under `[hooks]` in its config.

Any other agent can just call `greeg` directly: it prints to stdout and exits
`0` on exact hits, `1` on none (including discovery-only results), `2` on error.

## Using it

Common ripgrep flags work alongside ranked output:

```bash
greeg get_queryset                  # ranked hits, definitions first, ~600 tokens
greeg -w respond -l                 # files only, rg-shaped, pipe-safe
greeg 'fn poll_read' -t rs -C 3     # ripgrep flags behave as they do in rg
greeg createSourceFile --json       # ripgrep JSON Lines + kind/symbol/facets/footer
greeg respond --budget 0            # unlimited exact matches, path order
```

File searches with `-l`, `-c`, `--mode files|count`, `--budget 0`, or `--json`
default to **exact matching**: a miss never becomes a case, word-boundary, or
fuzzy-name match. Ranked text keeps the discovery ladder. Use
`--matching exact` to disable it there, or `--matching discover` to explicitly
request relaxed results in another format. `--no-ladder` remains a spelling
of `--matching exact`; use one or the other. Relaxed results still exit `1`
and name the rung in the footer. See [matching policies](references/ARCHITECTURE.md#when-nothing-matches).

And then the part grep can't do. Asking about *symbols* instead of *text*:

```bash
greeg def JoinHandle                # where it's defined: ranked, with signature and doc
greeg refs Semaphore                # references grouped by kind (call, type, import, …)
greeg callers spawn_blocking        # which functions call it (not which lines)
greeg impls Future                  # implementations and subclasses
greeg impact get_queryset           # what breaks if I change this: WILL / MAY BREAK / REVIEW
greeg outline src/sync/oneshot.rs   # the shape of a file
greeg show src/sync/oneshot.rs:340  # the whole definition enclosing that line
greeg map tokio/src/sync            # important files and directories, by PageRank
```

Two of those are worth committing to memory. `greeg show FILE:LINE` and
`greeg def NAME --mode block` both print a whole definition, which beats
guessing a `sed -n '340,380p'` range and reading the wrong 40 lines.

Every verb takes the search flags (`--budget`, `--json`, `--no-tests`, `--root`)
and answers in a few milliseconds from the index.

## How it works

1. **First query** in a repo is answered by a ripgrep-speed scan, while a
   trigram and word index is built in the background. No setup step, no daemon.
2. **Later queries** open only candidate files. A whole-word query opens exactly
   the files that hold that word: on a 74k-file tree, 31 files instead of 486.
3. **Edits are picked up per query** by a cheap `stat` pass, or the FSEvents log
   on macOS. A search after an edit reads the changed files directly and
   publishes the index update afterwards, so you never wait for it.
4. **Every hit is classified** from syntax computed at index time (`def`,
   `call`, `import`, `type`, `member`, `ident`, `doc`, `comment`, `string`),
   and carries the symbol it sits inside.
5. **Ranking and budget**: definitions outrank calls, source outranks tests,
   important files (import-graph PageRank) outrank leaves. The answer is cut to
   a token budget (2,000 by default) and the footer always says what was cut.

Test, vendored, generated, minified and mock files are **demoted, never
hidden**. `--budget 0`, `-l` and `-c` disable query relaxation by default.

The index lives outside your repo (under `~/Library/Caches/greeg` on macOS,
`$XDG_CACHE_HOME/greeg` elsewhere), so it never dirties the working tree and
survives `git clean`.

```bash
greeg index                 # build now instead of on first query
greeg index --status        # manifest: files, generation, deltas, build peak RSS
greeg --no-index PATTERN    # scan the tree like ripgrep, ignore the index
```

How all of this is put together, what each index component holds, and why the
shape was chosen: [`references/ARCHITECTURE.md`](references/ARCHITECTURE.md).

## Is it actually helping?

Don't take my benchmarks on faith. greeg can measure itself against *your*
traffic, on *your* repositories.

It's off by default, because the records hold your search patterns and paths.
Everything stays on your machine. Nothing is collected, nothing is sent
anywhere.

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

Read both halves together. The totals are what the agent actually paid; the
typical query says how that total is distributed. **Agents mostly grep one file
at a time, and on those greeg is a few tokens *larger* than grep.** The savings
come from the repo-wide searches, where grep dumps everything and greeg budgets
it. So a handful of queries carry most of the win.

The records live under your cache directory, mode 0600, never leave the machine,
and `greeg stats clear` deletes them. See [`references/STATS.md`](references/STATS.md) for per-session
breakdowns and build-to-build comparisons.

## Reference

**ripgrep-compatible flags**: `-i -S -s -w -x -F -U -n -l -c -A -B -C -g -t -T
-e -j --no-ignore --hidden -u -uu --max-columns --max-filesize --json --sort path`.
Cosmetic flags (`-N -H --color --no-heading --column --trim`) are accepted and
ignored, because the output they ask for is the output greeg already gives.
`-a`/`--text` and `-uuu` exit `2` rather than quietly answering without the
binary files ripgrep would have searched.

**greeg flags**: `--budget N` (tokens, default 2000, `0` = unlimited) ·
`--mode files|outline|content|block` · `--kind def,call,…` · `--near PATH` ·
`--no-tests --no-vendored --no-generated --all` · `--per-file N` · `--chain` ·
`--matching exact|discover` · `--no-ladder` · `--fresh auto|none|stat|fsevents` · `--no-index` ·
`--index-dir DIR` · `--stats` · `--precise` · `--session ID` · `--no-session`

**Languages with full syntax support**: Python, TypeScript/TSX, JavaScript,
Rust, Kotlin. Everything else gets a regex-based definition extractor, so
search, ranking and budgeting still work. You just lose the precise symbol
kinds. You can add a language yourself by dropping a tree-sitter grammar and a
`tags.scm` into `~/.config/greeg/lang/<name>/`; `greeg lang check DIR` validates
it. See [`references/ARCHITECTURE.md`](references/ARCHITECTURE.md).

**When something goes wrong**: a panic in the index path, a corrupt component
or a truncated file all fall back to a scan-mode answer plus a background
rebuild. You get a correct answer either way. Writers hold a lock and publish
atomically; readers never lock.

## Development

```bash
cargo build --release
cargo test --workspace                                   # unit + CLI contract tests, no network
cargo clippy --all-targets -- -D warnings

python3 bench/bench.py fetch tokio ktor django           # pinned corpora
python3 bench/parity.py "$GREEG_BENCH_CACHE" target/release/greeg   # rg parity
python3 bench/bench.py speed --corpora tokio,ktor,django # hyperfine protocol
python3 bench/bench.py oracle tokio django TypeScript-5.9 # SCIP accuracy oracle
```

CI runs build, tests, clippy, a binary-size gate, the rg parity suite and the
small-corpus speed protocol. The nightly workflow adds the large-corpus
benchmarks, the accuracy oracle and a randomized soak
(`bench/soak.py MINUTES GREEG CORPUS… --seed SEED`).

`bench/edits.py CORPUS GREEG` and the soak harness copy each source into a
private temporary snapshot and working directory. Dirty, untracked and ignored
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

## License

MIT or Apache-2.0, at your option.
