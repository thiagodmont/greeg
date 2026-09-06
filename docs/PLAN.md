# greeg — Implementation Plan

Status: v1.6, 2026-09-02. M0–M7 done (status blocks under each milestone). Companion to `DESIGN.md` (referenced as D§n).
Performance is the primary requirement; every milestone has a measured gate
and nothing merges that regresses a gate.

## 1. Scope of v1.0

In: indexed and scan-mode text search with ripgrep-compatible flags; hit
classification and enclosing symbols; ranking; token budget with facets and
honest footer; escalation ladder; adversarial-content handling; symbol verbs
(`def`, `refs`, `callers`, `impls`, `outline`, `map`, `impact`); session
memory; text and JSON output; Python, TypeScript/TSX, JavaScript, Rust,
Kotlin; a built-in benchmark suite against `rg` and `grep`; macOS and Linux
(arm64 and x86_64).

Later (v1.x): concept search feature (`tantivy`), SCIP import, structural
patterns via `ast-grep-core`, MCP server, learned ranker, Windows, Java/Go.

## 2. Repository layout

```
greeg/
  Cargo.toml                 workspace, [profile.release] lto = "fat", codegen-units = 1,
                             panic = "abort" is NOT used (we convert panics to fallback)
  crates/
    greeg/                   binary: clap CLI, command dispatch, output
    greeg-index/             file table, gram index, spans, symbols, graph, delta/tombstones,
                             build pipeline, freshness, blob cache
    greeg-query/             regex analysis -> gram plan, candidate evaluation, verification,
                             classification, ranking, budget shaping, ladder
    greeg-lang/              language specs, grammars, .scm queries, import resolvers,
                             regex fallback extractor, noncode lexer
    greeg-session/           session log, dedup, loop detection, focus set
    greeg-bench/             `greeg bench`: corpora pinning, hyperfine driver, oracle scoring
    greeg-fsevents/          macOS since-id FSEvents query (cfg(target_os = "macos"))
  lang/<name>/               tags.scm, locals.scm, noncode.scm, spec.toml, fixtures/
  bench/
    corpora.toml             repo, sha, adversarial flags
    queries/*.toml           query families per corpus
    baselines/<machine>.json stored results for regression gates
  docs/                      DESIGN.md, PLAN.md, FORMAT.md (on-disk formats), OUTPUT.md
  tests/                     integration tests over fixture repos
```

Rust 1.94 stable, edition 2024. `cargo deny` for licenses (no GPL, no
Hippocratic code copied). `cargo clippy -D warnings`. `just` recipes for
bench, fixtures and release builds. Release binaries via `cargo dist`
(macOS universal, Linux musl and gnu).

## 3. Milestones

Effort is engineer-weeks for one focused engineer; milestones M1–M3 are
sequential, M4 and M5 can overlap, M6 runs throughout.

### M0 · Spikes (1 week)

Purpose: retire the three biggest unknowns before committing to the layout.

| Spike | Question | Exit criterion |
|---|---|---|
| S1 gram size | Trigram file-level roaring postings: bytes per source byte on django, ktor, tokio, TypeScript, rust-lang/rust | ≤ 0.35× on all five, build ≥ 200 MB/s per core for extraction |
| S2 freshness | Rust parallel `lstat` + `readdir` diff on the 66k-file tree; FSEvents `sinceWhen` query latency and reliability after edits, renames, branch switch | stat pass < 60 ms; FSEvents < 5 ms and reports every changed directory in a 200-edit script |
| S3 Kotlin | `tree-sitter-kotlin-sg` 0.4.1 on ktor + kotlinx.coroutines + JetBrains/kotlin `compiler/testData` sample: parse time, ERROR-node rate, tag coverage of our draft `tags.scm` vs a JetBrains PSI dump for 100 files | ERROR files < 5 % on ktor; ≥ 90 % of top-level and member declarations tagged; documented gap list |

Deliverable: numbers in `docs/spikes.md`; go/no-go on file-level trigram v1
(expected go).

**Status: done 2026-09-01.** All three spikes went "go"; see `docs/spikes.md`.
Two gates were revised on evidence: index ratio applies above 50 MB of source
(0.15–0.23 × measured on the large corpora; small corpora sit at 0.37–0.41 ×
because of fixed per-gram overhead), and FSEvents latency is 12–14 ms rather
than 5 ms. Two design changes: directory change detection by directory mtime
instead of listing hashes, and a 150 ms FSEvents cutoff with stat fallback
(40 ms since v0.2, see DESIGN §15).
One Kotlin grammar gap (named context parameters) is documented and routed
to the regex fallback extractor.

### M1 · Scan mode with shaped output (3 weeks)

The tool is useful from this milestone on, without any index.

Tasks:
1. Workspace skeleton, CLI surface mirroring rg: `PATTERN [PATH...]`, `-i -w
   -F -x -n -l -c -A -B -C -g -t -T -j --json --no-ignore --hidden
   --max-columns`, plus `--budget --mode files|outline|content|block
   --near --no-tests --no-vendored --no-generated --all --fresh --session`.
   Unknown rg flags produce an error that names the nearest greeg flag.
2. Scan pipeline on `ignore` + `grep-searcher` with adaptive reader threads
   (D§5.4). Output parity test: for every query in `bench/queries`, the set of
   `(path, line)` from `greeg --mode content --budget 0` equals `rg -n`.
3. Language layer v0 (D§11): `spec.toml` for the five languages with
   extensions, comment/string lexer tables, definition regexes, test-path
   rules; adversarial flags (D§8) computed per file during the scan.
4. Classification v0 via the regex extractor and noncode lexer; enclosing
   symbol via nearest preceding definition at lower or equal indentation
   (Python, Kotlin, TS/JS, Rust braces counted by the lexer).
5. Ranking (D§6.3) without PageRank; per-file cap; budget shaping; adaptive
   context; facets mode; footer; `--json` extensions (D§10).
6. Token estimator calibrated against `o200k_base` on the corpora (< 10 %
   error on 1,000 sampled outputs).
7. Escalation ladder rungs 1, 2, 5 (rungs 3–4 need the symbol index).

Gate: on tokio, django, ktor and TypeScript the p50 of `greeg PATTERN` is
≤ 1.05 × the best `rg -j N` for the same pattern (N ∈ {1,4,cores}); output for
`get_queryset` on django fits in 2,000 tokens and lists all 13 definitions
first; `-w node` on TypeScript returns facets in < 1.5 s.

**Status: done 2026-09-01.** Crates `greeg-lang`, `greeg-query`, `greeg`
(scan mode, no index). Gate results on the reference machine, hyperfine
means, `rg -j4` being ripgrep's best thread count on this Mac:

| Query | rg -j4 | greeg | ratio |
|---|---|---|---|
| tokio `fn poll` | 16.9 ms | 15.4 ms | 0.91 |
| tokio `-w Waker` | 14.9 ms | 14.8 ms | 0.99 |
| django `get_queryset` | 107.2 ms | 106.3 ms | 0.99 |
| django `-w request` | 112.0 ms | 109.1 ms | 0.97 |
| ktor `respond` | 72.8 ms | 70.5 ms | 0.97 |
| ktor `fun respond` | 70.8 ms | 68.3 ms | 0.96 |
| TypeScript `createSourceFile` (66k files) | 957 ms | 1,006 ms | 1.05 |
| TypeScript `-w node` (facets) | 872 ms | 918 ms | 1.05 |
| rust-lang/rust `-w HirId` | 860 ms | 871 ms | 1.01 |

Parity: `(path, line)` sets identical to `rg --json` on 21 queries across the
five corpora (`bench/parity.py`). django `get_queryset` renders in ~1,800
tokens with all 18 non-test definitions first and the 54 test definitions
after them. Token estimate vs `o200k_base`: median 1.02, range 0.82–1.23
(`bench/tokens.py`). Ladder rungs 1, 2 and 5 implemented.

What it took to reach the speed gate (all recorded in DESIGN.md §5.4): search
inside the walker threads instead of walk-then-search; stat, flag and
classify only matched files; classify per hit line during the scan and fully
outline only shown files; ASCII-mode definition regexes (the Unicode lazy DFA
cost 14 µs per new line); per-line regex after a keyword prefilter instead of
`captures_iter` over the buffer; no rayon pool on the query path; searcher
BOM sniffing off so offsets index the buffer we read (files with a UTF-8 BOM
previously shifted every offset by 3 and panicked on classification).

Deviations: multi-line block comments and docstrings are classified only for
shown files (scan-time kinds are line-local); UTF-16 files are searched as
raw bytes rather than transcoded; `-A/-B/-C` context and adaptive context
share one code path and adaptive context is disabled when explicit context
is requested.

### M2 · Index phase 1: file table, grams, freshness (3 weeks)

Tasks:
1. On-disk formats (D§2.3, D§3.1, D§3.2) with `rkyv` archives and headers;
   `docs/FORMAT.md`; format-version check and rebuild.
2. Build pipeline stage A (D§4.1): walker, two pools, bounded channel, gram
   extraction with the 16 M-bit dedup bitset, per-thread maps, merge, roaring
   serialization, atomic publish; bucketed mode above the pair limit (D§4.2).
3. `greeg index` command; auto-spawn detached build on first query;
   phase-complete flags in the manifest; progress on stderr with `--wait`.
4. Query planner (D§5.2): Cox analysis over `regex-syntax` HIR with the
   documented limits; path/type/flag filters as bitmaps; tombstones and
   deltas.
5. Verification pass (D§6.1) with prior ordering and count-beyond-N.
6. Freshness (D§4.5): stat mode, TTL, inline re-extraction of ≤ 64 files,
   grams-only inline up to 2,000, spawn beyond; `greeg-fsevents` since-id
   mode on macOS; git-index oid shortcut via `gix`.
7. Delta segments and tombstones (D§4.3); compaction command and trigger.
8. Blob cache (D§4.4) with LRU bound.

Gate (reference machine, 66k-file TypeScript tree): `greeg createSourceFile`
p50 < 15 ms with `--fresh=none`, < 60 ms with `--fresh=stat`, < 30 ms with
FSEvents; index build phase 1 < 3 s; index ≤ 0.35 × source (≤ 15 MB absolute
on corpora under 50 MB); `greeg -w node` facets < 250 ms; correctness: `(path, line)` sets identical to `rg -n` for all
query families on all corpora, including after a scripted 200-file edit burst
and a branch switch, with no restart.

**Status: done 2026-09-01.** Crates `greeg-index` (format v1: file table,
trigram postings, planner, delta segments, freshness) and `greeg-fsevents`.
Gate results on the reference machine (hyperfine means; `rg -j4`):

| Corpus / query | rg -j4 | greeg | speedup | notes |
|---|---|---|---|---|
| TypeScript `createSourceFile`, `--fresh none` | 844 ms | 13.2 ms | 64× | gate < 15 ms |
| TypeScript `createSourceFile`, FSEvents | 844 ms | 23.8 ms | 35× | gate < 30 ms |
| TypeScript `createSourceFile`, stat | 844 ms | 61.3 ms | 14× | gate < 60 ms; 41 ms is `lstat` on 66k files |
| TypeScript `-w node` (facets) | 1,057 ms | 84 ms | 13× | gate < 250 ms |
| rust-lang/rust `mir_borrowck` | 1,286 ms | 4.8 ms | 268× | |
| django `get_queryset` | 88 ms | 5.3 ms | 17× | |
| tokio `fn poll` | 15.4 ms | 7.6 ms | 2× | |

Build phase 1: 2.08 s (TypeScript, 66k files), 2.13 s (rust); index 0.18× /
0.25× of source. Parity with `rg --json` on 21 queries through the index;
`bench/edits.py` (200 modified files, new file, new directory tree, delete,
directory rename, git restore) identical to ripgrep at every step on tokio
and django, without restart. Deferred: bucketed build mode, blob cache and
git-oid shortcut, in-place compaction (a background rebuild is the compaction).
Lessons are in D§15 (TTL 100 ms, FSEvents only above 8,000 files, index scope
= default ignore rules).

### M3 · Index phase 2: symbols, spans, graph, verbs (4 weeks)

Tasks:
1. `tags.scm`, `locals.scm`, `noncode.scm` for the five languages with
   fixtures and a coverage test per capture kind; kind normalization table.
2. Stage B extraction (D§4.1): parse with timeout, symbols, def spans, noncode
   spans, imports; `ERRORS_IN_PARSE` fallback to the regex extractor.
3. `symbols.bin` (array, names FST, tokens FST), `spans.bin` CSR, `graph.bin`
   with resolvers (D§7.2) and PageRank; quantized rank into `files.bin`.
4. Classification v1 via span tables and byte rules (D§6.2); `--precise`.
5. Verbs: `def`, `refs`, `callers`, `impls`, `outline`, `map`, `impact`
   (D§7.3–7.4) with text and JSON output; `--from` and session-based origin.
6. Ladder rungs 3 and 4 (split tokens, Levenshtein over FST).
7. Ranking v1 with PageRank and personalized restart on the focus set.
8. Session memory (D§9): log, dedup, loop detection, focus, hints.

Gate: phase 2 build < 12 s on 4 MLOC; `greeg def` p50 < 3 ms; `greeg refs`
on a 500-reference symbol < 80 ms; tree-sitter-vs-SCIP agreement (M6 oracle)
≥ 95 % for definitions on Python/TS/Rust and ≥ 85 % on Kotlin; reference
classification precision ≥ 0.9 against SCIP roles on sampled hits.

**Status: done 2026-09-01.** Format v2 adds `symbols.bin`, `spans.bin` and
`graph.bin` (D§3.3–3.5, `FORMAT.md`); stage B runs tree-sitter on all cores
after phase 1 publishes; verbs `def refs callers impls outline map impact`;
ladder rungs 3–4; `--precise`; session memory. Gate results (reference
machine, hyperfine means, process start included):

| Gate | Target | Measured |
|---|---|---|
| phase 2 build, rust-lang/rust (38,848 parsed files, 136 MB, ≈ 4 MLOC) | < 12 s | 3.8 s (parse 3.55 s on 12 threads, resolve + PageRank 73 ms) |
| phase 2 build, TypeScript (26,172 files) / django / ktor / tokio | | 1.3 s / 0.33 s / 0.23 s / 0.15 s |
| `greeg def` p50 | < 3 ms | 2.8 ms tokio `JoinHandle`, 2.8 ms rust `TyCtxt` (2.6–2.7 ms with `--fresh none`; ≈ 1.5 ms is process start) |
| `greeg refs` on a ≈ 500-reference symbol | < 80 ms | tokio `Semaphore` (367 hits) 4.0 ms; rust `HirId` (1,879 hits) 11 ms; rust `DefId` (4,344 hits) 23 ms |
| definition agreement, tree-sitter vs regex extractor | ≥ 95 % py/ts/rs, ≥ 85 % kt | Python 100 %, Rust 98.2 / 99.2 %, TypeScript 98.9 %, Kotlin 97.1 % (ktor) / 93.8 % (kotlinx.coroutines) |
| reference classification precision vs SCIP | ≥ 0.9 | not measured: the SCIP oracle is M6 work; `--precise` (tree-sitter node kinds) is the in-tree reference |
| parity and edit bursts with format v2 | identical | `bench/parity.py` PASS (21 queries, 5 corpora); `bench/edits.py` PASS on tokio and django (deltas carry symbols) |
| search latency after M3 (classification from stored spans, `--fresh none`) | no regression vs M2 | TypeScript `createSourceFile` 8.5 ms (M2: 13.2), `-w node` facets 66 ms (M2: 84), rust `mir_borrowck` 4.5 ms, `-w HirId` 10.2 ms, tokio `fn poll` 7.7 ms (rg -j4: 15.8) |

Index sizes (format v2): rust 90 MB for 188 MB source (grams 43, symbols
25, spans 15, files 5.5, graph 1.3); TypeScript 63 MB for 210 MB; django
21 MB; ktor 12 MB; tokio 4.3 MB. Phase 2 roughly doubles the index over
phase 1; the symbol and span tables are fixed-width and not yet compressed
(deferred, see below).

Extraction throughput is 7–10 MB/s per core across the five grammars (the
JavaScript baselines in the TypeScript repo are 3 MB/s and 22 % fallback:
they are compiler test outputs full of deliberate errors). Regex fallbacks:
0 % tokio/django, 0.4 % ktor (Kotlin 2.2 named context parameters), 1.5 %
rust (macro-heavy test files).

What it took: Rust items wrapped in `cfg_*! { … }` macros (tokio hides
`JoinHandle` and most of its public API that way) are opaque token trees to
tree-sitter, so brace-delimited macro bodies containing an item keyword are
re-parsed as items with shifted offsets (two levels deep); `impl Trait for
(A, B)` has no identifier to name the block, so it is named by the trait;
symbol lines are the *name's* line, not the declaration node's (Kotlin
annotations and TypeScript decorators start the node on an earlier line,
which is what made the first Kotlin agreement number 62 %); the Rust
resolver walked a 600-crate list per import with a `format!` per step
(2.2 s on rust-lang/rust) and now looks the crate up by directory once per
file (73 ms); Rust reference and generic types (`&'a T`, `Tx<T, Semaphore>`)
are typed by the byte rules instead of being idents.

Deviations from the design: names and tokens are sorted string tables with
binary search rather than FSTs (prefix and exact lookups are one binary
search; the fuzzy rung is a bounded Levenshtein scan over the name table,
≈ 10 ms on 400k names, acceptable for a zero-hit rung); `graph.bin` edges
come only from imports (no index-time reference pass, as designed); delta
segments carried symbols and spans but unresolved imports and a neutral rank
until M8; `tsconfig.json` `paths` were not honoured until M8; session ids
come from the first non-shell ancestor process (`proc_pidinfo` on macOS,
`/proc` on Linux) or `--session`/`GREEG_SESSION`; SCIP-based precision
measurement moves to M6. Deferred: compressed span tables (the tables are
mmapped and answered by binary search per hit; compression would add a
decode to that path to save 15 MB of a 90 MB index on rust-lang/rust),
FST-backed fuzzy search if the scan ever shows up in a profile (it only runs
on the zero-hit rung), `locals.scm` (locals are suppressed by query shape
instead), `greeg lang check`.

### M4 · Sparse grams and compaction tuning (2 weeks, gated)

Tasks: bigram weight table trained on the corpora (excluding the held-out
ones); index-time and query-time selection (D§5.3) with trigram fallback;
property test that query grams ⊆ document grams for 10⁶ random substrings;
format v2 behind a manifest flag; A/B on all corpora.

Gate to ship as default: ≥ 30 % smaller postings and ≥ 2× fewer candidate
files for the identifier family on at least four of five corpora, with no
query family slower than trigram by more than 5 %. Otherwise it stays an
opt-in flag and v1 ships trigrams.

**Status: done 2026-09-01 — gate failed; v1 ships trigrams, no opt-in.**
The A/B harness (`crates/greeg-index/examples/sparse_ab.rs`: `train` builds
a bigram weight table, `eval` measures every scheme on a corpus) compared
trigrams with two families of sparse grams on all five corpora, using
200–250 identifier queries per corpus (the S1 set plus every ~200th symbol
name from the index):

* `cox3-8`: the §5.3 rule (both boundary bigrams heavier than every interior
  bigram, lengths 3–8). Every trigram satisfies it vacuously, so it is a
  strict superset of the trigram index.
* `cox4-8`: the same rule without trigrams (literals with no selected gram
  scan).
* `minz k w`: minimizers, the heaviest k-mer (rarest bigram inside, hash
  tie-break) of every window of w k-mers, which is the only known local rule
  that guarantees query grams ⊆ document grams *and* a bounded gap; literals
  shorter than k + w − 1 scan.

| Corpus | Scheme | postings vs trigram | (gram, file) pairs vs trigram | candidates vs trigram (geo. mean) | queries worse | forced scans | extract MB/s/core |
|---|---|---|---|---|---|---|---|
| django | cox3-8 | 3.4× | 2.31× | 0.34 | 0 | 0 | 37 (tri 109) |
| django | cox4-8 | 2.4× | 1.31× | 0.46 | 23 / 218 | 9 | 64 |
| django | minz k4 w3 | 1.5× | 1.12× | 0.44 | 16 / 218 | 11 | 54 |
| ktor | cox3-8 | 2.9× | 2.06× | 0.51 | 0 | 0 | 40 |
| ktor | minz k4 w3 | 1.35× | 0.97× | 0.89 | 34 / 208 | 16 | 54 |
| tokio | cox3-8 | 3.3× | 2.22× | 0.68 | 0 | 0 | 39 |
| tokio | minz k4 w3 | 1.5× | 1.08× | 0.85 | 28 / 215 | 16 | 50 |
| rust (held out) | cox3-8 | ≈ 3× (pairs 2.07×) | 2.07× | 0.27 | 0 | 0 | 39 |
| rust (held out) | minz k4 w3 | ≈ 1.4× (pairs 0.98×) | 0.98× | 0.44 | 24 / 211 | 10 | 50 |
| TypeScript (held out) | cox3-8 | ≈ 3× (pairs 2.02×) | 2.02× | 0.41 | 0 | 0 | 44 |
| TypeScript (held out) | minz k4 w3 | ≈ 1.3× (pairs 0.91×) | 0.91× | 1.28 | 38 / 201 | 24 | 56 |

Weights were trained on tokio, django, ktor and kotlinx.coroutines; rust
and TypeScript are held out. Training on rust + TypeScript instead and
evaluating tokio gives the same picture (cox3-8 0.70, minz 0.87), so this is
not a training artifact. Postings bytes were measured with real roaring
bitmaps on the three small corpora and estimated from pairs on the two
large ones (long grams have shorter lists, so bytes per pair only go up).

Reading: the premise of §5.3 (common trigrams disappear, index shrinks)
does not hold. A trigram vocabulary is small and repeats inside files, so
(gram, file) pairs saturate at ≈ 0.1–0.15 per byte; long grams almost never
repeat, so every scheme that adds them adds pairs, and every scheme that
drops trigrams breaks literals of 3–6 bytes (`HirId`, `Model`, `node`) and
still does not cut pairs. The only scheme that cuts candidates by 2× or
more (cox3-8, the superset) triples the postings and halves extraction
throughput. And the cut is worth little: verifying `createSourceFile`'s
295 trigram candidates (18 true) costs ≈ 3 ms warm in a 4.5 ms query;
`mir_borrowck`'s 90 candidates (20 true) ≈ 1 ms. Decision: trigrams stay,
format v2 is unchanged, and no opt-in flag ships (it would be slower to
build, three times larger and at best a few milliseconds faster on the
longest identifiers). The harness stays in tree so the experiment can be
repeated when the corpus mix changes.

What the M4 time bought instead (the "tuning" half): the index path
materialized a `DefSummary` for every symbol of every matched file
(≈ 100 µs per Rust file); it now builds only the definitions the hits
reference (`-w HirId` on rust-lang/rust: classify CPU 34 → 5.7 ms, query
36 → 6.2 ms warm). Startup: `greeg --version` took 2.3 ms against 1.4 ms
for a hello-world with the same clap definition and 1.8 ms for a binary
that only links the five grammars; the missing millisecond was dyld loading
CoreFoundation and CoreServices for FSEvents on every invocation. The
frameworks are now `dlopen`ed on first use (only trees above 8,000 files
ask for FSEvents), which removes both from the link line: startup 1.7 ms,
`def JoinHandle` 2.8 → 2.1 ms, tokio `fn poll` 7.7 → 6.0 ms, and the
FSEvents check itself is unchanged (12 ms on the 66k-file tree, a touched
file is reported). Tests, clippy, parity-by-construction (no format change)
and `bench/edits.py` all pass.

### M5 · Hardening and distribution (2 weeks)

Tasks: panic-to-fallback wrapper; corrupt-index detection and rebuild; SIGBUS
guard for truncated mmaps (re-open and retry once, then scan mode);
`greeg doctor` (index status, freshness mode in use, language coverage, disk
use); man page and `--help` written for models (short, examples first);
`greeg hook claude` that installs a PreToolUse hook rewriting `rg`/`grep`
Bash calls to `greeg` and writes a short skill file; `cargo dist` release
pipeline; Homebrew tap and `cargo install`; runtime grammar loading for extra
languages.

Gate: 24-hour soak running an agent loop of randomized queries and edits on
three corpora with zero incorrect results versus `rg` and zero crashes; binary
< 25 MB; cold start on an unindexed repo answers in scan mode within
1.1 × rg.

**Status: done 2026-09-01** (soak result below). What shipped:

* *Panic-to-fallback*: the index path runs under `catch_unwind`; a panic
  prints one line, the query is answered from a scan, the manifest is
  dropped and a rebuild is queued (`GREEG_DEBUG_PANIC=1` injects one).
* *Corruption*: unreadable headers or delta files fail `Index::open` and
  trigger a rebuild; a posting list that fails to deserialize reads as
  "every file" for that gram (the answer stays a superset, so verification
  keeps it right) and marks the index for rebuild.
* *SIGBUS guard*: a handler `execv`s the same command with
  `--no-index --after-sigbus` (async-signal-safe); the re-executed process
  answers from a scan and rebuilds (`GREEG_DEBUG_SIGBUS=1` injects one).
  Own writers never truncate in place, so this only covers outside damage.
* *Deferred build spawn*: the background build starts after the answer is
  written, not before the scan (first query on django: 0.2–0.4 s → scan + 5 ms).
* `greeg doctor`: binary, root, index dir, generation, per-component disk
  use and ratio to source, counts, freshness mode `auto` would pick,
  languages with grammar and fallback counts, extra languages, sessions,
  advice.
* `--help` rewritten for models (examples first, how to read the output);
  `greeg man` prints the man page (clap_mangen).
* `greeg hook claude [--dry-run|--uninstall]` installs a Claude Code
  PreToolUse hook (`greeg hook run`, first in the list so it precedes other
  Bash hooks) that rewrites plain `rg`/`grep` invocations to `greeg`
  (flags mapped, `--include` → `-g`, unsupported semantics such as `-v`,
  `-o`, `--files`, expansions and globs left untouched, only the first
  pipeline segment) and writes `~/.claude/skills/greeg/SKILL.md`.
* `greeg stats` (opt-in: `greeg stats enable` writes `stats = true` to
  `~/.config/greeg/config.toml`; `GREEG_STATS=1|0` overrides): the hook
  appends a `hook` record per rewrite and every greeg run a `run` record
  (wall from process start, scan/shape ms, output bytes and token estimate)
  to `<cache>/stats/events.jsonl` (0600, rotates at 16 MB). The two join on
  blake3(cwd, rewritten argv). `greeg stats replay` runs the original
  rg/grep and the greeg rewrite in the recorded cwd (argv, never `sh -c`;
  one warm-up, median of `--runs`, killed after `--timeout`; the index is
  built first when missing) and the report prints avg/min/p50/p95/
  p99/max latency and tokens per class plus savings against rg capped at
  Claude Code's 30 000-character tool-output limit (`--cap`, `stats_cap`).
  `--since`, `--repo`, `--json`, `--verbose`, `status`, `clear`.
* Runtime grammars (D§11): `$GREEG_LANG_DIR` or `~/.config/greeg/lang/<name>/`
  with `spec.toml`, `grammar.so|dylib` and `tags.scm`; `dlopen` on first
  use, ABI-checked; generic comment/string lexer from the spec; `-t <name>`
  works; `greeg lang check DIR` validates and reports coverage. Verified
  with tree-sitter-go compiled from source on the Go port inside the
  TypeScript corpus: 5,105 Go files, 382k symbols in 2.1 s, `def`,
  `outline` and `-t go` working.
* CI (`.github/workflows/ci.yml`: build, test, clippy `-D warnings`, binary
  size gate, smoke on macOS and Linux) and release
  (`release.yml`: four targets, tarballs with sha256 and man page, GitHub
  release, Homebrew formula rendered from `homebrew/greeg.rb.in`).
  `cargo install --path crates/greeg` works today; the tap and crates.io
  publication need the repository to exist.

Gate results:

| Gate | Target | Measured |
|---|---|---|
| binary size | < 25 MB | 15.2 MB (release, five grammars linked) |
| cold start, scan mode vs `rg -j4` (warm cache) | ≤ 1.1× | TypeScript `createSourceFile` 0.98×, `-w node` 0.82–0.95×, django `get_queryset` 1.05× |
| first query with no index | scan + spawn | 131–156 ms on django (scan 125–147 ms, spawn ≈ 5 ms; the build runs after exit) |
| soak: randomized queries and edits vs rg, three corpora | 24 h, zero mismatches, zero crashes | first 9-minute run: 3,260 iterations, 0 crashes, 61 mismatches (two filter-semantics bugs, below); after the fix: 3,249 iterations, 0 mismatches, 0 crashes, worst query 1.2 s (`-w class` in parity mode on django) |
| fault injection | answer still correct | panic → one line + scan + rebuild; SIGBUS → re-exec + scan + rebuild; both verified |

What the soak found (both fixed in scan and index mode, verified with
(path, line) set comparisons and the parity suite): `-g '!*test*'` must
prune every file under a directory named `test`, as ripgrep's walker does
when it matches the glob against directories, so the index path now
checks override globs against every ancestor directory; and ripgrep gives
an override glob precedence over `-t`/`-T` (a file matching a whitelist
glob is searched even when the type filter would drop it), so both paths
now use ripgrep's own type definitions (`ignore::types`, plus greeg's
aliases and the runtime-loaded languages) with that precedence instead of
greeg's language table.

Deviations: the 24-hour soak has run for two 9-minute sessions so far (`bench/soak.py
MINUTES GREEG CORPUS...` is the harness; the long run is a CI/nightly job
once the repository exists); `cargo dist` was replaced by a plain
workflow because the tool is not installed here; `--help` doubles as the
man page source rather than a hand-written page; runtime grammars have no
definition regexes (scan-mode `def` kinds come only from the index) and no
import resolution.

### M6 · Benchmark suite (runs alongside M1–M5, 3 weeks total)

`greeg bench` implements the protocol from the research brief:

1. `bench/corpora.toml`: the 13 pinned repos (cpython `42de93a`, django
   `5babd2e`, home-assistant `f945b3d`, JetBrains/kotlin `9a22fe7`, ktor
   `f92fad0`, kotlinx.coroutines `f63a04b`, rust-lang/rust `a433023`, tokio
   `ea91b33`, deno `62a0d5d`, node `5c447f6`, vscode `c89cda6`, TypeScript
   `v5.9` tag for the pre-Go layout plus `main` for the many-files case,
   next.js `52cd5b9`) plus ripgrep's OpenSubtitles single file. `greeg bench
   fetch` shallow-clones at the SHA into the bench cache.
2. Speed protocol: `hyperfine -N --warmup 3 --runs 10 --export-json` per
   (tool, corpus, family, cache state); equalized flags; match-count
   verification; cold runs with `sudo purge` / `drop_caches` when permitted;
   index build time, size ratio, RSS; geometric-mean speedup vs rg per
   family; JSON results and a markdown table.
3. Quality protocol: `greeg bench oracle` runs `rust-analyzer scip`,
   `scip-typescript`, `scip-python`, `scip-java` (Gradle Kotlin repos) where
   the toolchain is present, dumps occurrences, samples symbols by ambiguity
   and kind, and scores definitions Acc@1/5/10, reference recall split by
   code vs noncode, context efficiency at a 500-line budget, tokens per query
   and tokens-to-first-correct-hit, for `greeg`, `rg`, and `grep`.
4. Agent protocol (manual, documented): arms A/B/C with `claude -p
   --output-format stream-json --disallowedTools Grep`, three paired runs,
   paired bootstrap; a script that extracts tool calls, tokens and success
   from the stream.
5. CI: kernels via `criterion` with saved baselines (5 % gate); e2e on the
   small corpora (tokio, ktor, django) per PR with a 10 % gate; full suite
   nightly on the reference machine.

**Status: done 2026-09-01.** What shipped: `bench/corpora.toml` (the 13
pinned repositories plus the TypeScript `v5.9.2` tag and OpenSubtitles, one
query per family each) and `bench/bench.py` with `fetch` (shallow clone at
the SHA or tag into `$GREEG_BENCH_CACHE`), `speed` (hyperfine `-N --warmup 3
--runs 10` per tool × corpus × family, warm and optionally cold, index build
in a fresh directory, RSS via `/usr/bin/time`, (path, line) match-set
verification between rg and greeg, geometric-mean speedups), `oracle` (runs
`rust-analyzer scip`, `scip-python`, `scip-typescript` or `scip-java` when
missing, decodes the SCIP protobuf with a stdlib reader, samples 75 names
per corpus stratified by ambiguity 1 / 2–5 / 6+ and kind, scores Acc@1/5/10
for `greeg def` vs `rg -nw` vs `grep -rnw`, reference and definition recall
of `greeg refs --budget 0`, hit-kind precision vs SCIP roles from spans and
with `--precise`, and context efficiency at 500 lines: useful lines,
coverage, o200k tokens, tokens to the first true definition), `gate`
(`speed` 10 %, `kernels` 5 %, baselines keyed by OS/arch/CPU under
`bench/baselines/`) and `report` (`docs/BENCH.md`). Criterion kernels in
`crates/greeg-index/benches/kernels.rs`; CI jobs `kernels` and `e2e` in
`ci.yml`; `nightly.yml` runs the medium tier, the oracle and a five-hour
soak. The agent protocol is documented and tooled under `bench/agent/`
(ten tasks with mechanical checks, three arms, `extract.py` for stream-json,
`analyze.py` paired bootstrap) and is run by hand.

Speed, reference machine, warm cache, means (`docs/BENCH.md` has every row):

| corpus | files | index build | index / source | rg | rg -j4 | greeg scan | greeg (indexed, budgeted) |
|---|---:|---:|---:|---:|---:|---:|---:|
| tokio | 849 | 0.23 s | 0.71× | 19–22 ms | 14–17 ms | 17–20 ms | 4.4–6.8 ms |
| ktor | 3,057 | 0.52 s | 0.77× | 107–117 ms | 61–65 ms | 62–73 ms | 5.7–12.6 ms |
| django | 7,031 | 0.76 s | 0.56× | 159–170 ms | 90–100 ms | 94–123 ms | 3.1–20 ms |
| kotlinx.coroutines | 1,314 | 0.21 s | 0.81× | 30–32 ms | 19–20 ms | 19–24 ms | 4.6–6.8 ms |
| TypeScript v5.9.2 | 74,268 | 5.8 s | 0.24× | 3.8–4.3 s | 0.97 s | 0.95–1.07 s | 6.4–82 ms |
| TypeScript main | 65,938 | 3.0 s | 0.30× | 3.1–3.2 s | 0.81–0.84 s | 0.81–0.91 s | 6.9–66 ms |
| rust-lang/rust | 62,362 | 5.0 s | 0.48× | 3.1–3.4 s | 0.79–0.84 s | 0.79–0.85 s | 3.4–9.5 ms |

Geometric mean over the seven corpora, relative to `rg` (default threads):
ident 57.5×, word 22.6×, phrase 56.7×, regex 40.1× for the budgeted greeg
answer; scan mode 2.1–2.2× (it uses 4 reader threads, like `rg -j4`); grep
0.55×. Verbs: `def` 2.0–2.8 ms, `refs` 3.2–16 ms, `callers` 3.4–29 ms on
every corpus. The slow greeg rows (`-w node`, 30–48k matches, 66–96 ms; `def
test_\w+`, 18k matches, 20 ms) are dominated by classifying and shaping
tens of thousands of hits, not by the index.

Quality, SCIP oracle (75 sampled names per corpus, `docs/BENCH.md`):

| | tokio (rust-analyzer) | django (scip-python) | TypeScript v5.9.2 (scip-typescript) |
|---|---|---|---|
| `greeg def` Acc@1 / @5 / @10 | 93 / 99 / 99 % | 96 / 96 / 96 % | 96 / 100 / 100 % (M8; 69 / 89 / 96 % before) |
| same, ambiguity 1 | 96 / 100 / 100 % | 92 / 92 / 92 % | 100 / 100 / 100 % |
| `rg -nw` first lines Acc@1 / @5 / @10 | 45 / 71 / 73 % | 56 / 76 / 79 % | 27 / 52 / 72 % |
| `grep -rnw` Acc@1 / @5 / @10 | 43 / 65 / 72 % | 56 / 75 / 80 % | 47 / 61 / 71 % |
| reference recall (`refs --budget 0`) | 98 % | 100 % | 97 % |
| definition recall | 98 % | 100 % | 88 % |
| def precision vs SCIP roles (spans / `--precise`) | 95 / 95 % | 100 / 100 % | 96 / 88 % |
| noncode precision | 100 % | 99 % | 100 % |
| tokens per bare-name query, greeg / rg (first 500 lines) | 700 / 2,813 | 609 / 4,420 | 778 / 10,894 |
| tokens to first true definition, median, greeg / rg | 41 / 48 | 38 / 31 | 124 / 77 |

Gate results against the M3 targets that were deferred here:

| Gate | Target | Measured |
|---|---|---|
| definition agreement vs SCIP, Python / Rust | ≥ 95 % | 100 % / 98 % (definition recall); Acc@10 on unambiguous names 92 % / 100 % |
| definition agreement vs SCIP, TypeScript | ≥ 95 % | 89 % recall; Acc@10 on unambiguous names 100 %. The misses are all names with ≥ 6 definitions inside `tests/cases/**` fixtures (`Dog`, `Boxified`, `foo14`), where `def` and `refs` list the first 40 definitions |
| definition agreement, Kotlin | ≥ 85 % | not measured: `scip-java` needs coursier and a Gradle build; the tree-sitter-vs-regex agreement (94–97 %) stands in |
| reference classification precision vs SCIP roles | ≥ 0.9 | 0.95 / 1.00 / 0.96 (def role); noncode 0.99–1.00 |
| kernels | criterion with baselines | gram extraction 1.29 GiB/s, lexer 2.0–2.1 GiB/s, tree-sitter extraction 14 MiB/s per core, planner 0.6–6.2 µs per pattern, 3-way roaring AND on 60k/40k/20k 5.1 µs, posting deserialize 0.8 µs; baseline saved for `darwin-arm64-apple-m4-pro` |
| e2e gate | 10 % | baseline saved; `bench/bench.py gate speed` PASS on the same host |

Findings. (1) The speed run's match-set verification found one line per
TypeScript corpus that greeg missed: a UTF-16 file with a byte-order mark,
which ripgrep transcodes and greeg had skipped as binary. Every file read
now goes through `greeg_lang::transcode_utf16`; the soak never hit it
because its query mix did not include `-w node`. (2) `impl Foo` headers are
definitions to greeg and references to SCIP; they are reported separately
(tokio: 118 of the 95 %-precision denominator would fall to 80 % if counted
as errors). (3) greeg's facet header costs about 90 tokens before the first
definition line, which is why rg's median tokens-to-first-definition is
lower on django and TypeScript even though greeg's Acc@1 is 1.4–2.1× higher;
moving the definitions block above the facets is a candidate for the output
contract in v1.1. (4) Raw density (useful lines per 1k tokens) favours rg
2–3× because greeg shows context lines and summaries; the budgeted answer is
4–14× smaller in total.

Deviations: `greeg bench` is `bench/bench.py`, not a subcommand (D§15);
cold-cache runs need passwordless `sudo purge` and were skipped here (the
`--cold` flag is implemented and used by the nightly); the eight large
corpora (cpython, home-assistant, kotlin, deno, node, vscode, next.js,
OpenSubtitles) are pinned and fetchable but were not run on the reference
machine in this session; the agent protocol was not run (API cost, manual by
design); the Kotlin oracle waits for `scip-java`; the Linux x86_64 gate run
in the definition of done waits for the CI runner.

### M7 · Review fixes, v0.2 (1 week)

Source: `docs/REVIEW.md` (2026-09-02), executed in its §7 order.

**Status: done 2026-09-02.** Shipped on branch `v0.2-review-fixes`:

* *Parity blockers*: absolute and `..` path arguments are canonicalised
  against the index root (outside → scan); glob-looking positionals become
  `-g`; `-U` multiline hits carry their own line numbers; one JSON `match`
  record per line with every submatch; `-l` prints bare paths and `-c`
  `path:count` with the footer on stderr and no budget truncation; stdin is
  searched when it is a readable pipe or file; exit 1 whenever the user's
  pattern itself had no hits (ladder substitutions included); rg's cosmetic
  flags are accepted; `-e` takes several patterns and leading-dash values;
  `--json` emits rg-exact `lines.text`/`submatches`, `context`, `begin`/`end`
  and `summary` records. `bench/parity.py` gained a 14-row matrix covering
  every one of these plus `.gitignore` edits, BOM files and exit codes.
* *Hook*: positional arguments and `--` are never dropped; combined short
  flags with attached values expand correctly; unspaced pipes and operators
  are tokenised; grep basic regexps are translated to ERE or left alone;
  unsupported semantics stay untouched (13 unit tests).
* *Index safety*: `.gitignore`/`.ignore`/`.rgignore` are tracked (format v3)
  and an edit forces a rebuild with a scan-mode answer meanwhile; writers
  take an exclusive `LOCK`, publish the manifest last and re-check it before
  applying a delta; `Index::open` loads only the deltas the manifest names;
  delta parsing is bounds-checked; `HUGE` files stay candidates; FSEvents
  re-stats every known file below a reported directory; `fsevents_id` is
  captured before the check; the `-i` planner keeps grams through `s`/`k`
  (`-i assessSkip` on TypeScript: 836 → 58 ms). Six integration tests.
* *Symbols and flags*: `exported` means `pub`/non-`_`/non-`private|internal`
  per language; `#[cfg(not(test))]` is not a test; functions in modules stay
  `fn`; Kotlin locals are no longer symbols; mock/stub/fake paths are
  demoted; over-broad `build|out|gen|deps|external|spec` rules removed;
  DESIGN §8 lists exactly what the code does. Agreement: Python 100 %,
  Rust 99.2 %, TypeScript 99.1 %, Kotlin 98.6 %.
* *Ranking and output contract v2* (`docs/OUTPUT.md`): calls above imports,
  exact-name definitions first, recency dropped, per-file cap scales with
  the file count; every layout groups by file with the path printed once,
  last container only (`--chain` for the full chain), demoted definitions
  collapsed to a count, imports collapsed to one line, two-line facets
  header with definitions above it, flags-only hints, no size/age/ms.
  o200k tokens on the standard queries: django `get_queryset` 1,378 → 899,
  TypeScript `-w node` 1,410 → 531, `refs Semaphore` 2,769 → 1,966,
  `callers spawn_blocking` 1,870 → 1,350; estimator within 0.98–1.06 of
  o200k.
* *Benchmark honesty* (protocol 2): every greeg run is preceded by
  `sleep 0.15` so the freshness check is paid, a `fresh` column is
  reported, medians replace means, the headline speedup is against
  `rg -j4`, the oracle samples usage-weighted names with an rg
  definition-regex baseline and bootstrap intervals, summary lines are
  neutral in the context metric, CI runs the parity matrix and `edits.py`
  on macOS and Linux and records (not gates) speed off the reference
  machine.
* Perf: shown files are read once; top-k selection; ladder rung 5 bounded
  to 50 ms and never walks `.git/`; `refs`/`impact` run one freshness check;
  `madvise` on the maps; phase-1 accumulators per thread.

Not done: the agent A/B/C protocol run (manual, API cost), the 24 h soak,
the Kotlin SCIP oracle, the eight large corpora, `--sort` kinds other than
`path`, `--count-matches`.

### M8 · Graph freshness and tsconfig paths (2 days)

Two of the M3 deferrals turned out to cost the agent exactly where it looks:
an edited file lost every import edge until a rebuild, and TypeScript
repositories built on path aliases had almost no graph at all.

Tasks:

1. A modified file keeps the rank of the version it replaces; new files stay
   neutral.
2. Delta segments resolve imports against the live base plus their own
   files and store the edges plus the superseded id per file (format v4,
   `FORMAT.md`).
3. Queries fold the base graph and the deltas: edges to a superseded id
   follow the file to its newest version; importers of a file are its
   unedited base importers plus the delta files that reach any version.
4. Gate: an index-level test that edits an imported file, an importer and
   adds a new importer, checking ranks and edges in both directions after
   each delta; `bench/edits.py` checks `def --from` on an edited importer
   still reports reach 1.0; the delta stays within the M2 budget.
5. `tsconfig.json`/`jsconfig.json` (nearest by directory, `extends` chain,
   JSON with comments) supply `paths` and `baseUrl`; probing falls through
   to the existing extension and index rules.
6. Gate: an alias-heavy corpus (shadcn-ui/ui) before and after.
7. Workspace packages (`package.json` `workspaces`, `pnpm-workspace.yaml`)
   resolve by name.

8. TypeScript/JavaScript accuracy against the SCIP oracle (`bench.py oracle
   TypeScript-5.9`, same 85 sampled names): `tests/baselines/reference/*.js`
   (compiler output that embeds the test source) flagged generated by
   directory name and by the `//// [` header; `exported` no longer leaks
   through function and class bodies; `def` ranks a declaration nested in a
   function at 0.7; object-literal members (`{ watchFile: () => … }`) carry
   symbol flag 8, classify as `member` (indexed and `--precise`) and rank at
   0.6, since scip-typescript records them as references to the typed member
   they implement; `var`/`let`/`const` inside `module`/`namespace` blocks are
   extracted; dynamic `import("…")` is an import. Triage tools: the ignored
   tests `dump_parse_errors` and `dump_sexp` in `greeg-lang`.

**Status: done 2026-09-03.** Format v4. Delta apply on rust-lang/rust with a
100 KB file: 15.5 → 17 ms (a path map over 62k files plus 384 `Cargo.toml`
reads on four threads); tokio unchanged at ≈ 15 ms. shadcn-ui/ui (3,875 TS
files, 10.3k `@/…` imports, 610 relative): 631 → 7,723 edges, phase 2
266 → 272 ms. Kotlin deltas resolve against the base by directory suffix
because the base stores no packages. `bench/edits.py` PASS on tokio with
the graph check. TypeScript-5.9 oracle: `def` Acc@1/5/10 69/89/96 % →
96/100/100 % (ambiguity 6+: 64 → 92 % at rank 1), definition precision
93 → 96 % (spans) and 86 → 88 % (`--precise`), definition recall 98 →
100 % on the classified hits, context useful lines 40 → 51 %, first
definition reached 85 → 97 % of queries. Remaining rank-1 misses are `.d.ts`
library names whose SCIP definition is the copy under `tests/lib` and names
with ten or more fixture definitions. Not done: Java, `exports` maps in
`package.json`, recomputing PageRank incrementally (a rebuild still does
that), and the tree-sitter-typescript bug on labeled tuple elements named
`symbol` (`[symbol: Symbol, …]`, one error node in `src/compiler/types.ts`).

### M9 · The answer is the whole word (1 day)

Source: the M8 oracle run. Half of what the digest printed for a bare
identifier could not be an occurrence of it. Measured over the 75 ranked
TypeScript-5.9 names, of 1,466 location-bearing lines: 499 (34 %) matched the
name inside a *longer* identifier (`createSourceFileWithText` for
`createSourceFile`, which also made the block claim "definitions (9 of 9)"
when SCIP knows 7), 242 (17 %) were the adaptive context lines of the ≤ 10-hit
rung, and 0 were duplicates. Useful lines were 51 %.

Tasks:

1. A hit carries `exact` (`is_exact`: the match is the whole pattern as a
   word, exact case), set where it is already computed; `indexed.rs` and the
   `-w` hint read it instead of recomputing or comparing match lengths.
2. A bare identifier searched literally and case-sensitively with at least one
   whole-word match answers about that word: near-misses leave the ranked
   answer and collapse to one `related` line naming up to four identifiers
   with counts (text and `--json` footer). Never in parity mode (`--budget 0`,
   `-l`, `-c`), never when nothing matches the whole word.
3. `+N more` and the `--budget` hint count only the hits the answer may draw
   on, so neither promises lines a larger budget would not show.
4. The `≤ 10 hits → 1 before, 1 after` adaptive-context rung becomes none:
   from four hits on the answer is hit lines only. One hit and ≤ 3 hits keep
   their context, `-A/-B/-C` are untouched.
5. Gate: `bench.py oracle TypeScript-5.9`, `bench/parity.py`, unit tests for
   the near-miss split and for parity keeping ripgrep's match set.
6. Protocol 3: 64 % of the non-useful lines that remain lie in files
   `scip-typescript` does not index (`lib.dom.d.ts`, `tests/baselines/**`,
   fixture `.js`). The oracle scores those as not-useful although it cannot
   adjudicate them, while the classification metric already skips them
   (`bench.py`, "only files SCIP covers say anything about roles").
   `context_metrics` gains a second ratio over the SCIP-covered lines only,
   reported **beside** the old one, never replacing it: the strict column
   keeps charging every tool for what it printed, the new one measures
   ranking on the lines the ground truth can judge.

**Status: done 2026-09-04.** TypeScript-5.9 oracle, same 75 names, greeg
0.3.0 + these changes:

| | M8 | M9 |
|---|---|---|
| context useful lines | 51 % | **77 %** |
| same over SCIP-covered lines only (protocol 3) | – | **93 % [90–96]** |
| first definition reached | 97 % | 100 % |
| coverage of true locations | 55 % | 63 % |
| tokens per bare-name query, mean / median | 628 / 528 | 531 / 517 |
| distinct true locations per 1k tokens | 16.3 | 22.3 (rg 20.3) |
| same at `--budget 6000` | 46 % | 72 % |

`def` Acc@1/5/10 (96/100/100 %), reference recall (97 %), definition recall
(88 %) and classification precision (96 % spans, 88 % `--precise`) are
unchanged: the verbs and the `--budget 0` paths never see the split.
`bench/parity.py` PASS (16 queries, 14-row matrix).

Protocol 3 drops 21 % of greeg's location lines from the denominator but
65 % of `rg`'s and 77 % of `grep`'s, which dump every match in the baseline
and fixture trees: it moves rg 55 → 84 % and grep 35 → 83 %, so it narrows
greeg's lead (77 vs 55 → 93 vs 84) rather than flattering it. That asymmetry
is the point — what is left is ranking, not who read fewer un-adjudicable
files. The 95 % bootstrap interval is [89.6, 95.6] at n = 75: the estimate
clears 90 %, the interval's lower bound sits just under it.

django and tokio were re-run on 2026-09-04 to test the near-miss split
against the naming conventions most likely to break it — Python's leading
underscore and Rust's `_mut`/`_ref` suffixes. Neither regressed; the split
puts them on the `related` line, named and counted:

| | tokio | django |
|---|---|---|
| useful lines, strict / SCIP-covered | 56 % → 76 % / 81 % [76–86] | 51 % → 73 % / 84 % [79–89] |
| coverage | 55 → 60 % | 50 → 52 % |
| tokens per query, mean / median | 726 → 612 / 577 → 528 | 649 → 525 / 523 → 478 |
| `def` Acc@1/5/10 | 83/97/99 %, unchanged | 91/91/91 %, unchanged |
| reference / definition recall | 98 / 95 %, unchanged | 99 / 100 %, unchanged |

Those two deltas span greeg 0.2.0 → 0.3.0 + M8 + M9, not M9 alone: their
2026-09-02 rows predate both milestones. The covered ratio is lower than
TypeScript's 93 % because the ground truth is sparser — rust-analyzer marks
55 % of greeg's code hits as occurrences and scip-python 43 %, against
scip-typescript's 86 % — so the covered denominator is a per-indexer number
and only comparable within one corpus.

`bench/bench.py fetch` now deepens an existing depth-1 clone before checking
out a pinned commit; without it a re-fetch of a corpus whose pin moved fails.

Not done: the wider sample (`--per-bucket 50`) that would halve the ±3 pp
interval, and the agent A/B/C protocol.

## 4. Testing strategy

* Unit: gram extraction (property: every 3-byte window present; line
  terminators excluded), regex analysis (table of 200 patterns with expected
  gram queries, including the degenerate `ANY` cases), classification byte
  rules per language, adversarial flags, token estimator.
* Golden: per-language fixture files with expected symbols, spans and
  imports as JSON; expected text output for 30 canonical queries per corpus
  snapshot (small fixture repos vendored under `tests/fixtures`).
* Differential: `(path, line)` parity with `rg -n` on every corpus and query
  family, in indexed and scan modes, before and after scripted edits.
* Property: query grams ⊆ document grams (M4); delta ∪ base minus tombstones
  equals a fresh full build for random edit sequences.
* Fuzz: regex analysis and the noncode lexer with `cargo fuzz` (no panics, no
  quadratic blowups over 1 MiB inputs).
* Performance: gates above, enforced in CI.

## 5. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Kotlin grammar gaps (61 % PSI structural match upstream) | S3 measures; regex extractor fallback per file; contribute fixes to the `-sg` fork; document unsupported syntax |
| tree-sitter API churn | pin `=0.27.x`, vendor grammar sources, upgrade only with the corpus test green |
| FSEvents log purged or disabled | detect and fall back to stat mode; never trust a stale id |
| Kernel contention on macOS reads | reader pool of 4, measured; expose `-j` for the read phase |
| Index bloat on giant monorepos | bucketed build, sharding by `FileId` range above 200k files, `HUGE` cap |
| Heuristic resolution wrong on overloads/extension receivers | confidence in output; `--precise`; SCIP import in v1.x |
| Model ignores the tool | rg-compatible flags, `greeg hook claude`, arm C of the agent benchmark decides what to change |
| Token estimate drift across models | calibration test; `--budget-exact` |

## 6. Definition of done for v1.0

All M1–M3, M5 and M6 gates green on the reference machine and on a Linux
x86_64 runner; `greeg bench` report published in `docs/` with the results
table from the research brief filled in for `grep`, `rg` and `greeg`;
DESIGN.md updated to match what shipped, with the decisions log extended for
every deviation.
