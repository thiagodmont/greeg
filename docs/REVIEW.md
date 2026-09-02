# greeg — Review after M6

Date: 2026-09-02. Scope: everything shipped through M6 (code, docs, benchmark
suite, hook), reviewed against the two product claims: (a) fastest grep for
coding agents, (b) more useful context per token. Items marked **verified**
were reproduced by hand on the reference machine against the bench corpora;
the rest come from code reading with `file:line` references.

## Status (2026-09-02, branch `v0.2-review-fixes`)

Everything in §7 steps 1–6 shipped except: the agent A/B/C run, the 24 h
soak, the Kotlin oracle, the large-corpus rerun, `--sort` kinds other than
`path` and `--count-matches`. Details per item in `PLAN.md` M7; the
parity matrix in `bench/parity.py` encodes C1–C6, C12–C14 and the
`.gitignore` case; C7/C8/C15/C16 have integration tests in
`crates/greeg-index/tests/index.rs`; C17 has per-language byte-rule tables;
C18/C19 have unit tests in `greeg-lang`.

## 0. Verdict

* The index engine is real and fast: 3–10 ms warm on 60k-file trees, parity
  with rg on every non-multiline query tried, clean clippy, tests pass.
* Four correctness bugs would bite a Claude Code user within minutes
  (absolute paths, `-l | xargs`, `.gitignore` edits, `-U`). None is caught by
  the parity suite because it only compares `(path, line)` sets on relative
  paths without `-U`.
* The token-efficiency claim is **not yet demonstrated**. By the suite's own
  data the per-query median favours rg (django 205 vs 430 tokens, TypeScript
  383 vs 634); the "4–14× smaller" figure is a mean driven by a handful of
  500-line rg outputs. Hands-on, half of a default answer is paths, chains
  and test-file definitions; a compact layout carries the same hits at 42 %
  of the cost.
* The speed table is 3–7× optimistic on large trees because hyperfine's
  back-to-back runs hide the freshness check behind the 100 ms TTL, and the
  headline "57× vs rg" divides by rg at its worst thread count on macOS.
* Nothing is committed to git.

## 1. Correctness — fix before anyone else uses it

| # | Finding | Where | Status |
|---|---|---|---|
| C1 | **Absolute path arguments return zero hits in index mode** with the hint "matched in ignored/hidden files: add --no-ignore --hidden". Claude Code passes absolute paths by default. Scan mode works. | `greeg-query/src/indexed.rs:81-86,156,171` compares `rel` against `o.paths` verbatim | verified |
| C2 | **`-l` / `-c` are not pipe-safe**: stdout gets `path  N hits [flags]`, a blank line, the footer and the `next:` line, and the list is truncated by `--budget` (also at `--budget 0` the footer stays). The hook rewrites `rg -l foo \| xargs …` into this. | `crates/greeg/src/main.rs:638,643,700-720`; `shape.rs:180-186`; `hook.rs:260` | verified |
| C3 | **Ignore-file edits are never detected.** The walker and both re-listing paths use `.hidden(true)`, so `.gitignore`/`.ignore` are never in the file table and `is_ignore_file` is dead code. After appending a path to `.gitignore`, rg drops the file, greeg keeps returning it under `--fresh stat`/`auto` until an unrelated rebuild. | `greeg-index/src/build.rs:78`, `fresh.rs:140,155,181,222,262` | verified |
| C4 | **`-U` multiline returns zero hits** (index and scan) where rg finds 140 lines on ktor. `CollectSink` stamps every match in a block with the block's first line and `process_file` drops hits whose end passes the line end. | `greeg-query/src/lib.rs:379-393,600-605` | verified |
| C5 | **One JSON `match` record per submatch, not per line**: 486 vs rg's 441 for `Semaphore` on tokio. Inflates every facet count and prints the same line three times under "top hits". | `main.rs:789` builds a single-element `submatches` per hit | verified |
| C6 | **stdin is ignored**: `echo hello \| greeg hello` searches the tree. rg searches a readable non-tty stdin when no path is given (`grep_cli::is_readable_stdin`). | `main.rs` dispatch | verified |
| C7 | **Manifest races** (no `LOCK`, no compare-and-swap): `fresh::apply` writes back the manifest captured at open, so a query that opened during phase 1 can revert `phase2` to false after the build finishes, or revert the generation to files the build already deleted. Two concurrent queries past the TTL create the same `delta/000n.tmp`. A delta computed against generation N can land in the new generation's `delta/` and apply stale tombstones. `Index::open` loads every `delta/*.bin` regardless of `manifest.deltas`. | `fresh.rs:310-346`, `format.rs:303`, `build.rs:229-263,398`, `index.rs:182` | code |
| C8 | Publish gap: `build` removes `delta/` and `tomb.bin` before writing the new manifest; readers in that window see deleted files again. | `build.rs:229-234` | code |
| C9 | Files > 4 MiB are never searched in index mode (no grams, no `--include-huge`, no `HUGE` handling in greeg-query); rg searches them. The footer's "skipped N huge" only appears in scan mode. | `build.rs:122-141` | code |
| C10 | First-64-matches cap per file is applied before `--kind`, so a definition that appears after 64 uses in a file is missing from content, from `--kind def` and from "definitions (N of N)". | `lib.rs:386,571,607` | code |
| C11 | `--kind` makes the footer lie: `1 of 31 hits`, `+30 more in this file (1 def)`. | `lib.rs:621` | code |
| C12 | Session dedup keys on `(file, line)` regardless of what was shown, has no mtime guard, and suppresses explicitly requested context: a second `greeg PAT -C 2` prints only `(shown before)` per hit. `--mode block` after a content answer prints nothing. | `session.rs:159`, `shape.rs` | verified |
| C13 | Glob-looking path (`greeg X 'django/db/**'`) → "no hits", exit 1. rg exits 2 with an error. Should become `-g`. | `main.rs` | verified |
| C14 | Exit code 0 when only a ladder rung (fuzzy/split) matched; agents using `$?` as "pattern present" get a false positive. | `main.rs:573` | code |
| C15 | `-U` with FSEvents: ancestor-directory moves are missed (only files whose immediate parent was reported are re-stat'd); `fsevents_id` is captured after the check, so edits during the pass are invisible to later FSEvents checks. | `fresh.rs:256,311` | code |
| C16 | `Segment::symbols()/spans()` and `Index::graph()` return `'static` views over the mmap: unsound public API. Delta header slicing is unchecked (panic on truncation, no rebuild mark); a corrupt tombstone bitmap is silently dropped (resurrects superseded versions). | `index.rs:46-51,188-200,449` | code |
| C17 | Byte-rule misfires on common code: Rust `let mut count = 0` → `count` is `type`; `match state {` / `for x in items {` → `call`; Kotlin `class Foo : Bar {` → `Bar` is `call` (which starves `impls`); TS/Python `{key: value}` → `type`; Python `class Foo(Base):` → `Base` is `ident`. | `lib.rs:434-483` | code |
| C18 | Rust `exported()` = any visibility modifier, so `pub(crate)`/`pub(super)` count; Python and Kotlin are always exported. `def JoinHandle` on tokio ranks `pub(crate) struct JoinHandle` (blocking.rs) and the mock in `fs/mocks.rs` above the real `pub struct JoinHandle` (runtime/task/join.rs), with or without session. | `greeg-lang/src/sym.rs:506-527` | verified |
| C19 | `#[cfg(not(test))]` and `#[cfg_attr(test, …)]` mark items `[test]`; functions in `mod x {}` become `method`; Kotlin locals inside functions are indexed as definitions (unanchored query); Kotlin `@Annotation fun` on one line is `call` in scan mode; Kotlin class kind sniffed from annotation text. | `sym.rs:299-304,545,593`; `kotlin.scm:6-7`; `defs.rs:103,152-174` | code |
| C20 | `--json` `lines.text` is trimmed and clipped and `submatches.start` is relative to it; rg consumers computing columns get wrong values. No `context`/`summary` records. | `lib.rs:614`, `main.rs:774-860` | code |
| C21 | UTF-8 BOM kept (`bom_sniffing(false)`): `^foo` and definition regexes miss line 1. NUL after 8 KiB drops the file without counting it as binary. Default `--max-filesize` 4 MiB skips files rg searches. | `lib.rs:567,575,815` | code |

## 2. Hook (`greeg hook run`) — verified cases

| Input | Output | Problem |
|---|---|---|
| `rg foo . ../other` | `greeg foo ../other` | drops `.` |
| `rg foo -- -weird` | `greeg foo -weird` | drops `--`, `-weird` becomes a flag, exit 2 |
| `rg -tjs foo` | `greeg -t -j -s foo` | attached values split into flags |
| `rg foo\|wc -l` | `greeg -l 'foo\|wc'` | unspaced pipe read as pattern |
| `grep 'a\|b' src` | `greeg 'a\|b' src` | BRE passed as Rust regex |
| `rg -- -x src` | `greeg -e -x src` | `-e` cannot take a leading-dash value (`main.rs:44` lacks `allow_hyphen_values`) |
| `rg -e a -e b` | `greeg -e a -e b` | `regexp` is `Option<String>`, clap rejects |
| `rg -N foo`, `-H`, `--color`, `--no-heading`, `-p`, `--column` | exit 2 on the CLI | README says "accepts ripgrep's flags"; the hook strips them but the binary should too |
| `grep foo` (stdin) | `greeg foo` | becomes a tree search |

Also: PreToolUse hooks run in parallel and "the last `updatedInput` wins", so
inserting at index 0 does not order it before other rewriters (this machine
has RTK installed); allowlist rules like `Bash(rg:*)` stop matching after the
rewrite. Conservative non-rewrites (`-v`, `-o`, `--files`, `-m`, `2>/dev/null`,
`timeout`, env assignments) are fine.

## 3. Benchmark honesty

* **TTL flatters the speed table.** `bench.py:222` runs `greeg` with `--fresh auto`; hyperfine's 13 back-to-back runs all land inside the 100 ms TTL, so the freshness check is skipped and the manifest is rewritten on every run. With 0.5 s gaps on the 66k-file TypeScript tree a query is ≈ 19 ms here (FSEvents ≈ 12 ms), and ≈ 30 ms on a 68k-file synthetic tree with `--prepare "sleep 0.15"`. On Linux (stat walk, no FSEvents) expect 60–70 ms. Still 15–40× rg, but the table says 6 ms. Fix: `--prepare "sleep 0.15"` and report the `fresh` cost separately; consider `--fresh none` as a documented "I just ran greeg" mode for agents issuing parallel calls.
* **"57× vs rg" divides by rg at its worst.** rg default threads on this Mac spends 31–45 s of system time per run; `rg -j4` is 3.7× faster. Against `rg -j4`, greeg is ≈ 15× on identifiers and greeg-scan is 0.92×. The report should headline vs best-configured rg and say the macOS kernel caveat aloud.
* **"All tools print path:line:text for every match"** (BENCH.md:7) is false for the `greeg` column (budgeted digest). `greeg-full` is genuinely unbudgeted.
* **Quality oracle**: names are sampled uniformly over SCIP definitions (`return5`, `mInt`, `_expr_167`, `test_if_tag_gt_02`, `C02`); 32/75 TypeScript and 29/75 django names have zero references, median 2. The budget never binds on them (`greeg6k` is token-identical to `greeg` at the median). rg's Acc@k uses the first lines of `rg -nw NAME`, which is non-deterministic across threads and not what an agent does (`rg 'fn NAME|struct NAME'`).
* **"Useful lines"** is a mean of per-row ratios and counts every layout line (blank, header, facet, footer, `+N more`) as waste: on tokio 45 % of greeg's lines are layout, and 21 % of its hit lines are in `cfg_*!`-gated code that rust-analyzer's single-cfg SCIP does not cover. The metric penalises both structure and SCIP blind spots.
* **Right metric**: tokens until all true definitions (or a fixed share of true locations) are covered, and distinct true locations per token with summary lines credited for what they aggregate. The A/B/C agent protocol is the only test of the thesis and has not been run.
* **CI**: `ci.yml:59` runs `parity.py`, whose table includes TypeScript and rust that the job never fetches → `FileNotFoundError` on every PR. Gates key on CPU model and only a `darwin-arm64-apple-m4-pro` baseline exists, so hosted runs create a baseline and pass vacuously; the Linux gate in the definition of done is unmet. Cold-cache prepare cannot run under `-N` on Linux; `opensubtitles` is skipped because it is a file; `tiktoken` install failure silently falls back to bytes/3.7 and the JSON does not record which tokenizer ran.
* **Statistics**: n=10 means with CV up to 0.59 (TTL outliers: TypeScript ident min 5.9 / max 18.7 ms); a 10 % gate on these flaps. Oracle buckets of 25 names carry ±19 pp at 95 %; no intervals printed. `bench.py:618` counts duplicated definitions twice in the useful ratio. `bench/agent/extract.py:40-44` counts tool calls from the assistant's `tool_use` input, so arm B (hook) will still be counted as `rg`.

## 4. Token efficiency — where the budget goes and what to do

django `get_queryset`, default budget, o200k = 1,398 tokens (verified):

| component | share |
|---|---:|
| test-file definition lines (19 lines) | 32 % |
| full `path:line` per hit | 27 % |
| enclosing chains (`create_generic_related_manager › GenericRelatedObjectManager › `) | 21 % |
| facets header (5 lines) | 8 % |
| footer + `next:` | 5 % |
| kind column | 3 % |

A mock compact layout (group by file, path once, last container only, test
definitions collapsed to `+19 test definitions (--all)`, header merged) carries
the same 28 non-test hits in **592 tokens (−58 %)**. Concretely:

1. **Group by file in every layout**, path printed once, `line kind text` under it. Facets and outline layouts repeat the full path per hit today.
2. **Last container only** in the chain by default; full chain under `--chain`.
3. **Collapse demoted definitions** (test/vendored/generated) into one count line; they are currently allowed to fill 60 % of the definitions block.
4. **Rank calls above imports** (kind weight import 0.6 > call 0.55): "top hits" for `Semaphore` are 10 `use …;` lines. Better: one line `imported by 19 files` and spend the slots on calls.
5. **Exact-name definitions first**: for `spawn_blocking` the canonical `pub fn spawn_blocking` is 6th of 30 behind `spawn_blocking_on`, `read_spawn_blocking` and test fns.
6. **Dedup submatches** (C5) and drop `exact-length` (meaningless for regexes).
7. **Per-file cap scales with file count**: `greeg X path/to/one/file.rs` returns 4 of 187 hits and 123 of 2,000 tokens.
8. **Drop `KB · age`** from headers (age from mtime is "today" for every file in a fresh clone; use git commit time or nothing), drop `· N ms` unless `--stats`.
9. **Hints**: never suggest a test area (`-g 'tests/admin_filters/**'`), never say "may be defined in a dependency" for a string-literal query, hint flags only (`next: --kind def | -g django/db/**`).
10. **Facets header**: merge kind+flag lines; move definitions above facets (PLAN.md already lists this).
11. Token divisors in `tokens.rs` (3.4/3.8) disagree with DESIGN (3.6/4.2); the estimate runs 5–8 % under o200k. Fine, but document one number.

## 5. Performance gaps versus the design

* No `madvise` on any mmap (§12 claims `MADV_RANDOM`/`WILLNEED`); postings deserialized with an allocation per lookup instead of a reused arena; candidates evaluated twice (`candidates` then `live`).
* Shown files are read 3–5 times per query (`refine_file`, `end_line_of`, `read_lines`, `--precise`); `refine_file` builds a `DefSummary` per definition in the window, the cost the index path explicitly avoids.
* `shape` fully sorts all hits and files when only top-k is needed; `per_hit` computed twice; a `String` per candidate for sort keys.
* Rung 5 of the ladder walks `.git/` and `node_modules` with `hidden + no_ignore`, uncapped, on every zero-hit query (78 ms on tokio, 125–165 ms for `-U` no-hit).
* `refs`/`impact` run the stat pass up to three times; `impact` ≈ 15 scans with `refine` on up to 400 files.
* Phase 1 rayon `fold` allocates a 2 MiB dedup bitset per split; postings sort runs on the global pool outside `pool.install` (a second pool); `read_to_end` through `Take` loses the size hint; delta files opened twice.
* Phase 2 holds every symbol name as `String` for all files at once (hundreds of MB on rust-lang/rust).
* Freshness rebuilds a 66k-tuple `known()` vector per check; `children_of` is O(files) per changed directory; manifest rewritten on every query past the TTL.
* `implementors` and `fuzzy_names` are linear scans; no path→id lookup (`verbs.rs:576` does `find`).
* `-l` does not stop at the first match per file; `-i` on `k`/`s` loses grams because regex-syntax expands them to Unicode classes (OR the ASCII gram group instead).

## 6. Docs drift, tests, hygiene

* DESIGN claims not shipped: `LOCK`, `--fresh-ttl` 300 ms (code 100), `watch` mode, `--include-huge`, `--budget-exact`, radix gram table, inode/content_hash/git_oid/line_count fields, blob cache, bounded A→B channel, personalised PageRank, overload grouping, caller count `?`, "all in comments/strings" report, `greeg show`, `context`/`summary` JSON records, context 40/12/4 (code 20/6/2), by-dir top 6 (code 8), inline thresholds (code parses up to 2,000 files inline, synchronously), compaction floor `max(200, 5 %)` (code 5 %, no floor), BINARY 30 % rule, MINIFIED sourceMappingURL, `*_test.*`. Code rules not documented: `e2e`, `dist`, `build`, `out`, `target`, `.next`, `site-packages` segments, `--mode count`.
* Adversarial rules that misfire: `build`, `out`, `gen`, `external`, `deps`, `spec`, `testing`, `fixtures` segments demote real source (Go `pkg/build`, API `spec/`, `src/external/`); `ends_with("test.kt")` flags `Latest.kt`; `.g.` is broader than `.g.dart`; no `mock`/`mocks`/`stub`/`fake` rule at all.
* Tests: 31 unit tests over 11.4k lines; zero in `build`, `fresh`, `index`, `gram`, `format`, `shape`, `hook` rewriting paths above; no `tests/` directory, no fixtures. `parity.py` never runs `-C/-A/-x/-U/-S`, BOM, binary, absolute paths or exit codes.
* Hygiene: no git commits; `crates/greeg/u8tmp.ts`; `spikes/target/` and `bench/__pycache__/` inside the tree; `spikes/` should be excluded or its target ignored.

## 7. Proposed order of work (v0.2)

1. **Parity blockers** C1–C6, C13, C14, hook rows in §2; add every one of them to `parity.py`, plus absolute paths, `-U`, `-l | xargs`, stdin, ignore-file edit, and exit codes. Add an `edits.py` step that edits `.gitignore`.
2. **Index safety** C7, C8, C16: `flock` on writes, re-read the manifest before `apply`, publish manifest last, `Index::open` honours `manifest.deltas`, checked slicing, `'_` lifetimes.
3. **Output contract v2** (§4 items 1–10) behind a single change of the text layout; re-measure tokens per useful location on the same queries; keep `--json` and `--budget 0` stable.
4. **Ranking**: fix `exported()` per language, exact-name-first for definitions, calls over imports, mocks demoted, recency from git or off.
5. **Bench honesty**: `--prepare sleep`, report vs `rg -j4`, medians, fix CI parity job and gate keying, usage-weighted oracle sample (names by reference count), definition-regex baseline for rg, credit summary lines, print intervals. Then run the agent A/B/C protocol once, because it is the only measurement of the thesis.
6. **Perf** (§5) in profile order: read-once refine, madvise, arena postings, ladder rung 5 scope, top-k selection.
7. Commit.
