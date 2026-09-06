# greeg — Technical Design

Status: v1.4, 2026-09-02, updated through M7 (v0.2 review fixes; the shipped output contract is `OUTPUT.md`) (implementation notes are marked "As shipped"). Companion to `PLAN.md`. Every number in this
document is either measured on the reference machine (Apple M4 Pro, 12 cores,
24 GB, macOS 25.5, APFS) or marked as a target.

## 0. One-paragraph summary

greeg is a single-binary, zero-config code search CLI for coding agents. It
accepts ripgrep's flags and regex dialect, but answers from a persistent
per-repository index so that a query touches only candidate files instead of
opening every file, classifies every hit with syntax information computed at
index time, ranks hits by usefulness instead of file order, and shapes the
output to a token budget with an explicit account of what was omitted. The
index is built in the background on first use, kept fresh by a cheap stat pass
(or file-system event log where available), and updated per file in
milliseconds. Without an index the tool degrades to a ripgrep-speed scan with
identical output features, so it is never slower than what it replaces.

## 1. Performance goals

Measured baseline (ripgrep 15.2.0, warm page cache, reference machine):

| Tree | Files | rg walk only | rg query (default threads) | rg query, 1 thread | rg query, 4 threads |
|---|---|---|---|---|---|
| microsoft/TypeScript main | 65,977 | 67 ms | 2.0–3.3 s (13–32 s system CPU) | 1.27 s | 0.92 s |
| django | 7,082 | — | 166 ms | — | — |
| ktor | 3,104 | — | 115 ms | — | — |
| tokio | 868 | — | 21 ms | — | — |

Interpretation: on many-small-file trees the cost is per-file `open/read/close`
(about 17 µs per file single-threaded on APFS) and it gets worse with more
threads because of kernel lock contention. Matching is negligible (0.26 s user
time). Python reading all 66k files sequentially takes 1.32 s; `lstat` on all of
them takes 131 ms; `readdir` of the whole tree takes 75 ms.

Targets (p50 warm, reference machine, 4 MLOC / 60k-file repo unless noted):

| Operation | Target | Why it is achievable |
|---|---|---|
| Process start to first byte of output (index present, `--fresh=none`) | < 5 ms | mmap 6 files, no parsing, no walk |
| Freshness check, stat mode, 60k files | < 50 ms | measured: 41 ms file stats at 4 threads plus ≈ 3 ms directory stats (S2) |
| Freshness check, FSEvents mode (macOS) | < 20 ms | measured 12–14 ms for a 200-file change set (S2); work then scales with the change set |
| Rare-identifier query end to end (≤ 300 candidate files) | < 15 ms + freshness | measured: plan 48 µs, verify 219 candidates in 15 ms on the 66k-file tree (S1) |
| Common-identifier query (2,000 candidate files), facets mode | < 250 ms | count-only pass, 4 reader threads, no line extraction for non-shown files |
| Regex with no extractable literal (`\w{5}\s+\w{5}`) | ≤ rg -j4 on same tree | full scan over the file table, still no walk, tuned thread count |
| Full index build, phase 1 (file table + grams), 450 MB tree | < 3 s | measured 2.5 s at 4 reader threads; extraction 275 MB/s per core, reads 31 MB/s per core (S1) |
| Full index build, phase 2 (symbols, spans, imports), 150 MB of source | < 12 s | tree-sitter measured 10–30 MB/s per core, 12 cores |
| Incremental update, one 100 KB file | < 8 ms | parse 3–7 ms, gram diff < 1 ms, append to delta segment |
| Index size on disk | ≤ 0.35 × source above 50 MB, ≤ 15 MB below (trigram); ≤ 0.25 × (sparse-gram) | measured 0.15 × (TypeScript) and 0.23 × (rust) with roaring file-level postings (S1) |
| Query RSS | < 60 MB on 4 MLOC repo | mmap, touch only needed pages |
| Symbol lookup `greeg def Name` | < 3 ms | FST lookup + array slice |
| Scan mode (no index), any tree | ≥ ripgrep at its best thread count | same crates, adaptive `-j` |

Non-goals: type-precise cross-file resolution without an external index (SCIP
import provides that); searching binary files; a long-running server as a
requirement.

## 2. Process and storage model

### 2.1 Processes

* `greeg <args>`: one short-lived process per query. Opens the index read-only
  via mmap, performs a freshness check, applies inline updates for a bounded
  number of changed files, answers, exits. No daemon needed.
* `greeg index [--wait]`: builds or rebuilds the index. On first query in a repo
  without an index, the query process forks a detached `greeg index` and answers
  the current query in scan mode. Subsequent queries use whatever phase has
  completed (phase 1 grams enable candidate pruning before phase 2 symbols
  exist).
* `greeg watch`: optional resident process using `notify` (FSEvents/inotify)
  that keeps the index updated and writes a heartbeat; queries seeing a live
  heartbeat skip the stat pass entirely.

### 2.2 Location and locking

Index lives outside the repository by default:
`$XDG_CACHE_HOME/greeg/<blake3(realpath(root))[:16]>/` (macOS:
`~/Library/Caches/greeg/...`). Override with `GREEG_INDEX_DIR` or
`--index-dir`. Rationale: never dirties the working tree, survives `git clean`,
and lets one index serve several worktrees of the same repo only when they are
byte-identical (we key by realpath, so worktrees get separate indexes; blob
reuse across them is handled by the content-hash cache, §4.4).

Single writer, many readers. Writers take an exclusive `flock` on `LOCK`.
Readers never lock: every on-disk structure is immutable once published and
publication is an atomic `rename` of a new file over the old name; readers that
already mapped the old file keep a valid view until they exit. A `manifest`
file names the current generation of every component.

### 2.3 Files

Shipped layout (M3, format version 2) is specified byte by byte in
`FORMAT.md`; the list below is the intended full set (LOCK, tombstones as a
separate file and blobcache are not shipped: tombstones live in each delta,
a rebuild replaces compaction, and the blob cache is deferred).

```
manifest            JSON: format version, root, git HEAD oid, fsevents last id,
                    generation numbers, phase completion flags, stats
LOCK                flock target
files.<gen>.bin     file table (rkyv archive, mmap)
grams.<gen>.bin     gram dictionary + postings (custom layout, mmap)
symbols.<gen>.bin   symbol array + names FST + split-token FST (mmap)
spans.<gen>.bin     per-file span tables, CSR (mmap)
graph.<gen>.bin     import graph CSR + PageRank scores (mmap)
delta/NNNN.bin      overlay segments: small, self-contained mini-indexes of
                    recently changed files (same layouts, tiny)
tombstones.<gen>.bin roaring bitmap of file ids superseded by delta or deleted
session/<id>.jsonl  per-session query log (see §9)
blobcache/          content-hash -> extracted per-file record (grams, symbols,
                    spans), for reuse across branches/worktrees
```

All archives start with a 16-byte header: magic `GREEG`, format version u16,
component id u8, flags u8, payload length u64. Format version mismatch means
rebuild; there is no migration code, by design.

## 3. Data model

Integers are little-endian. `FileId = u32` assigned in sorted-path order so that
a path-prefix filter (`-g 'src/**'`, positional `PATH` argument) becomes one or
a few contiguous `FileId` ranges, which intersect with posting bitmaps in
constant time per range.

### 3.1 File table (`files.bin`)

Per file:

| Field | Type | Notes |
|---|---|---|
| `path_off, path_len` | u32, u16 | into a shared UTF-8 path arena, paths relative to root |
| `dir_id` | u32 | into a directory table (for readdir-based freshness and facets) |
| `size` | u64 | |
| `mtime_ns` | i64 | |
| `inode` | u64 | rename detection |
| `content_hash` | [u8;16] | blake3 truncated; keys the blob cache |
| `git_oid` | [u8;20] or zero | from the git index when available; cheap blob reuse |
| `lang` | u8 | language id, 0 = none |
| `flags` | u16 | bitset: `TEST GENERATED VENDORED MINIFIED BINARY LOCKFILE HUGE ERRORS_IN_PARSE IGNORED_BY_GIT SYMLINK` |
| `line_count` | u32 | |
| `max_line_len` | u32 | minified heuristic input, also caps output width decisions |
| `pagerank_q` | u16 | quantized file importance (§6.3) |
| `span_off, symbol_off` | u32, u32 | first entries in `spans.bin` and `symbols.bin` |

Directory table: `path_off/len`, `parent`, `mtime_ns`, `entry_count`,
`ignore_gen` (id of the effective ignore rule set for this directory).

### 3.2 Gram index (`grams.bin`)

Key: a *gram* is a short byte string. Two schemes share the same on-disk layout:

* **Trigram (format v1, ships first).** Every 3-byte window of the ASCII
  case-folded content. Proven (Cox 2012, Hound, Zoekt), simple, size ≈ 20–35 %
  of source with file-level postings.
* **Sparse gram (format v2, planned).** Variable-length grams (3–8 bytes)
  selected by a bigram-weight rule (§5.3). Fewer, more selective postings.
  Enabled only after the validation gate in `PLAN.md` M4 passes.

Layout:

```
header
dict:      n_grams × { key: u64 (gram bytes packed, len in top byte),
                       posting_off: u64, doc_count: u32 }
           sorted by key -> binary search, or a 3-level radix table for
           trigrams (16 MiB direct table: key -> posting_off; used when
           format = trigram and file count > 5000, else the sorted dict)
postings:  concatenated roaring bitmaps (portable serialization) over FileId
```

Why file-level (non-positional) postings: we must open a candidate file anyway
to print lines, enclosing symbols and context, so positional verification buys
little, and Zoekt's positional lists cost 2–3.5 × corpus. File-level postings
with roaring compression fit the ≤ 0.35 × budget and intersect at memory speed.

Why ASCII case folding at index time: `-i` queries become free; selectivity loss
is small for identifiers (`Node` vs `node` share a list, both are verified by
the real matcher). Non-ASCII bytes are indexed raw.

What is indexed: files with `lang != 0` or a text mime guess, `size ≤ 4 MiB`
(larger files are marked `HUGE`, searched only in scan mode or with
`--include-huge`), not `BINARY`. `MINIFIED`/`GENERATED`/`VENDORED` files *are*
indexed (renames need them) but carry flags that demote and summarize them.

### 3.3 Symbols (`symbols.bin`)

```
symbols:   n × { name_id: u32, kind: u8, file: FileId, start: u32, end: u32,
                 line: u32, name_start: u32, parent: u32 (symbol id or NONE),
                 sig_len: u16, flags: u8 (exported/public, is_test, has_doc),
                 supertype_ids: (u32 off, u8 len) into a u32 array }
           sorted by (file, start); a file's symbols are a contiguous slice
names:     FST map: exact name bytes -> u32 name_id
           name_id -> { first_symbol_off, count } into a symbol-id array sorted
           by name then by rank
tokens:    FST map: lowercase split token (camelCase/snake_case/PascalCase parts,
           min length 3) -> range into a u32 array of name_ids
kinds:     function method class struct enum trait interface typealias module
           object constant variable field property macro enum_member
           (normalized across languages, mapped from each language's tags.scm)
```

The `tokens` FST supports the split-name rung of the escalation ladder and
`--split` searches (`getUserName` finds `get_user_name`). The `names` FST
supports prefix and Levenshtein-automaton fuzzy search without extra data.

**As shipped (M3).** `symbols.bin` uses sorted string tables with binary
search instead of FSTs: exact and prefix lookups are one binary search over
`name_off`, and the fuzzy rung is a banded Levenshtein scan over the table
(≈ 10 ms for 400k names, only on the zero-hit path). `SymRec` is 40 bytes
(`FORMAT.md`); `sig_len` is not stored (signatures are read from the file
when shown); `line` is the name's line. Per-name symbol lists are ordered
best-first at build time from kind weight, visibility, test flag and file
rank. Extraction covers 16 kinds (the table above plus `field` and
`variant`); locals are suppressed by the query shapes themselves (function-
valued `const`s only at program or export level, Python assignments only at
module and class level) rather than by a `locals.scm`.

### 3.4 Span tables (`spans.bin`)

Per file, three sorted arrays, CSR-indexed from the file table:

| Table | Entry | Purpose |
|---|---|---|
| `defs` | `{start,end,symbol_id}` sorted by start, nested | enclosing symbol of any byte offset by binary search + parent walk; hit kind `def` when the match is inside `[name_start, name_start+len)` |
| `noncode` | `{start,end,kind: comment|string|docstring}` sorted | hit kind `comment`/`string` by binary search |
| `imports` | `{start,end,target: FileId or NONE, raw_off}` | hit kind `import`; edges for the graph |

**As shipped.** The `defs` table is the per-file slice of `symbols.bin`
(no separate copy); `noncode` and `imports` are as described, 8 and 20 bytes
per entry.

`call` and `type-use` are *not* stored as spans (they are the most numerous
nodes and would double the index). They are decided at query time from the
bytes around the match using per-language byte rules (§6.2), which is O(1) per
hit and right for the overwhelming majority of identifier hits. `--precise`
re-parses the shown files with tree-sitter to replace the heuristic with the
real node kinds; this costs 1–10 ms per shown file and is capped by the budget.

### 3.5 Graph (`graph.bin`)

* File import graph as CSR (`out_off`, `out_len`, targets). Edges from resolved
  imports (§7.2) plus symbol references discovered at index time only through
  imports (we do not run a reference pass at index time; that is a query-time
  activity).
* `pagerank: f32` per file, damping 0.85, 20 iterations, computed at the end of
  phase 2 and after every compaction. Quantized to u16 into `files.bin` so the
  query path does not need `graph.bin` unless a verb asks for it.
* **As shipped.** Both directions are stored (out and in CSRs) so `map` can
  print importer counts and `def` can score reachability in either direction.
  Edge weight = 10 × imported names; Kotlin same-package peers add weight-1
  edges (packages of ≤ 30 files). Measured on rust-lang/rust: 108k edges from
  126k imports; resolution + PageRank 73 ms.

## 4. Index construction

### 4.1 Pipeline

```
walk (ignore::WalkParallel, ignore rules honoured, hidden skipped, .git skipped)
  └─▶ per-file stage A (I/O threads, count = min(cores, 4) on macOS, cores on Linux)
        read (pread into thread-local buffer; mmap only if size > 1 MiB)
        classify: binary (NUL in first 8 KiB), language (extension table, then
                  shebang), flags (test/vendored/generated/minified/lockfile, §8)
        content_hash = blake3(bytes)[..16]
        blobcache hit? -> reuse record, skip B and C
        gram extraction (§5.1) -> thread-local HashMap<gram, Vec<FileId>>
  └─▶ per-file stage B (CPU threads, count = cores)
        tree-sitter parse -> tags/locals queries -> symbols, def spans
        highlight query subset -> comment/string spans
        import extraction (query + language resolver) -> import list
        write record to blobcache
  └─▶ merge
        gram maps: per-thread maps merged by key; posting vectors sorted;
        roaring bitmaps built and serialized
        symbols concatenated in file order; names/tokens FSTs built from a
        sorted unique name list
        graph resolved (module path -> FileId) and PageRank computed
  └─▶ publish: write *.bin to temp names, fsync, rename, update manifest
```

Phase 1 = walk + stage A + gram merge + file table publish. Phase 2 = stage B +
symbols/spans/graph publish. Queries between the two phases use grams for
candidate pruning and fall back to regex-based definition detection (§7.5) for
kinds.

**As shipped (M3).** Phase 1 publishes first (2.1 s on the 66k-file trees),
then phase 2 re-reads the files with a grammar (page-cached) on a pool of
all cores, parses, runs the tags query, resolves imports, computes PageRank,
and republishes `files.bin` with ranks and `PARSE_ERRORS` flags together
with the three new components. Phase 2 is 3.8 s on rust-lang/rust (38,848
files, 136 MB; 7–10 MB/s per core), 1.3 s on TypeScript, 0.3 s on django.
Delta segments run both stages inline for the changed files and resolve
their imports against the base (§4.3).

Two thread pools are deliberate: file reading on macOS degrades past 4 threads
(measured: 1 thread 1.27 s, 4 threads 0.92 s, 12 threads 2–3 s on 66k files),
while parsing is CPU-bound and scales to all cores. Stage A hands parsed bytes
to stage B through a bounded channel (capacity 2 × cores) so memory stays
bounded at roughly `2 × cores × max file size`.

### 4.2 Memory during build

Per-thread gram maps use `hashbrown` with `u32` keys (trigrams pack into 24
bits; sparse grams hash to u64) and `Vec<u32>` values grown geometrically. For
a 450 MB tree the merged map holds ≈ 200k–500k distinct keys and ≈ 100–200 M
(gram, file) pairs before compression: about 0.6–1 GB transient. If the
estimated pair count exceeds a limit (default 400 M) the build switches to
*bucketed* mode: grams are partitioned by top bits into 16 buckets processed
sequentially, each bucket writing its own posting range. This keeps peak RSS
under 1.5 GB on any tree at the cost of re-reading files per bucket (reads are
cached by the OS after the first pass).

### 4.3 Incremental updates

Changed files (from the freshness check, §4.5) are re-extracted through stages
A and B and written to a *delta segment* `delta/NNNN.bin`, a small
self-contained index of only those files with the same component layouts. The
file ids of superseded versions are added to `tombstones`. A query evaluates:

```
candidates = (base_postings(q) \ tombstones) ∪ delta_postings(q)
```

Deleted files are tombstoned only. New files get fresh ids at the end of the id
space (which breaks the sorted-path range property for them; path filters fall
back to a per-file glob check for delta files, which are few).

**Graph freshness (as shipped).** The files an agent edits are exactly the
ones it asks about next, so a delta must not drop them out of the import
graph. A delta records, per file, the id it supersedes and its resolved
outgoing edges (imports resolved against the live base plus the delta's own
files; `FORMAT.md`), and a modified file keeps the rank of the version it
replaces. Queries see one graph: an edge from a base file to a superseded id
follows the file to its newest live version, and the importers of a file are
the base importers of its oldest version that were not themselves edited
(their edited versions carry their own edges) plus the delta files with an
edge to any version of it. PageRank itself is recomputed only by a build.
Kotlin imports in a delta resolve against the base by directory suffix
(`import a.b.C` → a live Kotlin file under `…/a/b/` declaring `C`), since the
base does not store packages. Cost: ≈ 2 ms per delta on rust-lang/rust (a
path map over 62k files and 384 `Cargo.toml` reads on four threads), nothing
measurable on tokio.

Compaction: when `|delta files| > max(200, 5 % of files)` or `|delta segments| >
16`, a background `greeg index --compact` rebuilds base components from base
minus tombstones plus deltas without re-reading unchanged files (records come
from the blob cache).

### 4.4 Blob cache

`blobcache/<hash[..2]>/<hash>`: the per-file extraction record (grams as a
sorted `Vec<u32>`, symbols, spans, imports) serialized with `postcard`,
`zstd`-compressed at level 1. Hit rate is high on branch switches and on
worktrees of the same repo. Bounded to 2 GB by LRU on access time. When git is
present, the git blob oid is recorded too so that `git checkout` of a known
blob needs no hashing: the freshness check compares the git index entry's oid
with `files.git_oid` before hashing content.

### 4.5 Freshness

Goal: know exactly which files changed since the index was published, in tens
of milliseconds, without a daemon.

Modes (`--fresh=auto|watch|stat|none`):

1. **watch**: `greeg watch` heartbeat younger than 2 s → changed set comes from
   its journal. Cost ≈ 0.
2. **fsevents (macOS, part of auto)**: the manifest stores the FSEvents stream
   event id at publish time. The query process opens a stream with
   `sinceWhen = stored id`, `kFSEventStreamCreateFlagNoDefer`, drains it
   synchronously, and gets the set of directories with changes since then from
   the kernel's persistent `/.fseventsd` log. Only those directories are
   re-scanned (`readdir` + `lstat`). Measured 12–14 ms for a 200-file,
   76-directory change set including `HistoryDone` (S2). FSEvents never
   signals an unknown or purged id, it simply never completes, so the drain
   has a 40 ms cutoff (150 ms until v0.2); on cutoff, drop or wrap flags the
   check falls through to stat mode. The stored id is refreshed on every successful check.
3. **stat (Linux default, macOS fallback)**: parallel `lstat` of every known
   file (4 threads) comparing `(size, mtime_ns, inode)`, then `lstat` of every
   known directory comparing `mtime_ns` (APFS and ext4 update it on entry add,
   remove and rename); only directories whose mtime moved are re-listed to
   find additions, deletions and renames. `.gitignore`/`.ignore` files whose
   mtime changed force a re-evaluation of ignore rules for their subtree.
   Measured 41 ms for 66k file stats at 4 threads; directory stats add ≈ 3 ms
   (S2). Hashing directory listings instead was measured at 32–57 ms and
   rejected.
4. **none**: trust the index. Used by benchmarks and by agents that just ran
   `greeg` a moment ago; `auto` also picks `none` when the manifest was
   published or verified less than `--fresh-ttl` (default 300 ms) ago, which
   makes parallel tool calls from one agent turn cost one check, not N.

Changed files are re-extracted inline if `count ≤ 64` (typical agent edit
burst; ≈ 5–100 ms), otherwise grams-only inline (`count ≤ 2000`) with symbol
extraction deferred to a detached background process, otherwise the query runs
in scan mode for the changed subset while a full re-index is spawned. In every
case the answer reflects the working tree at query time.

Git awareness: if `.git/HEAD` or the index file changed, the freshness check
reads the git index (via `gix`) to get oids for tracked files and uses the blob
cache for any oid it has seen; a branch switch over 10k files then costs
milliseconds per known blob instead of a re-parse.

## 5. Query planning: from regex to candidates

### 5.1 Gram extraction (index side)

Trigram mode: for each byte offset `i` in the case-folded buffer emit
`key = b[i] << 16 | b[i+1] << 8 | b[i+2]` unless any of the three bytes is
`\n`. Dedup per file with a 16 M-bit thread-local bitset (2 MiB) cleared per
file by tracking touched words, which is faster than a hash set for typical
files. Line terminators are excluded so that grams never span lines, matching
grep's line-oriented semantics (multiline mode, `-U`, falls back to
line-agnostic verification but keeps the same candidate set, which is a
superset).

Throughput target ≥ 300 MB/s per core: the loop is branch-light and the bitset
fits in L2.

### 5.2 Regex analysis (query side)

1. Parse the pattern with `regex-syntax` into HIR, with the flags implied by
   `-i`, `-w`, `-F`, `-x`.
2. Compute the *required gram query* with Cox's algorithm over the HIR:
   for each node maintain `(emptyable, exact: Option<Set<String>>, prefix:
   Set<String>, suffix: Set<String>, match: Query)` where `Query` is an AND/OR
   tree of gram keys. Concatenation combines exact sets by cross product with a
   limit (`|exact| ≤ 32`, strings truncated to 16 bytes) before degrading to
   prefix/suffix information; alternation unions; repetition of ≥1 keeps
   prefix/suffix; classes expand when small (≤ 8 members) else become
   `emptyable=false, exact=None`.
3. Case folding: the analysis runs on the case-folded pattern so `-i` costs
   nothing; the real matcher retains the exact flags.
4. Simplify the query tree: drop AND-branches whose gram doc-count exceeds
   `0.5 × total files` (they prune nothing and cost an intersection), keep the
   `k` rarest grams per AND (k = 6) which captures nearly all selectivity,
   flatten nested ORs, and if the result is `ANY` (no usable grams, e.g.
   `\w{5}\s+\w{5}` or a 1–2 character literal) plan a full scan over the file
   table.
5. Evaluate: `AND` = roaring intersection in ascending doc-count order with
   early exit on empty; `OR` = union. Apply the path filter as `FileId`
   range(s), the type filter as a precomputed per-language bitmap, and the flag
   filters (`--no-tests`, `--no-vendored`) as bitmaps stored in `files.bin`.
   Subtract tombstones; union delta results.

Cost: the whole plan for an identifier is a few dictionary lookups (binary
search or direct table) and a handful of bitmap intersections: measured
6–48 µs (S1). Keeping the six rarest grams costs at most 35 % more
candidates than using all of them. Trigram false positives are 3–10 × on
long identifiers (219 candidates for 19 true files for `createSourceFile` on
the 66k-file tree), which verification absorbs in ≈ 15 ms; §5.3 exists to cut
that.

### 5.3 Sparse grams (format v2)

Definition. Let `w(x, y) ∈ [0, 255]` be a weight for the byte bigram `xy`,
higher for rarer bigrams, from a table trained on a multi-language corpus and
embedded in the binary (64 KiB). For content `b[0..n)`, the gram `b[i..j]`
(inclusive, `j − i ≥ 2`, `j − i ≤ 7`) is *selected* iff

```
min(w(b[i], b[i+1]), w(b[j-1], b[j])) > max over i < k < j-1 of w(b[k], b[k+1])
```

i.e. both boundary bigrams are strictly heavier than every interior bigram.
Because the rule depends only on bytes inside the gram, every selected gram of
a query string is also a selected gram of any document containing that string,
which is the covering property needed for correctness. To guarantee that every
query of length ≥ 3 yields at least one gram, positions where no gram is
selected fall back to the plain trigram at that position, both at index and at
query time (the query-side analysis applies the same rule to each literal).

Why it was expected to help: common substrings such as `ion`, `for`, `ent`
sit inside heavier boundaries and would never be emitted on their own, so
their giant posting lists would disappear; identifiers produce a few long,
rare grams. GitHub reports the scheme at index ≤ 1× raw content with
positions; Cursor adopted it client-side.

**As measured (M4): rejected.** The rule as stated selects every trigram
(a trigram has no interior bigram), so it is a superset of the trigram
index: 2.0–2.3× the (gram, file) pairs and ≈ 3× the postings bytes on every
corpus, at 39 MB/s per core extraction instead of 106. Variants that drop
trigrams (minimum length 4, or minimizers over k-mers, the only local rule
with a bounded gap) cannot answer 3–6-byte literals without a full scan
and still do not reduce pairs, because long grams almost never repeat
within a file while the trigram vocabulary saturates at ≈ 0.1–0.15 pairs
per byte. Candidate reductions of 2–4× on identifiers are real but worth
≈ 1–3 ms per query at current verification cost. Full tables in
`PLAN.md` M4; harness in `crates/greeg-index/examples/sparse_ab.rs`. Format
v2 keeps trigrams and no opt-in ships.

### 5.4 Scan mode

Used when there is no index, when the plan is `ANY` and the tree is small, or
with `--no-index`. It is ripgrep's own pipeline (`ignore` walker,
`grep-searcher`, `grep-regex`) with three changes: adaptive thread count for the
read phase (`min(cores, 4)` on macOS, measured 3× faster than the default on
66k small files), the same output shaping as indexed mode, and definition
detection through per-language regexes (§7.5) instead of span tables.

Implementation notes from M1: searching runs inside the walker's threads
(files are searched as they are discovered, as ripgrep does; walking first
and searching second cost 3–6 % on small trees), read buffers are reused per
thread, and classification is two-stage. During the scan each hit is
classified from its own line only (line-local lexer, definition regex on the
line, byte-context rules), which costs microseconds per hit and is exact for
definitions, calls and imports but misses multi-line block comments and
docstrings. After ranking and budget selection, only the files that will be
shown are read again and fully outlined (`refine`), which fixes those kinds
and attaches enclosing chains. Measured cost of classifying every matched
file up front was 7–11 ms and 2–3× ripgrep's user CPU; the two-stage form
brings scan mode within the M1 gate.

Two further measured lessons from M1 that apply to every regex on the hot
path: build the per-language definition regexes with Unicode disabled
(`\w`, `\s` ASCII-only), because Unicode classes made the lazy DFA spend
14 µs per previously unseen line against 1 µs in ASCII mode; and run them
per line after a keyword prefilter (`memmem` for `fn `, `class `, ...)
rather than with `captures_iter` over a whole buffer, which is what makes
the full outline of a shown file cost about 1 ms. Cloning regexes per thread
made no measurable difference and was removed.

M2 added two more: the definition prefilter must be a first-token check
(definitions start with a keyword or modifier; JS/TS methods with an
identifier then `(` and a `{` on the line), not a set of `memmem` searches
per line, which cost 10 finder constructions per line and 5 ms per 500 KB
file; and `refine` lexes and outlines only a window around the shown hits
(512 KiB before the first, 64 KiB after the last), accepting that a block
comment spanning the window start or an enclosing class that begins earlier
is missed in files above that size.

## 6. Verification, classification and ranking

### 6.1 Verification pass

Candidates are processed in *prior order* (§6.3) by a pool of
`min(cores, 4)` reader threads (I/O-bound) so that the best files are
verified first and the budget can cut off early.

Per file: `pread` into a reusable buffer (mmap above 1 MiB), then
`grep_searcher::Searcher::search_slice` with a `Sink` that records
`(line_number, byte_offset, match_ranges)` for the first `N` hits (N = 64) and
only counts beyond that. Line numbers come from `memchr` counting `\n` up to
the offset, incrementally per file. Context lines are extracted later, only for
hits that survive ranking and the budget.

Two-pass behaviour is implicit: the first pass produces counts for every
candidate (needed for facets and the footer) and line details only for the
first N per file; the second "pass" is the extraction of context for the
final set, which touches at most `budget / ~30 tokens` lines.

### 6.2 Hit classification

Given a hit at byte offset `o` in file `f`:

1. `defs[f]` binary search for the innermost def span containing `o` gives the
   enclosing symbol chain (parent pointers) and, if `o` falls inside that
   symbol's name range, kind `def`. Exception: a JS/TS object-literal member
   (`{ watchFile: () => … }`, `{ get(k) {…} }`, symbol flag 8) classifies as
   `member`; it implements a typed member far more often than it defines one
   (scip-typescript records it as a reference to the interface property), and
   `def` ranks it at 0.6. `--precise` applies the same rule from the tree.
2. `noncode[f]` binary search gives `comment` / `string` / `docstring`.
3. `imports[f]` binary search gives `import`.
4. Otherwise a byte-context rule decides: skip whitespace after the match; `(`
   → `call` (Kotlin/Rust also `{` after an identifier for trailing-lambda and
   struct-literal forms); preceded by `new ` → `call`; preceded by `: `,
   `-> `, `<`, `impl `, `extends `, `implements `, `is `, `as ` or followed by
   `<` → `type`; preceded by `.` → `member`; else `ident`.

All lookups are O(log n) on small per-file arrays; there is no parsing on the
query path unless `--precise`.

### 6.3 Ranking

Each hit `h` in file `f` gets

```
score(h) = kind_w[h.kind]
         × loc_w[f.flags]            (source 1.0, test 0.45, generated 0.2,
                                      vendored 0.2, minified 0.1)
         × (0.6 + 0.4 × pagerank_norm[f])
         × near_w(f, focus)          (1.0 same dir as a focus file, 0.85 sibling
                                      dir, 0.7 otherwise; focus = --near paths
                                      or files seen in this session)
         × recency_w(f)              (1.0 modified < 1 day, 0.95 < 30 days,
                                      0.9 otherwise; from mtime)
         × exact_w                   (1.15 when the match equals the query as a
                                      whole word with exact case)
kind_w:  def 1.0, import 0.6, call 0.55, type 0.5, member 0.45, ident 0.4,
         docstring 0.3, comment 0.25, string 0.25
```

The file *prior* used to order verification is
`loc_w × (0.6 + 0.4 × pagerank_norm) × near_w × recency_w`, computable from
`files.bin` alone. Within a file, hits are ordered by score then line. Across
files, a per-file cap (default 4 shown hits, the rest summarized as
`+N more in this file`) prevents one dense file from filling the budget.
Symbols with many definitions (overloads, overrides) are grouped by the
symbol's name and kind.

PageRank is computed over the file import graph with edge weights = number of
imported names. The personalized variant (restart mass on focus files) is used
only when a focus set exists, in the query process, over the CSR in
`graph.bin`; at 60k nodes and ~300k edges, 10 iterations take a few
milliseconds.


**As shipped (v0.2).** `kind_w` is def 1.0, call 0.6, type 0.5, import 0.45,
member 0.45, ident 0.4, docstring 0.3, comment 0.25, string 0.25; a
definition whose symbol name equals the query (whole word, exact case) gets
× 1.3 (and from M9 a bare identifier query keeps only whole-word matches
while any exist, so `exact_w` decides ordering only under `-i`, `-F` and
regex queries); files under `mock(s)`, `__mocks__`, `stub(s)`, `fake(s)` are demoted
to 0.45 like tests; `recency_w` was removed (every file in a fresh clone is
"today"); the per-file cap is budget-driven when at most three files match
or a single file is named.

### 6.4 Budget and shaping

Token estimate: `ceil(bytes / 3.6)` for code lines and `ceil(bytes / 4.2)` for
paths, calibrated against `o200k_base` on the corpora (target error < 10 %).
`--budget-exact` switches to `tiktoken-rs` for the final output only (adds
1–2 ms).

Shaping algorithm:

1. If total hits ≤ budget-equivalent lines: emit content mode with adaptive
   context (`1 hit → 40 lines, ≤ 3 → 12, ≤ 10 → 4, else 0` around each hit,
   clipped to the enclosing symbol's range).

   *As shipped (M9, unreleased).* The `≤ 10 → 4` rung is gone: from four hits on, the
   answer is hit lines only. The context lines around a hit are the largest
   block of an answer that carries no location of its own, and with several
   hits on the page the reader can already triangulate; measured on the SCIP
   oracle they cost more lines than they earn (PLAN.md M9). A bare identifier
   query also answers about the whole word only: hits inside longer
   identifiers collapse to the `related` line (OUTPUT.md).
2. Else emit *facets first* (§10.3): counts by kind, by top-level directory
   (top 6), by language, by flag, plus the top definitions and the top 10 hits
   by score, all within the budget.
3. Lines longer than `--max-columns` (default 200) are clipped around the match
   with a `…` marker; files flagged `MINIFIED` are never expanded, only counted.
4. The footer always states: hits shown / total, files shown / total, files
   skipped by flag with counts, whether the index was fresh, the escalation
   rung used, and one next-step hint chosen by a small rule table.

## 7. Symbols, references and resolution

### 7.1 Extraction (index time)

Per language: a tree-sitter grammar, a `tags.scm` with `@definition.<kind>`,
`@name`, `@doc` captures extended with `@scope` for container nesting and
`@supertype` captures, a `locals.scm` to suppress local variables, and a
`noncode.scm` (comments, strings, docstrings). Queries are ours, modelled on
Sourcegraph's `scip-tags.scm` shape, not the upstream `tags.scm` files, which
are too coarse (upstream Python has no method/field distinction).

Grammar set: `tree-sitter-python` 0.25, `tree-sitter-rust` 0.24.2,
`tree-sitter-javascript` 0.25, `tree-sitter-typescript` 0.23.2 (both `typescript`
and `tsx` parsers, queries shared), `tree-sitter-kotlin-sg` 0.4.1 (ast-grep's
maintained fork of fwcd). Runtime `tree-sitter` 0.27, pinned exactly.

Parse budget per file: 200 ms timeout via the progress callback; on timeout or
when `ERROR` nodes cover > 20 % of the file the file is flagged
`ERRORS_IN_PARSE` and the regex extractor (§7.5) supplies definitions.
Measured on ktor and kotlinx.coroutines (S3): 1.6 % and 0.9 % of files carry
any error, 9 of 2,572 ktor files exceed the 20 % threshold, all of them using
Kotlin 2.2 named context parameters (`context(ctx: T)`), which the grammar
does not parse yet; draft tag coverage of top-level and member declarations
is 92–98 %.

**As shipped (M3).** One query file per language (`crates/greeg-lang/queries/*.scm`)
with captures `@def.<kind>`, `@name`, `@supers`, `@import`, `@package`,
`@noncode.*`; supertype lists are captured as text and split into
identifiers (`extends A<T> implements B, C`, `: Send + Sync`, `Base(),
I by x`, `(Base, metaclass=M)`). Doc comments are detected by prefix
(`///`, `//!`, `/**`) and Python docstrings by position. Two Rust-specific
additions were necessary in practice: items wrapped in brace-bodied macro
invocations (`cfg_rt! { pub struct JoinHandle … }`, tokio's whole public
surface) are re-parsed as items with shifted offsets, two levels deep; and
`impl Trait for (A, B)` is named by the trait. Kotlin `interface`/`enum
class` are distinguished by the declaration head (the grammar folds both
into `class_declaration`); unnamed companion objects are named
`companion`; secondary constructors `constructor`. Agreement with the regex
extractor on definition names and lines: Python 100 %, Rust 98–99 %,
TypeScript 99 %, Kotlin 94–97 % (`extract_bench --agree`). Fallback rates:
0.4 % ktor, 1.5 % rust-lang/rust.

### 7.2 Import resolution (index time)

Per-language resolvers map an import statement to a `FileId`:

| Language | Rule |
|---|---|
| Python | `from a.b import c` / `import a.b` → `a/b.py`, `a/b/__init__.py`, relative dots resolved from the importing file's package; search roots: repo root, any dir containing `pyproject.toml`/`setup.py`, `src/` |
| TypeScript/JavaScript | `from './x'` → `x.ts .tsx .js .jsx .mjs .cjs d.ts`, `x/index.*`; the nearest `tsconfig.json`/`jsconfig.json` (with its `extends` chain) supplies `paths` (longest matching pattern first) and `baseUrl`; workspace packages named by the root `package.json` `workspaces` or `pnpm-workspace.yaml` resolve by `name[/sub]`; other package-name imports unresolved (external) |
| Rust | `use crate::a::b` → `src/a/b.rs`, `src/a/b/mod.rs`, `src/a.rs` containing `mod b`; `mod x;` declarations; workspace crates by name from `Cargo.toml` members |
| Kotlin | `import a.b.C` → any file whose `package a.b` declares `C` (symbol table lookup); same-package implicit visibility → edge to every file with the same package (weighted 0.1) |
| Java (later) | as Kotlin, plus `import static` |

Unresolved imports keep their raw text for `--external` queries.

**As shipped.** Python, Rust and JS/TS resolvers are path-based as in the
table (Rust also honours `mod x;` declarations and workspace crate names
from `Cargo.toml`, looked up per file by walking the directory chain);
Kotlin uses the package table plus a `package.Name → file` map built from
top-level symbols. JS/TS honours `tsconfig.json` as in the table: on
shadcn-ui/ui (3,875 TS files, 10.3k `@/…` imports against 610 relative ones)
edges went from 631 to 7,723 and phase 2 from 266 to 272 ms. Java is not
implemented.

### 7.3 Definition lookup (`greeg def NAME [--from FILE]`)

1. `names` FST exact lookup → symbol id slice; if empty, ladder (§7.6).
2. Score each candidate: kind compatibility with the request (default any),
   `loc_w`, PageRank of its file, and *import reachability* from `--from`
   (1.0 if the candidate's file is directly imported by `--from`, 0.8 if
   within 2 hops, 0.6 same package/directory, 0.4 otherwise). Without
   `--from`, the session's recently seen files act as the origin.
   **As shipped:** `kind_w × exported(1.0 / 0.85) × nested(0.7 when the
   enclosing symbol is a function or method: a closure is not the
   definition an agent asks for while a top-level one exists) × loc_w ×
   (0.6 + 0.4 × rank) × reach`.
3. Output the signature line, enclosing chain, doc comment first line, and
   caller count (from a cached reference count if computed in this session,
   else `?`).

### 7.4 References, callers, implementations

* `refs NAME`: gram query for the name (word-bounded matcher
  `\bNAME\b`), verification, classification, grouping by kind then file. Ties
  each hit to a candidate definition by the same reachability rule as §7.3 and
  reports the fraction that resolved with confidence ≥ 0.8.
* `callers NAME`: `refs` filtered to kind `call`/`member`, output keyed by the
  enclosing symbol of each hit (so the answer is a list of *functions*, not
  lines), with per-caller hit counts. `--depth 2` repeats for the enclosing
  symbols.
* `impls NAME`: symbols whose `supertype_ids` contain `NAME`'s name id (Rust
  `impl T for X`, Kotlin `: T`, TS `implements`/`extends`, Python bases), plus
  `refs` hits of kind `type` in class headers as low-confidence extras.
* `impact NAME`: `refs` grouped by file with kinds, plus `callers --depth 2`,
  with a `WILL BREAK` (call or type in non-test source) / `MAY BREAK` (member,
  ident) / `REVIEW` (comment, string, test) split.

**As shipped.** `refs` reports the share of hits whose file reaches a
defining file with ≥ 0.8; `callers` keys on the innermost enclosing symbol
and `--depth 2` runs a bounded second `refs` per caller; `impls` adds the
low-confidence type-position extras only on definition lines whose kind is
a container or type alias; `impact` also routes files without a grammar to
REVIEW. Measured: `def` 2.8 ms end to end, `refs` on 367–1,879 hits 4–11 ms,
`callers` 5 ms, `map` 3 ms.

### 7.5 Regex fallback extractor

For files without a grammar, with parse errors, or before phase 2 completes:
per-language anchored regexes for definitions (`^\s*(async\s+)?def\s+(\w+)`,
`^\s*class\s+(\w+)`, `^\s*(pub(\([^)]*\))?\s+)?(fn|struct|enum|trait|type|mod|const|static)\s+(\w+)`,
`^\s*(export\s+)?(default\s+)?(async\s+)?(function\*?|class|interface|type|enum|const|let|var)\s+(\w+)`,
`^\s*(\w+\s+)*(fun|class|object|interface|val|var|typealias)\s+(<[^>]*>\s*)?(\w+\.)?(\w+)`),
evaluated with `regex` multi-pattern sets on line starts. Comment/string spans
in this mode come from a byte-level lexer that understands each language's
comment and string delimiters (including Python triple quotes, Rust raw
strings, Kotlin raw strings and `${}` interpolation, JS template literals).

### 7.6 Escalation ladder

On zero verified hits, retry in order and report the rung: (1) drop `-w`,
(2) `-i`, (3) split-token search via the `tokens` FST (query split on
case/underscore boundaries, all parts required), (4) fuzzy: Levenshtein
automaton (distance 1 for length < 8, else 2) over the `names` FST, (5) the
same query over `IGNORED_BY_GIT` and `HUGE` files reported as a count only.
Each rung is bounded by the same budget and stops at the first rung with hits.
The ladder also runs when hits exist only in `noncode` spans, reporting
"N hits, all in comments/strings".

**As shipped.** Rungs 3 and 4 run only for bare identifiers (or `-F`), use
the split-token and name tables, and rerun the search as a word-bounded
alternation of up to eight names, reported as `matched split tokens →
spawn_blocking, …` or `matched fuzzy → …`. `def` runs its own ladder over
names (case-insensitive, split tokens, fuzzy) and lists the substitutions.
Distance is 1 below 8 characters, else 2; patterns shorter than 4
characters skip the fuzzy rung.

## 8. Adversarial content detection

Computed once per file at extraction (stage A) from the first 64 KiB:

| Flag | Rule |
|---|---|
| `BINARY` | NUL byte in the first 8 KiB |
| `MINIFIED` | `max_line_len > 1000` and `avg_line_len > 200` in the first 64 KiB, or no newline in the first 2 KiB, or a `.min.` in the name / `*.bundle.js`, or a `sourceMappingURL=` directive in the first 64 KiB with `avg_line_len > 120` |
| `GENERATED` | header within first 2 KiB matching `@generated`, `DO NOT EDIT` (any case), `Code generated by`, `autogenerated`, `auto-generated`, `automatically generated`, `This file was/is generated`; or a first line starting with `//// [` (TypeScript compiler baselines: the test source and its emit in one file); or path segment `generated`, `__generated__`, `_gen`, `autogen`, `compiled`, `dist`, `.next`, `target`, `baselines`, `baseline`, `golden` (recorded tool output checked in for comparison); or name `*_pb2.py`, `*.pb.go`, `*.g.dart`, `*.g.cs`, `*.g.ts`, `*.designer.cs`, `*.generated.ts`, or `*.d.ts` under a `generated` directory. `build`, `out` and `gen` are not signals on their own (Go `pkg/build`, `src/gen`). |
| `VENDORED` | path segment `vendor`, `vendored`, `_vendor`, `third_party`, `thirdparty`, `third-party`, `node_modules`, `.yarn`, `bower_components`, `site-packages`. `external` and `deps` are not signals on their own. |
| `LOCKFILE` | `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.lock`, `poetry.lock`, `uv.lock`, `Pipfile.lock`, `gradle.lockfile`, `pixi.lock`, `composer.lock`, `Gemfile.lock`, `bun.lock`, `bun.lockb`, `flake.lock` |
| `TEST` | path segment `test`, `tests`, `__tests__`, `specs`, `testing`, `testdata`, `test_data`, `fixtures`, `snapshots`, `__snapshots__`, `e2e`, `integration-tests`, `mock`, `mocks`, `__mocks__`, `stub`, `stubs`, `fake`, `fakes`; or name matching `test_*`, `*_test.*`, `*.test.*`, `*.spec.*`, `conftest.py`, `*Test.kt`, `*Tests.kt` (uppercase `T`: `Latest.kt` is not a test); or `*_spec.*` under a `spec` directory (`spec/` alone is not a signal: API specs). Segments match case-insensitively. |
| `HUGE` | size > 4 MiB |

Flags are per-file bitsets in `files.bin` so filters are bitmap operations.
Defaults: everything is searched; `MINIFIED`, `LOCKFILE`, `BINARY` are counted
but never expanded; `GENERATED`/`VENDORED`/`TEST` are demoted. `--all` removes
demotion; `--no-tests`, `--no-vendored`, `--no-generated` remove entirely.

## 9. Session memory

Optional, on by default, keyed by `--session ID` or, absent that, by the parent
process id chain (`getppid()` walked to the first non-shell ancestor) so that
all calls from one agent process share a session. Storage:
`session/<id>.jsonl`, one record per query: timestamp, normalized query, mode,
hit counts, shown `(file, line)` set, shown symbol ids, files subsequently
requested via `greeg show`. Files older than 24 h are deleted.

Uses: (a) *dedup*: a block already shown in this session is replaced by
`(shown before: path:line)`; (b) *loop detection*: if ≥ 3 of the last 5
queries normalize (lowercase, split tokens) to the same token set with zero new
hits, emit a short summary with the closest symbol names from the fuzzy rung
instead of repeating results; (c) *focus*: files shown or requested recently
form the `near` set for ranking; (d) *next-step hints*. Reading the log costs
< 1 ms; it is capped at 2,000 records.

**As shipped.** The session id is `--session`, `GREEG_SESSION`, or the pid
of the first non-shell ancestor (`proc_pidinfo` on macOS, `/proc/<pid>/stat`
on Linux), so every call from one agent process shares a file. Records hold
the normalized token set, shown files and the (file, line) pairs whose
context was printed. Dedup replaces an already-shown context with
`(shown before)`; loop detection fires on the third query with the same
token set in the last five and prefixes the hint line; the focus set is the
last twelve shown files and doubles as the `--from` origin for `def` when
none is given. Files older than 24 h are pruned when a new session starts.

## 10. Output

### 10.1 Text (default, model-facing)

```
<kind> <path>:<line>  <enclosing chain> › <signature or line>   <annotations>
```

Grouped by file when a file has more than one shown hit:

```
<path>  [<flags>] <size> <age>
  <line>: <text>            in <enclosing>
```

Facets and footer as in §6.4. Colors only when stdout is a TTY. Paths relative
to the search root, forward slashes.

### 10.2 JSON (`--json`)

ripgrep's JSON Lines schema is preserved exactly (`begin`, `match`, `context`,
`end`, `summary`, `text`/`bytes` encoding) so existing consumers keep working.
`match` records gain:

```json
"kind": "def|call|import|type|member|ident|comment|string|docstring",
"symbol": {"name": "...", "kind": "method", "container": "class Foo", "start": 400, "end": 520, "confidence": 0.9},
"file_flags": ["test"],
"score": 0.73
```

and two new record types: `facets` and `footer` (with the omitted counts and
the escalation rung). `greeg def/refs/callers/impls/outline/map` emit their own
documented record types under `--json`.

### 10.3 Facets record

```
{"type":"facets","data":{"total":2897,"files":423,"by_kind":{...},"by_dir":[["ktor-server",1712],...],
 "by_lang":{...},"by_flag":{"test":722,...},"top_defs":[...],"top_hits":[...]}}
```

## 11. Language support layer

A language is a directory `lang/<name>/` with: `grammar` (crate dependency or a
prebuilt shared object loaded via `libloading` at runtime, ast-grep style),
`tags.scm`, `locals.scm`, `noncode.scm`, `spec.toml` (extensions, shebangs,
comment/string lexer table, definition regexes, test-path patterns, import
resolver name and parameters, kind mapping). The five launch languages are
compiled in; others can be dropped into `~/.config/greeg/lang/` and are picked
up without rebuilding. A `greeg lang check <dir>` command runs the queries over
fixtures and reports capture coverage.

**As shipped (M5).** `spec.toml` carries name, extensions, symbol, comment
and string delimiters and import prefixes; the grammar is a shared object
compiled from the grammar's `parser.c` (`cc -shared -fPIC -O2 -I src
src/parser.c [src/scanner.c]`) and `dlopen`ed on first use after an ABI
version check; `tags.scm` uses greeg's capture names. Extra languages get
symbols, noncode spans and imports (as raw text) at index time, byte-rule
kinds, a generic lexer in scan mode, and `-t <name>`; they have no
definition regexes and no resolver. `greeg lang check DIR` validates and
measures coverage. Verified with tree-sitter-go on 5,105 files.

## 12. Concurrency, memory and I/O rules

* No global thread pool contention: one `rayon` pool for CPU work, one
  fixed-size reader pool for I/O, sizes chosen per platform (§4.1).
* `pread` into thread-local reusable buffers for files ≤ 1 MiB; `mmap` with
  `MADV_SEQUENTIAL` above. Never `mmap` tens of thousands of small files.
* Index files are `mmap`ed read-only with `MADV_RANDOM` for postings and
  `MADV_WILLNEED` for the file table; nothing is deserialized eagerly (`rkyv`
  archived access, `roaring` deserialized lazily per posting list into a
  thread-local arena that is reused across lists).
* Zero allocation per hit in the verification loop: hits go into a
  preallocated `Vec` per file, strings are byte slices into the file buffer
  until final formatting.
* Output is written through a single `BufWriter` of 64 KiB; JSON uses
  `serde_json` with `to_writer` and no intermediate `String`s.
* All hashing is `xxh3` for in-memory tables and `blake3` for content
  identity.
* Panics are converted to a scan-mode fallback with a one-line warning to
  stderr; a corrupt index is deleted and rebuilt, never repaired.
  **As shipped (M5):** `catch_unwind` around the index path, a one-line
  panic hook, unreadable posting lists read as "every file" (superset) and
  mark the index for rebuild, and a SIGBUS handler that `execv`s the same
  command with `--no-index --after-sigbus`. The background build is spawned
  after the answer is written so it never competes with the scan that
  answers the first query.
* Process start is part of every query's latency budget: nothing that costs
  at load time is linked unless every query needs it (system frameworks are
  `dlopen`ed lazily; grammars and regexes compile on first use or on a
  helper thread). Measured: 1.7 ms for `greeg --version` against 1.4 ms for
  a bare clap binary.

## 13. Performance validation

Kernels get `criterion` benchmarks with saved baselines and a 5 % regression
gate in CI: gram extraction (MB/s), posting intersection (ns per list), span
lookup (ns), freshness stat pass (ms per 10k files), verification (MB/s),
formatting (µs per hit). End-to-end numbers come from `greeg bench` (see
`PLAN.md` M6) with `hyperfine` JSON and a 10 % gate against the stored baseline
on the reference machine.

**As shipped (M6).** `crates/greeg-index/benches/kernels.rs` (criterion):
gram extraction and literal keys, the regex planner on eight patterns, the
noncode lexer and the tree-sitter extractor on Rust and Python, roaring
intersection and deserialization. `bench/bench.py gate kernels` compares
`target/criterion/*/new/estimates.json` with `bench/baselines/kernels-<host>.json`
(5 %); `gate speed` compares `bench/results/speed-<host>.json` (greeg's
search means and the index build) with `bench/baselines/speed-<host>.json`
(10 %). Baselines are keyed by OS, architecture and CPU model, so the gates
bind on the reference machine and only record elsewhere. The end-to-end
protocol is `bench/bench.py` (`fetch`, `speed`, `oracle`, `report`) rather
than a `greeg bench` subcommand: it orchestrates git, hyperfine, ripgrep,
grep and the SCIP indexers, none of which belong in the shipped binary.
Results and the rendered `docs/BENCH.md` are in the repository.

## 14. Crate list

| Purpose | Crate | Version |
|---|---|---|
| CLI | `clap` | 4.6 |
| Walk / ignore | `ignore`, `globset` | 0.4.33 / 0.4.20 |
| Match / search | `grep-searcher`, `grep-regex`, `grep-matcher`, `grep-printer` | 0.1.17 / 0.1.14 / 0.1.9 / 0.3.1 |
| Regex analysis | `regex-syntax`, `regex-automata` | 0.8.11 / 0.4.18 |
| Byte search | `memchr`, `aho-corasick`, `bstr` | 2.8 / 1.1 / 1.13 |
| Parsing | `tree-sitter` 0.27 + grammar crates listed in §7.1, `tree-sitter-tags` 0.27 | pinned exactly |
| Postings | `roaring` | 0.11 |
| Archives | `rkyv` 0.8, `postcard` 1.1, `zstd` 0.13 | |
| Dictionaries | `fst` 0.4 (with `levenshtein`) | |
| Storage | `memmap2` 0.9, `redb` 4.2 (manifest and blob-cache index only) | |
| Git | `gix` 0.87 (index and blob oids; feature-minimal) | |
| Hashing | `blake3`, `xxhash-rust` | |
| Threads | `rayon` 1.12, `crossbeam-channel` | |
| Watch | `notify` 8.2, `notify-debouncer-full` | |
| FSEvents since-id | `fsevent-sys` (macOS only) | |
| Tokens | `tiktoken-rs` 0.12 (optional feature `exact-tokens`) | |
| JSON | `serde`, `serde_json` | |
| Optional | `tantivy` 0.26 (feature `concept`), `fastembed` (feature `embed`), `ast-grep-core` (feature `structural`), `scip` 0.9 (feature `scip`) | |

## 15. Decisions log

| Decision | Chosen | Rejected | Reason |
|---|---|---|---|
| Posting granularity | file-level | positional (Zoekt) | must open files anyway to print; 3× smaller; intersections at memory speed |
| Gram scheme v1 | trigram | sparse gram first | proven; sparse gram is an optimization gated on measurement |
| Case handling | fold ASCII at index time | separate case-sensitive index | `-i` free, negligible selectivity loss on code |
| Call/type kinds | query-time byte rules | stored spans | spans for every call would double index; rules are O(1) and accurate on identifiers; `--precise` exists |
| Index location | user cache dir | `.greeg/` in repo | keeps trees clean, survives `git clean`, no gitignore edits |
| Freshness | stat pass + FSEvents since-id + optional watcher | mandatory daemon | zero-setup requirement; measured stat cost is acceptable |
| Cross-file resolution | import-graph heuristics + SCIP import | stack-graphs | stack-graphs archived, no Kotlin/Rust, pinned to tree-sitter 0.24 |
| Reader threads on macOS | 4 | all cores | measured 3× faster on 66k small files |
| Serialization | hand-laid fixed-width tables (bytemuck views over mmap), JSON manifest | rkyv, bincode | zero-copy with no framework churn; layouts documented in FORMAT.md; rkyv 0.8's API and format instability were not worth it for four flat tables |
| Freshness TTL | 100 ms | 300 ms | measured: a script editing and querying within 300 ms saw stale results; agents never edit and search within 100 ms |
| FSEvents vs stat | FSEvents only above 8,000 files | always FSEvents on macOS | stream setup costs ~11 ms regardless of size; the stat pass is 0.8 ms at 849 files |
| Index scope | default ignore rules only; `--no-ignore`/`--hidden` queries scan | indexing ignored files too | keeps the index equal to what ripgrep searches by default; those flags are rare |
| Compaction | full rebuild in the background when deltas ≥ 16 or > 5 % of files are superseded | merging deltas in place | a rebuild is 2.5 s on the largest corpus; merge code is not worth its bugs yet |
| Concept search | optional feature, entity BM25 | built-in embeddings | evidence favours lexical for agents; keeps binary and startup small |
| Name dictionaries | sorted string tables + binary search; bounded Levenshtein scan for fuzzy | `fst` maps with Levenshtein automata | zero dependencies and zero-copy; fuzzy only runs on the zero-hit path where 10 ms is invisible; revisit if profiles disagree |
| Rust macro bodies | re-parse brace-bodied macro invocations that contain item keywords as items | treat macro bodies as opaque (tree-sitter default) | tokio hides its public API inside `cfg_*!`; without this `def JoinHandle` missed the real struct |
| Symbol line | line of the name | line of the declaration node | annotations and decorators start the node lines earlier; agents want the `fun`/`class` line (Kotlin agreement 62 % → 97 %) |
| Delta symbols and edges | extracted inline; imports resolved against the base, rank carried over, edges folded at query time (§4.3) | leave deltas unresolved until the rebuild (v0.3) | the edited files are the agent's working set: `def --from`, `impact` and `map` on them lost every edge until a rebuild that fires only at 16 deltas or 5 % of the tree; the delta-time cost is ≈ 2 ms on the largest corpus |
| Session identity | first non-shell ancestor pid | env-only ids | works with any agent harness without configuration; `--session` still overrides |
| Sparse grams | rejected after the M4 A/B (trigrams stay, no opt-in) | shipping as default or as a flag | every variant is larger (1.3–3.4× postings) and slower to build; the only 2× candidate cut is the superset scheme; verification of trigram false positives costs 1–3 ms |
| FSEvents linkage | `dlopen` CoreFoundation/CoreServices on first use | link the frameworks | loading them cost ≈ 1 ms of every process start (2.3 → 1.7 ms); only large trees use FSEvents |
| SIGBUS handling | re-exec with `--no-index` from the signal handler | sigsetjmp/longjmp, or crashing | execv is async-signal-safe and the re-run answers correctly; longjmp out of a Rust frame is unsound |
| Corrupt posting list | read as "every file", rebuild after | fail the query | verification makes a superset harmless; the user still gets an answer |
| Build spawn timing | after the answer is written | at index-open failure | the build's threads doubled first-query latency on django |
| Extra grammars | `dlopen` of a user-compiled parser + greeg-style `tags.scm` | ast-grep-style dynamic language packs, WASM grammars | no new dependencies; the tree-sitter C ABI is stable; WASM needs a runtime |
| Type filters | ripgrep's `ignore::types` definitions with override-glob precedence | greeg's language table | the M5 soak found `-t js -g '*.py'` and `-g '!*test*'` disagreeing with rg; parity means rg's rules, aliases (`rs`, `kt`) kept |
| Index-path definitions | materialize only the definitions hits reference | all symbols of a matched file | ≈ 100 µs per Rust file, 34 ms of CPU on a 300-file query |
| Phase-2 memory | per-file extracts kept in memory, one pass | streaming merge | ≈ 100 MB transient on rust-lang/rust (430k symbols) is fine on the reference machine; bucketed mode stays deferred with the gram build |
| Benchmark driver | `bench/bench.py` (Python, stdlib + optional tiktoken) | `greeg bench` subcommand | the protocol shells out to git, hyperfine, rg, grep and four SCIP indexers; keeping it out of the binary keeps the 25 MB gate and the dependency list honest |
| Quality oracle | SCIP occurrences decoded by a 60-line protobuf reader | `scip` CLI, `protobuf` package | no extra install; the four fields the scorer needs (path, range, symbol, roles) are stable |
| `impl` headers vs SCIP | reported separately, not counted as classification errors | dropping `impl` from the definition kinds | SCIP marks `impl Foo` a reference to `Foo`; agents asking `def Foo` want the impl blocks listed after the struct |
| UTF-16 sources | transcode files that start with a UTF-16 byte-order mark at every read | treat them as binary (NUL bytes) | ripgrep's default `--encoding auto` searches them; the M6 speed run's match-count verification found one such file per TypeScript corpus that the soak's query mix never hit |
| Regression gates on hosted CI | record-only unless a baseline for the host key exists | absolute thresholds | runner speed varies by 2×; the reference machine's nightly is the binding run |
| Recency weight | dropped | mtime-based 1.0/0.95/0.9 | every file in a fresh clone has today's mtime; git commit time would cost a `git log` per file |
| Kind weights | call 0.6 > import 0.45 | import 0.6 > call 0.55 | `use …;` lines filled "top hits" and tell an agent nothing about usage |
| `-l` / `-c` output | rg-shaped on stdout, footer on stderr, never budget-truncated | budgeted list with footer on stdout | the hook rewrites `rg -l … \| xargs …`; anything but bare paths breaks the pipe |
| Ignore files | tracked in the file table (format v3), never searched; an edit forces a rebuild and a scan-mode answer | re-evaluating rules per subtree inline | the walker skipped dotfiles so edits were invisible; a rebuild is 0.2–6 s and the scan answer is correct meanwhile |
| Writer coordination | exclusive `flock` on `LOCK`, manifest written last, re-read before delta apply, deltas loaded only as the manifest names them | lock-free writers | measured races: phase 2 reverted by a query's stale manifest, duplicate delta names from parallel tool calls |
| Text layout | group by file, path once, last container, demoted definitions collapsed, definitions above facets | `path:line` per hit with full chain and size/age header | paths and chains were 48 % of a default answer and test definitions 32 %; same hits at 35–60 % fewer tokens |
| Speed protocol | `--prepare "sleep 0.15"`, medians, headline vs `rg -j4`, `fresh` column | back-to-back hyperfine means vs default `rg` | the 100 ms TTL hid the freshness check and default `rg` is 3.7× slower than `rg -j4` on macOS |
| Oracle sample | usage-weighted names with ≥ 3 references, rg definition-regex baseline, intervals | uniform over SCIP definitions, `rg -nw` first lines | `return5`/`mInt`-style names never exercise the budget; nobody searches for a definition with `-nw` |
| FSEvents cutoff | 40 ms, then the stat pass | 150 ms | the daemon intermittently takes 135 ms+ to replay a `sinceWhen` stream (≈ 1 run in 10–30 on the reference machine); a 74k-file stat pass is 41 ms, so the bounded worst case is ≈ 80 ms instead of 150–210 ms |
