# How greeg works

greeg answers a search from a persistent per-repository index instead of
walking the tree. What follows is what that index holds, how a query becomes an
answer, and why the pieces are shaped the way they are.

Five crates: `greeg` (CLI, output), `greeg-query` (planning, verification,
ranking, budget), `greeg-index` (file table, postings, symbols, freshness,
build), `greeg-lang` (grammars, extraction, import resolvers),
`greeg-fsevents` (macOS change log).

## Why an index at all

Searching a large tree is bound by opening files. On a 66k-file checkout,
ripgrep spends about 17 µs per file on `open`/`read`/`close` and only 0.26 s of
user time actually matching. Throw more threads at it and it gets *worse*,
because the kernel starts contending on its own locks.

Matching all of TypeScript takes milliseconds. Reaching the bytes takes seconds.

So the index exists to answer one question cheaply: **which files could
possibly match?** Everything else follows from that.

## What is on disk

The index lives outside the repository, under `~/Library/Caches/greeg/<hash of
realpath>` on macOS or `$XDG_CACHE_HOME/greeg/…` elsewhere. It never dirties
the working tree and survives `git clean`. `GREEG_INDEX_DIR` or `--index-dir`
override it.

Every component is a flat, fixed-width table read through `mmap`, with no
deserialization step. A `manifest` (JSON) names the current generation of each.

| File | Holds | Used for |
|---|---|---|
| `files.<gen>.bin` | path, size, mtime, language, flags, line count, rank | the file set, filters, ranking |
| `grams.<gen>.bin` | trigram → roaring bitmap of file ids | candidates for any pattern |
| `words.<gen>.bin` | whole word → roaring bitmap of file ids | candidates for identifier queries |
| `symbols.<gen>.bin` | every definition: name, kind, file, span, parent, supertypes | `def`, `impls`, enclosing-symbol lookup |
| `spans.<gen>.bin` | per file: definition, comment/string, and import ranges | classifying a hit in O(log n) |
| `graph.<gen>.bin` | file import graph (both directions) + PageRank | `map`, ranking, reachability |
| `delta/NNNN.bin` | the same layouts, for recently changed files | edits, without a rebuild |

`<gen>` is the generation the component was written under. A rebuild can
publish a whole new set while readers still hold the old one, and the manifest
says which generation is current.

Two choices here were deliberate.

**Postings are per file, not per position.** A positional index (which byte
offsets hold this trigram) costs 2–3.5× the corpus. greeg has to open a
candidate file anyway to print the line, its enclosing symbol and its context,
so knowing *where* in the file buys almost nothing. File-level postings with
roaring compression land at 0.15–0.6× of source, and they intersect at memory
speed.

**Words sit next to trigrams.** Trigrams can't tell `createSourceFile` from
`createSourceFileWithText`. On a 74k-file tree, that query opened 486 files to
find 30 with a real match. One posting list per distinct word cuts it to 31,
and `-w node` from 6,086 files to 1,879. The word section costs 0.4–0.6× of the
gram section, and it answers the question agents ask most.

## Building the index

The first query in a new repository is answered by a ripgrep-speed scan while
the build runs detached behind it. There is no setup step and no daemon.

The build runs in two phases so queries get useful answers early:

**Phase 1** walks the tree (honouring `.gitignore` exactly as ripgrep does),
reads each file on a small pool, classifies it, and extracts its trigrams and
words. The pool is 4 threads on macOS, where reading degrades badly past that,
and all cores on Linux.

Postings accumulate against a byte budget and spill sorted segments to disk
when they hit it, so peak memory stops tracking the size of the tree. A k-way
merge streams them back at publish time. Small repositories never reach the
budget and never spill.

**Phase 2** re-reads the files that have a grammar (now warm in the page
cache), parses them with tree-sitter on all cores, extracts definitions,
comment/string spans and imports, resolves the imports into graph edges, runs
PageRank, and republishes the file table with ranks.

Between the two phases, queries already prune candidates with the grams, and
fall back to a regex definition extractor for kinds.

Publication is a single atomic `rename` per component followed by the manifest.
Writers take an exclusive lock; **readers never lock**, and a reader that has
already mapped an old generation keeps a valid view until it exits.

## Answering a query

1. **Plan.** The pattern is parsed into a regex HIR, and the required grams are
   computed over it (Cox's algorithm): concatenation crosses literal sets,
   alternation unions, classes expand while they are small. Case folding
   happens at index time, so `-i` is free. A whole-word query skips the grams
   and reads word postings instead. A pattern with no usable literal (say
   `\w{5}\s+\w{5}`) plans a full scan over the file table, which still beats
   a walk.
2. **Select candidates.** Roaring bitmap intersections in ascending
   document-count order, with early exit on empty. Path filters become file-id
   ranges (ids are assigned in sorted-path order, so `src/**` is one range),
   type filters and flag filters are precomputed bitmaps. Tombstones are
   subtracted, delta postings unioned.
3. **Verify.** A pool of reader threads reads candidates in *prior order*
   (best files first) and matches them with the real regex engine. Ordering by
   prior means the budget can stop early and still hold the best hits.
4. **Classify.** Each hit gets a kind from binary searches over that file's span
   tables: inside a definition's name → `def`; inside a comment or string →
   `comment`/`string`; inside an import → `import`. Otherwise a per-language
   byte rule reads the characters around the match: `(` after → `call`, `.`
   before → `member`, `: ` or `-> ` or `impl ` before → `type`, else `ident`.
   That's O(1) per hit, and right for the overwhelming majority of identifier
   hits. `--precise` re-parses the shown files and replaces the rule with real
   node kinds.
5. **Rank.** `kind × location × importance`. Definitions outrank calls outrank
   types outrank comments. Source files outrank tests, mocks, vendored and
   generated code, which are **demoted, never hidden**: the footer always says
   how many got pushed down. Important files (import-graph PageRank) outrank
   leaves, and a definition whose name equals the query exactly gets a further
   boost.
6. **Shape to a budget.** If everything fits, print it. If it doesn't, print
   *facets*: counts by kind, by directory, by language, the top definitions and
   the top hits. What an agent needs first from a 357-hit query is the shape of
   the answer, and 50 lines of it won't tell you that.

`--budget 0`, `-l` and `-c` bypass token shaping and default to exact matching.
Their stdout contains only result rows; diagnostics and the footer go to
stderr. They never relax a failed query unless `--matching discover` is explicit.

### When nothing matches

A ranked text query that finds nothing climbs a ladder, and the footer says which rung
answered: drop `-w`, then case-insensitive, then split the query on
camelCase/snake_case boundaries and look for names built from those tokens,
then a bounded fuzzy search over the symbol names, then hits that exist only in
ignored or hidden files. `SpawnBlocking` finds `spawn_blocking` this way.
`--matching exact` (or the retained `--no-ladder` spelling) turns it off.
Files/count modes, unlimited output and JSON default to exact matching;
`--matching discover` explicitly enables the ladder for file searches in
those formats. The query layer carries a `MatchingPolicy` independent of
rendering; its default is exact, and the CLI selects discovery for ranked text.

`--mode files|count` follows the same defaults as `-l`/`-c`. An explicit policy
overrides the format default; `--no-ladder` conflicts with `--matching`.
Exact matching still honors `-i`, `-S`, regexes and fixed strings. It does not
change file selection, classification, freshness, or output budgets.

`def` also honors `-i` and `-S` without enabling discovery. Requested case
variants remain exact hits, even when a same-case definition also exists.
Non-ASCII definition names absent from the symbol/module index use the scan
fallback because the current symbol and word indexes can truncate them.
Existing indexed symbol/module results are retained; the fallback costs a
scan of the selected files.

File searches exit 0 for exact hits, 1 for no exact hits (including
discovery-only answers), and 2 for errors, including output-write failures.
Stdin always uses exact matching and rejects explicit discovery. JSON keeps
its existing summary/footer records on a miss, without match records.

Callers that previously consumed relaxed file lists, counts, unlimited output
or JSON must now opt in with `--matching discover`. Library callers replace
`Options::ladder` with `Options::matching`; its default is exact. These policies
do not promise complete enumeration under a positive budget or resolve the
remaining symbol-verb and selection inconsistencies.

### Agent hook contract

The hook accepts one simple `rg` invocation and emits `greeg --matching exact`
with an explicit `--max-filesize`: the supplied byte limit, or `18446744073709551615`
when no limit was requested. This prevents the native 4 MiB default from omitting
large files. Explicit zero retains its zero-byte meaning. Reading larger files
can cost more memory and time; native greeg's default remains unchanged.
It is an agent-facing presentation adapter: ranked text remains budgeted and
adds syntax information. It is not a byte-compatible replacement for ripgrep.
Direct `greeg` calls retain the exact/discovery defaults described above.

| Input | Hook behavior |
|---|---|
| Plain `rg PATTERN [PATHS]`, one pattern including `-e`, literal quoting/escapes | Rewrite; preserve argument boundaries, request exact matching and the requested file-size limit |
| `-i -S -s -w -x -F -U`, context, type/glob, thread and numeric size limits | Forward supported flags; `-u`/`-uu` map to ignore/hidden flags |
| Standalone `-l` / `-c` | Exact matching with paths/counts on stdout; greeg diagnostics/footer on stderr |
| Line/heading/column/color presentation flags | Use greeg's ranked presentation; validate `--color` values |
| `grep`, `egrep`, `fgrep`, including explicit-file searches | Decline: traversal, regex dialects and binary behavior differ |
| `--json`, `--stats`, sorting, suppressed diagnostics, unknown/unsupported flags | Decline; retain the original output/error contract |
| Paths such as `./rg` or `/usr/bin/rg`, wrappers, assignments | Decline; do not substitute another executable |
| Pipelines, `cd ... &&`, lists, conditionals, background jobs, redirections | Decline the entire command |
| Comments, continuations, expansions, unquoted globs (including option values), malformed quoting | Decline; quoted metacharacters remain literal |
| Leading/trailing unquoted blank lines | Accept only at command boundaries; preserve escaped spaces and literal Unicode whitespace |
| Nonempty inherited `RIPGREP_CONFIG_PATH`, even with `--no-config` | Decline conservatively; do not interpret configuration |

Declining produces no hook response and no successful-rewrite statistics record.
The original tool runs under the host's normal permission handling. Accepted
commands use the existing `Bash` / `tool_input.command` protocol; Codex replies
retain the required `permissionDecision: allow`, while Claude replies omit that
decision. Automated tests exercise both response shapes and execute rewritten
arguments through `/bin/sh`; they do not certify every host version or live
approval interaction. New host protocols require separate verification.

This narrows earlier hook eligibility, including grep and pipeline rewrites.
It does not fix the remaining native search selection/index-coverage gaps or
make limited ranked output complete. File/count differential tests cover the
recorded fixture and flags, not all ripgrep inputs. Configure the host to disable
the hook when original output or unverified behavior is required. No grep, JSON,
or pipeline rewrite should be re-enabled without end-to-end equivalence tests.

### Hook configuration updates

Both installers share a configuration snapshot and publication helper in the
CLI crate. Reads require a regular file and do not follow a config-file symlink.
Edits are parsed before publication. Changed configurations acquire a persistent
same-directory `.greeg-<filename>.lock` using a nonblocking advisory lock, then
validate the original content, identity and timestamps. Busy writers return an
error to retry. No-op edits and dry runs do not create locks or temporary files.
No-op updates recheck the snapshot before allowing subsequent skill changes.

The replacement is written exclusively to a same-directory temporary file,
metadata is preserved, and the file is synced before a final snapshot check.
Existing files are replaced by rename; initially absent files are published by
hard link, which cannot overwrite a concurrent creation. The directory is synced
after publication. Failures before publication leave the original intact and
clean up the temporary file; a directory-sync error explicitly reports that
publication already occurred. A forced kill can leave a temporary file, but the
original remains complete and the OS releases the lock. Lock files are retained
to keep one stable inode for cooperating writers; do not delete them during use.

Replacement requires current-user ownership and a single link, preserves existing
permissions, and creates new configurations with mode `0600`. Newly created lock
and temporary files explicitly receive mode `0600` regardless of umask; existing
lock permissions and caller-owned parent directories are not changed. macOS copies
metadata, including ACLs and extended attributes, and updates the modification
time to reflect the new content. Linux preserves the mode and
group for ordinary files; source or replacement files with extended attributes or ACLs are refused
instead of silently losing that metadata. Parent-directory permissions are not
modified. Config-file symlinks and nonregular files are refused rather than
followed or replaced; directory symlinks remain supported.

This is optimistic conflict detection for third-party editors, not a filesystem
compare-and-swap: an uncoordinated write after the final check can still race with
rename. The lock coordinates current greeg writers only. Parent-directory swaps
and arbitrary same-user tampering are outside this contract. File and directory
syncs improve durability but are not a tested power-loss guarantee. Configuration
publication and generated-skill changes are not a joint transaction; a later
skill error can occur after configuration publication and is reported as such.

### Generated skill ownership

Both installers inspect `SKILL.md` before editing configuration. Generated text
ends with a `greeg-managed-skill:v1` HTML comment identifying the agent and a
BLAKE3 checksum of the generated body. This detects edits; it is not an
authentication signature against other code running as the same user. Unknown
marker versions, another agent's marker, checksum mismatches, and unrecognized
unmarked text are preserved, including on uninstall. Exact copies of the current
bundled legacy text are adopted on install or removed on uninstall; older or
edited unmarked variants are conservatively preserved. Older greeg binaries do
not honor this protection.

Unmodified managed skills can be upgraded and removed. Reinstalling identical
content does not rewrite the file or change its mode, inode or modification time.
Modified skills produce a preservation message while hook configuration edits
continue. Move a preserved file aside before reinstalling to regenerate it.
Dry runs report the same ownership decision without writes.

The shared configuration snapshot helper also publishes skill updates atomically
and checks identity/content before removal under the same persistent per-file
lock. New skill files use mode `0600`; replacements retain metadata under the
platform rules above. Removals sync the containing directory and retain the lock
file. File symlinks, nonregular files, invalid UTF-8 and unreadable skill files are
rejected at inspection before configuration publication. Mutation of a hard-linked
or foreign-owned managed file is refused. Existing parent directories and their
permissions are retained. Configuration and skill publication are separate;
uncoordinated changes after the final snapshot check, directory swaps, and
power-loss behavior have the same limitations as configuration publication.

### Session memory

A query that names an agent session (`--session ID`, or the parent agent
process discovered automatically) appends a record to `session/<id>.jsonl`
under the index directory: the normalized query, the files it showed, and the
line ranges it printed. Records older than a day get pruned, and the log keeps
the last 2,000. Reading it costs well under a millisecond, and buys 3 things:

- **Context dedup.** Context lines already printed this session, for a file
  whose mtime hasn't changed, get dropped and the hit marked as seen. Explicit
  `-A`/`-B`/`-C` is never deduped.
- **Focus set.** The 12 most recently shown files, newest first, get a ranking
  boost, so a follow-up query lands in the code you were already reading.
- **Loop detection.** The same token set 3 times in the last 5 queries adds a
  footer hint, and says so plainly when the repeat turned up nothing new.

`--no-session` turns all of it off.

## Staying fresh

The index has to reflect the working tree at query time, and there's no daemon
to keep it there.

* **macOS, large trees**: the manifest stores an FSEvents stream id. The query
  asks the kernel's persistent log which directories changed since then, and
  rescans only those. About 12 ms for a 200-file change set.
* **Everywhere else**: a parallel `lstat` of every known file, comparing size,
  mtime and inode, plus directory mtimes to catch additions and deletions.
  41 ms for 66k files. Below about 8,000 files this beats FSEvents, whose
  stream setup costs ~11 ms no matter how small the tree.
* **Back-to-back calls**: a manifest verified in the last 100 ms is trusted, so
  several tool calls in one agent turn pay for a single check.

A search that finds changes **answers first**. It drops the stale versions from
its candidates, reads the changed files directly, prints the answer, and only
then spawns a detached process to extract them and publish a delta segment.
You still get the working tree as it was at query time. The tree-sitter work is
what moved off the critical path.

Past a threshold (an edited `.gitignore`, more than 2,000 changed files, 16
accumulated deltas, or 5 % of the tree) greeg spawns a full rebuild and answers
the query with a scan.

Symbol verbs are the exception. `def`, `refs` and `outline` need the changed
files' symbols, so they apply the delta inline before answering.

## Symbols and the import graph

Definitions come from tree-sitter queries written for greeg, modelled on
Sourcegraph's tag queries. The upstream `tags.scm` files are too coarse to tell
a method from a field.

A file that fails to parse, or whose language has no grammar, falls back to
anchored regexes per language. `def` and `refs` still work everywhere; you lose
the precise kinds and nothing else.

Imports are resolved to file ids at index time, per language: Python package
paths and relative dots, TypeScript/JavaScript through the nearest
`tsconfig.json` `paths` and `baseUrl` plus workspace packages, Rust `use` and
`mod` declarations against the crate layout, Kotlin through the package table.
Resolved imports become weighted graph edges. PageRank over that graph is what
makes a hub file outrank a leaf in every ranked answer.

The verbs read this directly. `refs` is a word-bounded search grouped by kind.
`callers` is `refs` filtered to calls and keyed by the *enclosing function*, so
you get back a list of functions instead of a list of lines. `impls` reads the
supertype lists. `impact` splits references into WILL BREAK (a call or type in
non-test source), MAY BREAK (member or bare identifier) and REVIEW (comment,
string, test).

Cross-file type resolution here is heuristic. It's a set of import-graph rules,
not a compiler, so where it matters the output carries a confidence, and
`--precise` tightens classification on the files actually shown.

## What the output means

Ranked text is grouped by file, with the path printed once:

```
django/db/models/manager.py
  150 def   def get_queryset(self):        ‹ BaseManager
  212 call  return self.get_queryset()     ‹ BaseManager.all
```

The column after the line number is the hit kind, and `‹` names the enclosing
symbol. A row that *is* a definition names its parent, never itself.

The footer is a contract, and the tests in `crates/greeg/tests/cli.rs` assert
it:

* A complete answer (every hit shown, nothing skipped, matched as asked) ends
  with just `N hits · M files`. `no hits` is bare. Those counts say the output
  is whole, and anything more would be 5 tokens that change nothing about what
  you do next.
* Everything else is accounted for: `shown/total hits · shown/total files`,
  `N files demoted (hits)`, `skipped N binary` (only when non-zero), `matched
  <rung>` when the ladder relaxed the query, and `~N tokens`.
* `next:` suggests flags only, never a different pattern.

A bare identifier is answered as a whole word. Matches inside *longer*
identifiers stop competing for answer lines and collapse into one `related`
line naming them with their file counts. If nothing matches the whole word,
those near-misses become the answer instead.

`--json` keeps ripgrep's JSON Lines schema exactly, adding `kind`, `symbol`,
`file_flags` and `score` to match records, plus `facets` and `footer` record
types.

## Languages

Python, TypeScript/TSX, JavaScript, Rust and Kotlin ship compiled in with
tree-sitter grammars. Every other language gets the regex extractor, the
byte-level comment/string lexer, and full search, ranking and budgeting. What
it loses is precise symbol kinds and import resolution.

You can add one without rebuilding greeg. Drop `spec.toml`, a compiled
`grammar.so`/`.dylib` and a `tags.scm` using greeg's capture names into
`~/.config/greeg/lang/<name>/`, then run `greeg lang check DIR` to validate
coverage against your fixtures.

## When things go wrong

The index is a cache, and every failure path ends in a correct answer.

A panic anywhere in the index path is caught, degrades to a scan, and queues a
background rebuild. A corrupt posting list reads as "every file" and marks the
index for rebuild; a superset is harmless, because verification runs the real
matcher anyway. A truncated component fails to open and triggers a rebuild.
A `SIGBUS` from a file truncated under an active mmap re-executes the same
command with `--no-index`. A format-version mismatch rebuilds. There's no
migration code, by design.

## Design decisions

| Decision | Chosen | Instead of | Why |
|---|---|---|---|
| Process model | short-lived process per query | a resident server | warm queries are 3–10 ms; a daemon is a lot of code to keep an index and a filesystem in step, and buys nothing at that latency |
| Posting granularity | one bitmap per file | positional postings | the file has to be opened anyway to print; 3× smaller |
| Identifier queries | whole-word postings beside the trigrams | sparse or variable-length grams | measured: every sparse variant was larger *and* slower to build; word postings cut candidate reads 15× on the queries agents actually ask |
| Call/type kinds | byte rules at query time | stored spans for every call | spans for every call site would roughly double the index; the rules are O(1) and `--precise` exists for when they are not enough |
| Index location | user cache directory | `.greeg/` in the repository | never dirties the tree, survives `git clean`, no `.gitignore` edits |
| Freshness | stat pass, or the OS change log | a mandatory watcher process | zero setup is the whole point, and the measured cost is tens of milliseconds |
| Post-edit search | answer first, publish the update after | apply the update, then answer | the inline update cost 28–150 ms on every search after an edit |
| Delta durability | no fsync on delta segments | fsync every publish | `F_FULLFSYNC` was 4–5 ms of the 28 ms an edit cost the next query, and the index is a cache: a torn delta simply rebuilds |
| Demoted files | ranked down, always counted | filtered out | an agent that can't see test code can't reason about test failures; the footer says what got pushed down and how to bring it back |
| Cross-file resolution | import-graph heuristics | stack-graphs | archived upstream, no Kotlin or Rust support |
| Reader threads on macOS | 4 | all cores | measured 3× faster on 66k small files; kernel lock contention dominates |
| Serialization | hand-laid fixed-width tables over mmap | a serialization framework | zero-copy, with no framework churn or format instability, for what are 4 flat tables |
| Unsupported flags | exit 2 | accept and ignore | `-a` asks for binary files the index doesn't hold, so answering without them is a wrong answer dressed as a cosmetic one |
| Type filters | ripgrep's own type definitions | greeg's language table | parity means rg's rules, including its glob precedence |
| Extra languages | `dlopen` a user-compiled parser | WASM grammars or a plugin format | no new dependencies; the tree-sitter C ABI is stable |

## Measuring it

`bench/bench.py` drives the benchmark protocol: pinned corpora, hyperfine runs
against `grep`, `rg` and `rg -j4`, an accuracy oracle scored against SCIP
indexes (`rust-analyzer`, `scip-python`, `scip-typescript`), and regression
gates keyed by host, so a laptop records its numbers but only the reference
machine can fail the build.

Paired reports are registered in `bench/reports.toml`: filenames, titles, order
within each section, optional analysis and configuration recheck relationships.
Use optional `notes` for measurement-specific interpretation and limits; headings
render them without adding branches for individual runs.
`bench/hook_config.py --suite skills` measures skill lifecycle and configuration together; the default
suite retains its configuration fixtures. Both it and `bench/hooks.py` record
execution start/completion timestamps in new result files.
Set `pr` to the positive number of the originating greeg PR once known; rendered
headings link to it. Omit it for unpublished measurements.
Record `measured_at` as an unquoted TOML offset datetime (for example,
`2026-09-23T11:35:57Z`) with `timestamp_source = "measurement"` for a known
execution time. For historical results without one, use the committer timestamp
of the first commit adding the dataset (follow renames), set
`timestamp_source = "first_commit"`, and include its full SHA in `timestamp_commit`.
Reports normalize times to UTC and explicitly label these commit dates as estimates,
not recorded execution times. Omit all three fields if neither time is known;
partial metadata and datetimes without a timezone are rejected. Other measurement
metadata stays in the raw results.
Add measurements to `hook`, `hook_config` or `matching_review` there without
changing renderers; `matching` and `matching_recheck` each allow exactly one dataset.
`bench/report_catalog.py` validates the catalog and report-facing fields for
matching, hook and installer protocols 1/2, and reads each dataset once per render.
Unknown result metadata is allowed; unknown catalog options and protocols fail.
Missing files and valid empty result arrays produce no section. Present invalid
JSON, duplicate fields, invalid types or missing required fields stop the report
with a filename/field diagnostic before writing the output. Explicit null timings
remain unmeasured; failed or undefined timing pairs produce no comparison.
Validation does not recompute raw-sample statistics or verify binary contents;
legacy speed, oracle and disposable-corpus schemas remain outside this validator.

`bench/reporting.py` shares table formatting, links and latency thresholds;
`bench/bench.py` retains protocol-specific rendering and explanations.
Configuration recheck identity claims require both datasets and matching recorded
binary digests. No process-wide result cache is retained between renders.

`bench/parity.py` checks that greeg's match set equals ripgrep's across every
corpus and query family, in both indexed and scan mode. `bench/soak.py` fires
randomized queries at randomized edits.

Results and method: [BENCH.md](BENCH.md). Usage statistics: [STATS.md](STATS.md).
Raw results live in [`bench/results/`](../bench/results/). For measurements on
your own agent traffic, see [the README](../README.md#is-it-actually-helping).
