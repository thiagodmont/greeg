# How greeg works

greeg answers a search from a persistent per-repository index instead of
walking the tree. This document explains what that index holds, how a query
becomes an answer, and why the pieces are shaped the way they are.

The five crates: `greeg` (CLI, output), `greeg-query` (planning, verification,
ranking, budget), `greeg-index` (file table, postings, symbols, freshness,
build), `greeg-lang` (grammars, extraction, import resolvers),
`greeg-fsevents` (macOS change log).

## Why an index at all

Searching a large tree is not bound by matching. It is bound by opening files.
On a 66k-file checkout, ripgrep spends about 17 µs per file on
`open`/`read`/`close` and only 0.26 s of user time actually matching — and it
gets *worse* with more threads, because the kernel contends on its own locks.
Matching all of TypeScript takes milliseconds; reaching the bytes takes seconds.

So the index exists to answer one question cheaply: **which files could
possibly match?** Everything else follows from that.

## What is on disk

The index lives outside the repository, under `~/Library/Caches/greeg/<hash of
realpath>` on macOS or `$XDG_CACHE_HOME/greeg/…` elsewhere. It never dirties
the working tree and survives `git clean`. `GREEG_INDEX_DIR` or `--index-dir`
override it.

Every component is a flat, fixed-width table read through `mmap` with no
deserialization step. A `manifest` (JSON) names the current generation of each.

| File | Holds | Used for |
|---|---|---|
| `files.bin` | path, size, mtime, language, flags, line count, rank | the file set, filters, ranking |
| `grams.bin` | trigram → roaring bitmap of file ids | candidates for any pattern |
| `words.bin` | whole word → roaring bitmap of file ids | candidates for identifier queries |
| `symbols.bin` | every definition: name, kind, file, span, parent, supertypes | `def`, `impls`, enclosing-symbol lookup |
| `spans.bin` | per file: definition, comment/string, and import ranges | classifying a hit in O(log n) |
| `graph.bin` | file import graph (both directions) + PageRank | `map`, ranking, reachability |
| `delta/NNNN.bin` | the same layouts, for recently changed files | edits, without a rebuild |

Two deliberate choices here.

**Postings are per file, not per position.** A positional index (which byte
offsets hold this trigram) costs 2–3.5× the corpus. greeg has to open a
candidate file anyway to print the line, its enclosing symbol and its context,
so knowing *where* in the file buys almost nothing. File-level postings with
roaring compression land at 0.15–0.6× of source and intersect at memory speed.

**Words sit next to trigrams.** Trigrams cannot tell `createSourceFile` from
`createSourceFileWithText`: on a 74k-file tree that query opened 486 files to
find 30 with a real match. One posting list per distinct word cuts that to 31,
and `-w node` from 6,086 files to 1,879. The word section costs 0.4–0.6× the
gram section and answers exactly the question agents ask most.

## Building the index

The first query in a new repository is answered by a ripgrep-speed scan while
the build runs detached behind it. There is no setup step and no daemon.

The build runs in two phases so queries get useful answers early:

**Phase 1** walks the tree (honouring `.gitignore` exactly as ripgrep does),
reads each file on a small pool — 4 threads on macOS, where reading degrades
badly past that, all cores on Linux — classifies it, and extracts its trigrams
and words. Postings accumulate against a byte budget and spill sorted segments
to disk when it is reached, so peak memory stops tracking the size of the tree;
a k-way merge streams them back out at publish time. Small repositories never
reach the budget and never spill.

**Phase 2** re-reads the files that have a grammar (now warm in the page
cache), parses them with tree-sitter on all cores, extracts definitions,
comment/string spans and imports, resolves the imports into graph edges, runs
PageRank, and republishes the file table with ranks.

Between the two phases, queries already prune candidates with the grams and
fall back to a regex definition extractor for kinds.

Publication is a single atomic `rename` per component followed by the manifest.
Writers take an exclusive lock; **readers never lock**, and a reader that has
already mapped an old generation keeps a valid view until it exits.

## Answering a query

1. **Plan.** The pattern is parsed into a regex HIR, and the required grams are
   computed over it (Cox's algorithm): concatenation crosses literal sets,
   alternation unions, classes expand while they are small. Case folding
   happens at index time, so `-i` is free. A whole-word query skips the grams
   and reads word postings instead. A pattern with no usable literal — say
   `\w{5}\s+\w{5}` — plans a full scan over the file table, which is still
   cheaper than a walk.
2. **Select candidates.** Roaring bitmap intersections in ascending
   document-count order, with early exit on empty. Path filters become file-id
   ranges (ids are assigned in sorted-path order, so `src/**` is one range),
   type filters and flag filters are precomputed bitmaps. Tombstones are
   subtracted, delta postings unioned.
3. **Verify.** Candidates are read in *prior order* — best files first — by a
   pool of reader threads, and matched with the real regex engine. Ordering by
   prior means the budget can stop early and still have the best hits.
4. **Classify.** Each hit gets a kind from binary searches over that file's span
   tables: inside a definition's name → `def`; inside a comment or string →
   `comment`/`string`; inside an import → `import`. Otherwise a per-language
   byte rule reads the characters around the match: `(` after → `call`, `.`
   before → `member`, `: ` or `-> ` or `impl ` before → `type`, else `ident`.
   This is O(1) per hit and right for the overwhelming majority of identifier
   hits. `--precise` re-parses the shown files to replace the rule with real
   node kinds.
5. **Rank.** `kind × location × importance`. Definitions outrank calls outrank
   types outrank comments. Source files outrank tests, mocks, vendored and
   generated code — those are **demoted, never hidden**, and the footer always
   says how many were pushed down. Important files (import-graph PageRank)
   outrank leaves, and a definition whose name equals the query exactly gets a
   further boost.
6. **Shape to a budget.** If everything fits, print it. If not, print *facets*:
   counts by kind, by directory, by language, the top definitions, and the top
   hits — because the first thing an agent needs from a 357-hit query is the
   shape of the answer, not its first 50 lines.

`--budget 0`, `-l` and `-c` bypass shaping entirely and reproduce ripgrep's
match set exactly, because the agent hook rewrites pipelines like
`rg -l … | xargs …` and anything but bare paths on stdout would break them.

### When nothing matches

Rather than returning nothing, greeg climbs a ladder and reports the rung it
used: drop `-w`, then case-insensitive, then split the query on
camelCase/snake_case boundaries and look for names made of those tokens, then
a bounded fuzzy search over the symbol names, then report hits that exist only
in ignored or hidden files. `SpawnBlocking` finds `spawn_blocking` this way.
`--no-ladder` turns it off.

## Staying fresh

The index must reflect the working tree at query time, with no daemon.

* **macOS, large trees**: the manifest stores an FSEvents stream id; the query
  asks the kernel's persistent log which directories changed since then and
  rescans only those. About 12 ms for a 200-file change set.
* **Everywhere else**: a parallel `lstat` of every known file comparing size,
  mtime and inode, plus directory mtimes to catch additions and deletions.
  41 ms for 66k files. Below about 8,000 files this beats FSEvents, whose
  stream setup costs ~11 ms regardless of size.
* **Back-to-back calls**: a manifest verified within the last 100 ms is
  trusted, so several tool calls in one agent turn pay for one check.

A search that finds changes **answers first**: it drops the stale versions from
its candidates, reads the changed files directly, prints the answer, and only
then spawns a detached process to extract them and publish a delta segment. The
answer is still the working tree at query time; what left the critical path is
the tree-sitter work. Past a threshold — an edited `.gitignore`, more than
2,000 changed files, 16 accumulated deltas, or 5 % of the tree — a full rebuild
is spawned instead and the query is answered by a scan.

Symbol verbs are the exception: `def`, `refs` and `outline` need the changed
files' symbols, so they apply the delta inline before answering.

## Symbols and the import graph

Definitions come from tree-sitter queries written for greeg — modelled on
Sourcegraph's tag queries rather than the upstream `tags.scm` files, which are
too coarse to tell a method from a field. A file that fails to parse, or whose
language has no grammar, falls back to anchored regexes per language, so
`def` and `refs` still work everywhere; you lose only the precise kinds.

Imports are resolved to file ids at index time, per language: Python package
paths and relative dots, TypeScript/JavaScript through the nearest
`tsconfig.json` `paths` and `baseUrl` plus workspace packages, Rust `use` and
`mod` declarations against the crate layout, Kotlin through the package table.
Resolved imports become weighted graph edges, and PageRank over that graph is
what makes a hub file outrank a leaf in every ranked answer.

The verbs read this directly. `refs` is a word-bounded search grouped by kind.
`callers` is `refs` filtered to calls and keyed by the *enclosing function*, so
the answer is a list of functions rather than a list of lines. `impls` reads
the supertype lists. `impact` splits references into WILL BREAK (a call or type
in non-test source), MAY BREAK (member or bare identifier) and REVIEW (comment,
string, test).

Cross-file type resolution is heuristic, not a compiler. Where it matters the
output carries a confidence, and `--precise` tightens classification on the
files actually shown.

## What the output means

Ranked text is grouped by file, with the path printed once:

```
django/db/models/manager.py
  150 def   def get_queryset(self):        ‹ BaseManager
  212 call  return self.get_queryset()     ‹ BaseManager.all
```

The column after the line number is the hit kind; `‹` names the enclosing
symbol. A row that *is* a definition names its parent, never itself.

The footer is a contract, and the tests in `crates/greeg/tests/cli.rs` assert
it:

* A complete answer — every hit shown, nothing skipped, matched as asked — ends
  with just `N hits · M files`. `no hits` is bare. The counts say the output is
  whole, and nothing else would change what you do next.
* Anything else is accounted for: `shown/total hits · shown/total files`,
  `N files demoted (hits)`, `skipped N binary` (only when non-zero), `matched
  <rung>` when the ladder relaxed the query, and `~N tokens`.
* `next:` suggests flags only, never a different pattern.

A bare identifier is answered as a whole word. Matches inside *longer*
identifiers stop competing for answer lines and collapse to one `related` line
naming them with their file counts — unless nothing matches the whole word, in
which case the near-misses are the answer.

`--json` preserves ripgrep's JSON Lines schema exactly, adding `kind`,
`symbol`, `file_flags` and `score` to match records plus `facets` and `footer`
record types.

## Languages

Python, TypeScript/TSX, JavaScript, Rust and Kotlin are compiled in with
tree-sitter grammars. Every other language gets the regex extractor, the
byte-level comment/string lexer, and full search, ranking and budgeting — it
loses precise symbol kinds and import resolution.

You can add one without rebuilding greeg: drop `spec.toml`, a compiled
`grammar.so`/`.dylib` and a `tags.scm` using greeg's capture names into
`~/.config/greeg/lang/<name>/`. `greeg lang check DIR` validates coverage
against your fixtures.

## When things go wrong

The index is a cache, and every failure path ends in a correct answer.

A panic anywhere in the index path is caught and degrades to a scan plus a
background rebuild. A corrupt posting list is read as "every file" — a superset
is harmless because verification runs the real matcher anyway — and marks the
index for rebuild. A truncated component fails to open and triggers a rebuild.
A `SIGBUS` from a file truncated under an active mmap re-executes the same
command with `--no-index`. A format-version mismatch rebuilds; there is no
migration code, by design.

## Design decisions

| Decision | Chosen | Instead of | Why |
|---|---|---|---|
| Process model | short-lived process per query | a resident server | warm queries are 3–10 ms; a daemon is a large amount of code to keep an index and a filesystem in step, and buys nothing at that latency |
| Posting granularity | one bitmap per file | positional postings | the file has to be opened anyway to print; 3× smaller |
| Identifier queries | whole-word postings beside the trigrams | sparse or variable-length grams | measured: every sparse variant was larger *and* slower to build; word postings cut candidate reads 15× on the queries agents actually ask |
| Call/type kinds | byte rules at query time | stored spans for every call | spans for every call site would roughly double the index; the rules are O(1) and `--precise` exists for when they are not enough |
| Index location | user cache directory | `.greeg/` in the repository | never dirties the tree, survives `git clean`, no `.gitignore` edits |
| Freshness | stat pass, or the OS change log | a mandatory watcher process | zero setup is the point; the measured cost is tens of milliseconds |
| Post-edit search | answer first, publish the update after | apply the update, then answer | the inline update cost 28–150 ms on every search after an edit |
| Delta durability | no fsync on delta segments | fsync every publish | `F_FULLFSYNC` was 4–5 ms of the 28 ms an edit cost the next query, and the index is a cache: a torn delta simply rebuilds |
| Demoted files | ranked down, always counted | filtered out | an agent that cannot see test code cannot reason about test failures; the footer says what was pushed down and how to get it back |
| Cross-file resolution | import-graph heuristics | stack-graphs | archived upstream, no Kotlin or Rust support |
| Reader threads on macOS | 4 | all cores | measured 3× faster on 66k small files; kernel lock contention dominates |
| Serialization | hand-laid fixed-width tables over mmap | a serialization framework | zero-copy with no framework churn or format instability for what are four flat tables |
| Unsupported flags | exit 2 | accept and ignore | `-a` asks for binary files the index does not hold; answering without them is a wrong answer, not a cosmetic difference |
| Type filters | ripgrep's own type definitions | greeg's language table | parity means rg's rules, including its glob precedence |
| Extra languages | `dlopen` a user-compiled parser | WASM grammars or a plugin format | no new dependencies; the tree-sitter C ABI is stable |

## Measuring it

`bench/bench.py` drives the benchmark protocol: pinned corpora, a hyperfine
harness against `grep`, `rg` and `rg -j4`, an accuracy oracle scored against
SCIP indexes (`rust-analyzer`, `scip-python`, `scip-typescript`), and
regression gates keyed by host so a laptop records but only the reference
machine binds. `bench/parity.py` verifies that greeg's match set equals
ripgrep's across every corpus and query family, in both indexed and scan mode.
`bench/soak.py` runs randomized queries against randomized edits.

Results and method: [`BENCH.md`](BENCH.md). Measuring greeg against your own
agent traffic: [`STATS.md`](STATS.md).
