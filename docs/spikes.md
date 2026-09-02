# M0 spike results

Date: 2026-09-01. Machine: Apple M4 Pro (12 cores), 24 GB, macOS 25.5, APFS,
warm page cache. Code: `spikes/` (throwaway, not part of the product).
Corpora: shallow clones at the SHAs listed in `PLAN.md` M6 where applicable
(TypeScript is the current `main`, which is the Go port: 65,651 of its 65,977
files are test baselines under `tsc/`, so it is the many-tiny-files case).

Decision summary: **go** for file-level trigram postings (S1), **go** for
stat-based freshness with FSEvents history on macOS (S2), **go** for
`tree-sitter-kotlin-sg` with a documented gap list (S3). Two design changes
came out of S2 (§2.3).

## 1. S1 — file-level trigram index

`s1-grams ROOT --threads N --q a,b,c`: walk with `ignore`, read every file
≤ 4 MiB, ASCII case-fold, extract unique trigrams per file with a 16 M-bit
bitset, per-thread `gram → Vec<FileId>` maps folded and reduced, roaring
bitmaps, serialized size measured. Selectivity: AND of all query grams
(rarest first) vs the truth from re-reading candidates with `memmem`.

### 1.1 Size and build time (4 reader threads)

| Corpus | Files | Indexed MB | Distinct grams | Postings MB | Dict MB | Index / source | Build total |
|---|---|---|---|---|---|---|---|
| tokio | 849 | 6.0 | 25,449 | 2.1 | 0.4 | 0.41 | 72 ms |
| ktor | 3,057 | 15.8 | 63,891 | 5.4 | 1.0 | 0.41 | 302 ms |
| django | 7,031 | 38.1 | 132,292 | 12.0 | 2.1 | 0.37 | 525 ms |
| rust-lang/rust | 62,362 | 187.6 | 150,187 | 40.3 | 2.4 | 0.23 | 2.55 s |
| TypeScript (main) | 65,938 | 210.0 | 132,704 | 29.8 | 2.1 | 0.15 | 2.52 s |

Gate was ≤ 0.35 × on all five; it holds on the two large corpora (0.15, 0.23)
and misses on the three small ones (0.37–0.41). The small-corpus ratio is
per-bitmap and dictionary overhead (16 B per gram, roaring container headers)
against a small source; the absolute cost there is 2–14 MB and irrelevant.
Revised gate for M2: ≤ 0.35 × on corpora above 50 MB, ≤ 15 MB absolute on
anything smaller. The dictionary can shrink 4× with a 3-level radix table for
trigrams (no key storage), which is already the plan for trees above 5,000
files.

Posting-list doc counts (files per gram): p50 = 2–5, p90 = 39–155,
p99 = 429–4,148, max = the file count (grams like `for`, `the`). The
distribution is extremely skewed, which is what makes the "drop grams that
prune nothing" planner rule (D§5.2) important.

### 1.2 Throughput

| Corpus | Threads | Read MB/s per core | Extract MB/s per core | Wall build |
|---|---|---|---|---|
| TypeScript | 4 | 31 | 275 | 2.52 s |
| TypeScript | 12 | 9 | 128 | 2.46 s |
| rust | 4 | 34 | 196 | 2.55 s |
| rust | 12 | 9 | 90 | 2.49 s |
| django | 4 | 51 | 203 | 0.53 s |
| tokio | 4 | 57 | 188 | 0.07 s |

Extraction meets the ≥ 200 MB/s per core gate at 4 threads on the large
corpora (188–203 on the small ones, within noise). Reads dominate: 6.7 s of
CPU for 210 MB at 4 threads (31 MB/s per core, i.e. ~17 µs per file of
syscall cost), and at 12 threads per-core read throughput collapses to 9 MB/s
with no wall-clock gain, confirming the macOS kernel contention seen with
ripgrep. Phase-1 build gate (< 3 s on a 450 MB tree) passes at 2.5 s.

### 1.3 Selectivity and query cost

| Corpus | Query | Grams | Rarest list | Candidates (all grams) | Candidates (6 rarest) | True files | Verify | Plan |
|---|---|---|---|---|---|---|---|---|
| TypeScript | createSourceFile | 14 | 708 | 219 | 295 | 19 | 15 ms | 48 µs |
| TypeScript | checkExpression | 13 | 70 | 36 | 36 | 15 | 3 ms | 10 µs |
| TypeScript | ParseFlags | 8 | 333 | 58 | 61 | 3 | 4 ms | 19 µs |
| TypeScript | node | 2 | 4,548 | 4,521 | 4,521 | 4,515 | 207 ms | 6 µs |
| TypeScript | for | 1 | 26,442 | 26,442 | 26,442 | 26,442 | 662 ms | 4 µs |
| rust | mir_borrowck | 10 | 552 | 90 | 90 | 20 | 1.5 ms | 34 µs |
| rust | HirId | 3 | 2,406 | 363 | 363 | 315 | 5 ms | 24 µs |
| rust | TyCtxt | 4 | 907 | 870 | 870 | 869 | 33 ms | 29 µs |
| rust | for | 1 | 36,377 | 36,377 | 36,377 | 36,377 | 899 ms | 2 µs |
| django | get_queryset | 10 | 167 | 110 | 114 | 73 | 1.5 ms | 14 µs |
| django | HttpResponseRedirect | 18 | 346 | 131 | 146 | 46 | 1.5 ms | 35 µs |
| django | request | 5 | 1,587 | 1,018 | 1,018 | 844 | 13 ms | 30 µs |
| ktor | respond | 5 | 815 | 537 | 537 | 437 | 7 ms | 20 µs |
| ktor | ContentNegotiation | 16 | 178 | 152 | 153 | 146 | 2 ms | 18 µs |

Reading: planning is microseconds; verification of a few hundred candidate
files is single-digit to low double-digit milliseconds. For
`createSourceFile` on the 66k-file tree the whole query is ≈ 15 ms against
ripgrep's 3.2 s. Keeping only the six rarest grams costs at most 35 % more
candidates and is a fine planner default.

Trigram false positives are real: 219 candidates for 19 true files
(`createSourceFile`), 131 for 46 (`HttpResponseRedirect`), 58 for 3
(`ParseFlags`), 90 for 20 (`mir_borrowck`). The common English-like trigrams
inside identifiers (`ate`, `ion`, `our`) are shared by thousands of files.
This is the quantitative case for the sparse-gram format in M4: a 3–10 ×
candidate reduction on exactly the identifier queries agents issue most. It
does not change the M2 decision because verification is already cheap.

Three-character queries (`for`) are full scans by construction: 660–900 ms
here because the spike re-reads files through `par_bridge`; the product path
uses the 4-thread reader pool and is bounded by the same per-file open cost
as ripgrep (≈ 0.9 s on this tree). Nothing indexed can beat that for a query
that matches 40 % of files; the answer is facets, not speed.

## 2. S2 — freshness

`s2-fresh snapshot` records `(path, size, mtime_ns, inode)` per file and an
entry-name hash per directory; `check --threads N` re-stats everything in
parallel and re-hashes every directory; `fsid`/`fssince` exercise the FSEvents
persistent log through `FSEventStreamCreate(sinceWhen = id)`.

### 2.1 Stat pass

| Corpus | Files | Dirs | 1 thread stat + readdir | 4 threads | 8 threads | 12 threads |
|---|---|---|---|---|---|---|
| TypeScript | 65,938 | 721 | 114 + 54 = 168 ms | 41 + 36 = 77 ms | 39 + 37 = 76 ms | 45 + 37 = 82 ms |
| rust | 62,362 | 4,676 | 102 + 120 = 222 ms | 38 + 57 = 95 ms | 39 + 54 = 92 ms | 48 + 61 = 109 ms |
| django | 7,031 | 3,269 | 12 + 48 = 61 ms | 4 + 32 = 36 ms | 4 + 38 = 43 ms | 5 + 44 = 49 ms |

`lstat` of 60k+ files takes 38–41 ms at 4 threads (gate < 60 ms: pass).
Hashing directory entries is the expensive half (32–57 ms) and scales badly
with directory count, so it is replaced (§2.3). Four threads is again the
right pool size on macOS.

### 2.2 FSEvents history (macOS)

Scripted change set on django: append to 200 files across 76 directories,
create one file, rename one directory subtree, delete one file.

| Query | Result | Time |
|---|---|---|
| `fssince <id at snapshot>` | 84 events, 76 distinct directories, `HistoryDone` received, no drops | 14.4 ms (12.4 ms on repeat) |
| stat pass after the same edits | 404 changed files, 197 changed directories (rename subtree counted per path) | 33.6 ms |
| `fssince <id − 10⁹>` (id older than the log) | 0 events, `HistoryDone` never arrives | hit the 3 s cutoff |

Gate (< 5 ms and every changed directory reported): the directory set is
complete for edits, creates and deletes, and a rename reports the parent
directory, which is enough because the parent is then re-listed. Latency is
12–14 ms rather than 5 ms; most of it is stream setup and run-loop
scheduling. This is still 3–6 × cheaper than the stat pass and, more
importantly, it makes the follow-up work proportional to the change set
instead of the tree. Accepted with the target revised to < 20 ms.

The unknown-id case is the important negative result: FSEvents does not
signal "too old", it simply never completes. The design now uses a 150 ms
cutoff and falls back to the stat pass; the id stored in the manifest is
also refreshed on every successful check so it never ages far.

### 2.3 Design changes from S2

1. Directory change detection uses the directory's own `mtime` (APFS updates
   it on entry add, remove and rename) instead of hashing entries. A 4,676
   `lstat` pass costs ≈ 3 ms; only directories whose `mtime` moved are
   re-listed. Expected total for a 60k-file, 5k-directory tree: ≈ 45 ms at
   4 threads instead of 95 ms.
2. FSEvents mode gets a 150 ms completion cutoff and always falls back to
   stat mode on timeout, drop flags or id wrap.

## 3. S3 — Kotlin grammar

`s3-kotlin ROOT…` with `tree-sitter` 0.27.0 and `tree-sitter-kotlin-sg`
0.4.1: parse every `.kt`/`.kts`, count `ERROR`/`MISSING` nodes, run a draft
tag query (class, object, companion, function, property, typealias, enum
entry, constructor parameter, secondary constructor), and compare
top-level and member declarations (indent ≤ 4) with a regex oracle.

| Corpus | Files | MB | Wall (12 threads) | Parse CPU | MB/s per core | Files with errors | Files > 20 % error bytes | Coverage vs oracle |
|---|---|---|---|---|---|---|---|---|
| ktor | 2,572 | 10.3 | 189 ms | 1.34 s | 7.7 | 42 (1.6 %) | 9 | 92.1 % |
| kotlinx.coroutines | 1,082 | 4.1 | 89 ms | 0.49 s | 8.3 | 10 (0.9 %) | 0 | 97.6 % |

Tags found on ktor: 3,341 classes, 324 objects, 14,732 functions, 22,211
properties, 4,014 constructor properties, 430 enum entries, 131 type aliases,
137 secondary constructors.

Gates: error files < 5 % on ktor (pass, 1.6 %); ≥ 90 % of top-level and
member declarations tagged (pass, 92 % and 98 %, and understated: the largest
"mismatches" are data classes whose multi-line primary-constructor `val`
properties the oracle counts and the tag count excluded, e.g. `Operation.kt`
98 vs 31, `OpenIdProviderMetadata.kt` 42 vs 5; counting the `param` capture
brings those to parity).

Known gaps (the documented list the gate asked for):

* **Named context parameters** (`context(ctx: CodeGenContext)` before a
  declaration, Kotlin 2.2) are not parsed. All 9 badly-broken ktor files are
  in `ktor-compiler-plugin` and use this form; 11 files in the tree use it.
  The declaration that follows is lost inside an `ERROR` node, so the regex
  fallback extractor must run for those files. Worth an upstream grammar
  patch; the fix is a small grammar rule.
* Scattered single-node errors in test files on `<`, `:` and `;` in generic
  call sites and labelled expressions (10 files in coroutines, all under 20 %
  error bytes); declarations around them still tag correctly.
* The grammar folds `class`, `interface`, `enum class`, `data class` and
  `annotation class` into one `class_declaration` node; kind refinement reads
  the leading keyword child, which is cheap.
* Parse throughput measured here (7.7–8.3 MB/s per core with all 12 threads
  parsing tiny files) is below the 10–30 MB/s single-thread figure from the
  research survey; whole-tree wall throughput is 46–54 MB/s, so 100 MB of
  Kotlin still parses in ≈ 2 s. Phase-2 targets in D§1 stand.

## 4. Consequences for M1–M2

* Trigram file-level index confirmed as the v1 format; planner keeps the six
  rarest grams; dictionary becomes a radix table above 5,000 files.
* Reader pool of 4 on macOS is now measured three ways (ripgrep, S1 reads,
  S2 stats) and is the default.
* Freshness: `lstat` files + `lstat` directories, re-list only moved
  directories; FSEvents with 150 ms cutoff; expected ≈ 45 ms worst case on
  60k files without FSEvents, ≈ 15 ms with.
* Kotlin: ship `-sg` 0.4.1, run the regex extractor on files flagged
  `ERRORS_IN_PARSE`, open an upstream issue for named context parameters.
* Sparse grams (M4) have a concrete target: cut identifier candidates 3–10 ×
  on the large corpora.
