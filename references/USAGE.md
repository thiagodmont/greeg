# Using greeg

The [README](../README.md) is the overview. This guide holds the details: how
matching, budgets and JSON work, every flag, where greeg keeps its data, and
how the agent hook behaves.

## Searching

Common ripgrep flags work alongside ranked output:

```bash
greeg get_queryset                  # ranked hits, definitions first
greeg -w respond -l                 # files only, rg-shaped, pipe-safe
greeg 'fn poll_read' -t rs -C 3     # ripgrep flags behave as they do in rg
greeg createSourceFile --json       # typed JSON Lines for tools and agents
greeg createSourceFile --json=rg    # ripgrep JSON Lines only: every match
greeg respond --budget 0            # unlimited exact matches, path order
```

### Exact and relaxed matching

File searches with `-l`, `-c`, `--mode files|count`, `--budget 0`, or `--json`
default to **exact matching**: a miss never becomes a case, word-boundary, or
fuzzy-name match. Ranked text keeps the discovery ladder. Use
`--matching exact` to disable it there, or `--matching discover` to explicitly
request relaxed results in another format. `--no-ladder` remains a spelling
of `--matching exact`; use one or the other. Relaxed results still exit `1`
and name the rung: in the footer of a search, and in the header and the closing outcome line of a symbol verb. See [matching policies](ARCHITECTURE.md#when-nothing-matches).

### Symbol verbs

```bash
greeg def JoinHandle                # where it's defined: ranked, with signature and doc
greeg refs Semaphore                # references grouped by kind (call, type, import, …)
greeg callers spawn_blocking        # which functions call it (not which lines)
greeg impls Future                  # implementations and subclasses
greeg impact get_queryset           # files using it, by evidence: LIKELY AFFECTED / POSSIBLE / REVIEW
greeg outline src/sync/oneshot.rs   # the shape of a file
greeg show src/sync/oneshot.rs:340  # the whole definition enclosing that line
greeg map tokio/src/sync            # important files and directories, by PageRank
```

`greeg show FILE:LINE` and `greeg def NAME --mode block` both print a whole
definition, which beats guessing a `sed -n '340,380p'` range. Every verb takes
the search flags (`--budget`, `--json`, `--no-tests`, `--root`) and answers in a
few milliseconds from the index.

## Budgets

`--budget` sets one run's token budget. Without it, greeg uses `GREEG_BUDGET`,
then the level saved with `greeg budget`, then 2000:

```bash
greeg budget          # the budget in use and where it comes from
greeg budget low      # 1000; also medium (2000), high (5000), a number, or none
greeg budget none     # no limit: every search prints every match
```

`--max-bytes N` caps stdout at N bytes, on top of the budget (`0`, the default,
sets no cap): the answer is fitted to both limits, in whole lines and records,
and its footer says `cut by --max-bytes` (`truncated_by: "bytes"` in JSON). It applies to `-l`,
`-c`, `--budget 0` and stdin too, which keep their first results. When not even
the smallest answer fits, greeg writes nothing and exits `2`. `--json=rg` takes
no `--max-bytes`.

The level is saved as `budget = N` in `~/.config/greeg/config.toml`
(`GREEG_CONFIG_DIR` names another directory). `none`
suits a human at a terminal more than an agent: a search for a common word
can print hundreds of thousands of tokens. `--json=rg`, `-l` and `-c` are
never budgeted. A search for the word `budget` needs `greeg -e budget`, as
for any command name.

Tokens are estimated, not counted: greeg splits text the way OpenAI's
o200k_base tokenizer does for ASCII and weights each piece. Measured on 716
search and verb answers from seven corpora, the estimate is within 8 % of
the real count for 95 % of them; short answers with unusual words can be off
by up to 24 %. Non-ASCII characters count as punctuation, so text in scripts
such as Chinese or Japanese is undercounted.

## Flags and output formats

**ripgrep-compatible flags**: `-i -S -s -w -x -F -U -n -l -c -A -B -C -g -t -T
-e -j --no-ignore --hidden -u -uu --max-columns --max-filesize --json --sort path`.
Cosmetic flags (`-N -H --color --no-heading --column --trim`) are accepted and
ignored, because the output they ask for is the output greeg already gives.
`-a`/`--text` and `-uuu` exit `2` rather than quietly answering without the
binary files ripgrep would have searched. A NUL byte in a file's first 64 KiB
skips it as binary; a later one ends its search at that line, and the output
says where ([binary files](ARCHITECTURE.md#binary-files)).

`--json=rg` prints ripgrep's JSON Lines records and nothing else, for every
match: no budget, ripgrep's offsets (after a UTF-8 BOM) and exit status. It
takes no `--budget`, no `--max-bytes`, no command and no `-U` (exit `2`). `--json=greeg` is greeg's
own format for searches and verbs: a header naming the schema, typed records,
paths and content that never lose a byte, each file's encoding and offset
coordinates, and an `outcome` in every footer. For searches and verbs, bare
`--json` is `--json=greeg` since 0.11; commands with a JSON report of their
own (`stats`, `index --status`) keep it. `--json=legacy`, greeg's earlier records with
`kind`, `score`, `facets` and `footer`, is removed in 0.12 and says so on a
terminal. See
[`references/JSON.md`](JSON.md#migrating-from-bare---json).

**greeg flags**: `--budget LEVEL|N` (tokens: `low` 1000, `medium` 2000, `high`
5000, `none` or `0` = unlimited; default 2000 or the level `greeg budget` saved) ·
`--max-bytes N` (stdout ceiling) ·
`--capabilities` (what this build supports, one JSON record; [JSON.md](JSON.md#capabilities)) ·
`--explain` (what an answer was built from: index use and freshness; a search's candidates, filters and ranking terms; a symbol verb's counts, and `def`'s score terms; [JSON.md](JSON.md#explain)) ·
`--mode files|outline|content|block` · `--kind def,call,…` · `--near PATH` ·
`--no-tests --no-vendored --no-generated --all` · `--per-file N` · `--chain` ·
`--matching exact|discover` · `--no-ladder` · `--fresh auto|none|stat|fsevents` · `--no-index` ·
`--root PATH` · `--index-dir DIR` · `--stats` · `--precise` · `--session ID` · `--no-session` ·
`--no-persist` (write nothing to disk: no index build or refresh, session or
statistic; also `GREEG_NO_PERSIST=1`)

**Languages with full syntax support**: Python, TypeScript/TSX, JavaScript,
Rust, Kotlin. Everything else gets a regex-based definition extractor, so
search, ranking and budgeting still work. You just lose the precise symbol
kinds. You can add a language yourself by dropping a tree-sitter grammar and a
`tags.scm` into `~/.config/greeg/lang/<name>/`; `greeg lang check DIR` validates
it. See [`references/ARCHITECTURE.md`](ARCHITECTURE.md).

**When something goes wrong**: a panic in the index path, a corrupt component
or a truncated file all fall back to a scan-mode answer plus a background
rebuild. Every component carries a digest per block, checked as it is read,
so a changed byte is found before it can change an answer. A `--budget 0`
search streams its output, so a failed check after output has started can't
fall back: greeg stops with `the index failed a check after output started;
output is incomplete (the index will be rebuilt)` and exits `2`. Otherwise
you get a correct answer either way. Writers hold a lock and publish
atomically; readers never lock.

**When output can't be written**: an answer greeg could not deliver never
exits `0`. A write error exits `2` with the reason on stderr. A reader that
leaves (`greeg … | head`) ends the run by SIGPIPE without a message, as it
ends grep.

## The index

```bash
greeg index                 # build now instead of on first query
greeg index --status        # manifest: files, generation, deltas, build peak RSS
greeg --no-index PATTERN    # scan the tree like ripgrep, ignore the index
greeg doctor                # index location, freshness mode, languages, disk use
greeg purge                 # list every index, session and statistic greeg keeps
greeg purge --yes           # remove them (what is in use is kept)
```

The index lives outside your repo (under `~/Library/Caches/greeg` on macOS,
`$XDG_CACHE_HOME/greeg` elsewhere), so it never dirties the working tree and
survives `git clean`; an `--index-dir` inside the tree is left out of searches,
and one that is the tree itself is refused.
Each greeg version has its own `v<N>-<key>/` directory there, so two versions
used on one repository in turn never rebuild each other's index; another
version's index is removed once it has gone unused for 14 days. A new greeg
version, or a change to the extra languages, rebuilds once in the background;
so does an edit to a file import resolution reads (`tsconfig.json`,
`package.json`, `Cargo.toml`, ...), since it moves the edges of unchanged
files.

```bash
greeg index                 # build now instead of on first query
greeg index --status        # manifest: files, generation, deltas, build peak RSS
greeg --no-index PATTERN    # scan the tree like ripgrep, ignore the index
greeg purge                 # list every index, session and statistic greeg keeps
greeg purge --yes           # remove them (what is in use is kept)
```

## Session memory

Session memory is separate from opt-in statistics. Agent searches remember recently
shown files and context for up to 24 hours. `--no-session` disables those reads and
writes; it does not disable indexing or opt-in statistics.

Session logs use a private `0700` directory and `0600` files under the index
location. Each log is limited to 2,000 records and 4 MiB; individual records are
limited to 256 KiB. Expired records cannot influence ranking or context deduplication.
New records contain normalized query tokens and shown paths/ranges, but no raw
pattern field. These tokens and paths can still reveal sensitive activity.

Unsafe storage is refused. Session persistence is best effort: a busy lock or I/O
failure can skip a record while the search still succeeds. See the
[session storage contract](ARCHITECTURE.md#session-memory) for retention,
compatibility and concurrency limits.

## Agent hook details

A rewrite changes only the command: the call's timeout, background flag and
description are passed back unchanged. Codex applies hook rewrites from 0.131
on. `crates/greeg/tests/fixtures/hooks` holds each host's documented hook
payload; the hook tests check that each reply has the shape that host's
documentation accepts.

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
[configuration update contract](ARCHITECTURE.md#hook-configuration-updates)
for concurrency and interruption limits.

Generated skills carry a versioned ownership marker and content checksum.
Reinstall updates unmodified managed skills; uninstall removes only those skills
or an exact copy of the bundled legacy text. Edited or unrecognized skills are
preserved with a message; move one aside before reinstalling to regenerate it.
Skill-file symlinks, nonregular files and unreadable content stop the operation
before configuration changes. Configuration and skill updates are separate
operations; `--dry-run` previews both and writes neither.

The hook preserves supported search arguments and requests exact matching; text
results still use greeg's ranking and token budget. An explicit file-size flag
preserves ripgrep's requested limit (unlimited by default), so files over 4 MiB
are not silently omitted. It leaves grep variants,
JSON/stats output, explicit executable paths, pipelines, subshells,
comments, redirections and shell expansions with the original tool. In a
`;`/`&&`/`||` list, each eligible `rg` command is rewritten on its own. An inherited
`RIPGREP_CONFIG_PATH` also disables rewriting. `greeg hook explain 'rg -l foo src'`
shows the rewrite, or why a command is left alone. See the
[hook capability matrix](ARCHITECTURE.md#agent-hook-contract) for the
supported subset and validation limits.

## Statistics storage

The records live in a private cache directory (`0700`, files `0600`), stay under
about 33 MB per file kind, never leave the machine, and `greeg stats clear`
deletes them. Replay runs only a ripgrep found on an absolute `PATH` entry, or the
one you pass with `--rg /absolute/path`. See [`references/STATS.md`](STATS.md) for per-session
breakdowns and build-to-build comparisons.

## Verifying a release archive

Each archive is built and qualified on its own target's runner before
publication: its version, files and a semantic suite (`bench/qualify.py`).
The release log prints the minimum macOS each binary declares and the newest
glibc symbol version each Linux binary needs; systems older than the
runners (macOS 15, Ubuntu 24.04) are not verified. Archives from 0.11 on
carry a build provenance attestation; to check one, download the archive and
verify it with the GitHub CLI (2.49 or newer):

```bash
v=0.11.0; t=aarch64-apple-darwin
curl -sSLO https://github.com/thiagodmont/greeg/releases/download/v$v/greeg-$v-$t.tar.gz
gh attestation verify greeg-$v-$t.tar.gz --repo thiagodmont/greeg
```
