# greeg output formats

## Text (default)

Chosen by the shaper (DESIGN.md §6.4) from the mode and the budget.

### Content layout

One header per file, then hits ordered by score. Adaptive context appears
only when there are few hits (1 hit: 20 lines, up to 3: 6, up to 10: 2),
clipped to the enclosing definition.

```
django/db/models/manager.py  25 KB · 2mo
    150 def     def get_queryset(self):    ‹ BaseManager
    212 def     def get_queryset(self):    ‹ EmptyManager
        +3 more in this file (2 call, 1 comment)
```

Columns: line, kind, line text (leading indentation removed, clipped to
`--max-columns` around the match with `…`), enclosing symbol chain after `‹`.
File header: path, `[flags]` when any of test/generated/vendored/minified/
lockfile apply, size, age of last edit.

### Facets layout (broad queries)

Used automatically when content for every hit would exceed the budget and
there are more than 12 hits.

```
respond  2,898 matches · 423 files · exact-length 1,204
by kind   def 38  call 1,401  ident 210  comment 1,236  string 13
by area   ktor-server/ktor-server-core/ 612  ktor-server/ktor-server-plugins/ 1,100 ...
by lang   kt 2,801  txt 97
by flag   source 2,176  test 722

definitions (38 of 38)
  def     ktor-server/.../ApplicationResponseFunctions.kt:44  suspend fun ApplicationCall.respond(message: Any?)

top hits
  call    ktor-server/.../Routing.kt:120 [test]  handle › call.respond(HttpStatusCode.OK)
```

Definitions take at most 60% of the budget when other hits exist; source
files rank before demoted ones.

### Outline layout (`--mode outline`)

One line per hit: `kind path:line  chain › text`.

### Block layout (`--mode block`)

Content layout, but each definition hit prints its whole body (capped at 200
lines), deduplicated per definition.

### Files / count (`-l`, `-c`, `--mode files|count`)

`path  N hits  [flags]` or `path:N`, ordered by file prior (source first) unless
`--budget 0`.

### Footer

Always present:

```
77 of 358 hits · 38 of 73 files · demoted 37 files (190 hits) · skipped 3 binary, 0 huge · matched case-insensitive · ~1434 tokens · 93 ms
next: get_queryset --kind def  |  get_queryset -g 'django/db/**'  |  get_queryset --no-tests
```

`matched <rung>` appears when the escalation ladder had to relax the query
(without word boundary, case-insensitive, `split tokens → names`,
`fuzzy → names`) or when hits exist only in ignored/hidden files. `~N tokens`
is the estimate for the whole output. `(shown before)` after a hit means its
context was already printed earlier in this session and is not repeated.

## Verbs

Every verb accepts the search flags (`--budget`, `--json`, `--no-tests`,
`--root`, `--fresh`, …) before or after the verb. All work without an index
through scan mode except `map`; with the index they answer in a few
milliseconds.

### `greeg def NAME [--from FILE] [--def-kind KIND]`

```
def JoinHandle  7 of 18 definitions · index · 1 ms
  struct    tokio/src/runtime/task/join.rs:163  pub struct JoinHandle<T> {   [exported]
            "An owned permission to join on a task (await its termination)."
  impl      tokio/src/runtime/task/join.rs:324  impl<T> Future for JoinHandle<T> {   : Future reach 1.0
  variant   tokio/src/runtime/tests/task_combinations.rs:63  CombiAbortSource › JoinHandle,   [test]
  +11 more (raise --budget)
next: greeg refs JoinHandle  |  greeg callers JoinHandle  |  greeg outline tokio/src/runtime/task/join.rs
```

Columns: kind, `path:line`, enclosing chain › signature line, then
annotations (`[exported,test]`, `: supertypes`, `reach` when the definition
is imported by `--from` or a recently seen file). A doc first line follows
in quotes. `impl` blocks are capped at three when other kinds exist. With no
exact name the ladder tries case-insensitive, split tokens and fuzzy names
and reports `matched fuzzy → spawn_blocking`.

### `greeg refs NAME`

Hits grouped by kind, best first, with the budget split across kinds in
proportion to √count:

```
refs Semaphore  367 hits · 33 files · 59% resolved to a definition · index · 8 ms
defined at  tokio/src/sync/batch_semaphore.rs:35 (struct)  …

call (5)
  tokio/src/sync/mpsc/bounded.rs:161  channel › let semaphore = Semaphore {
type (76)
  tokio/src/sync/rwlock.rs:95  RwLock › s: Semaphore,
  +70 more
```

`resolved` is the share of hits whose file imports (directly or within two
hops) a file defining the name.

### `greeg callers NAME [--depth 2]`

One line per calling function: kind, `path:def_line`, enclosing chain, `×`
call count, hit lines. `--depth 2` adds `← called by …` with the functions
that call each caller.

### `greeg impls NAME`

Definitions whose supertype list contains NAME (`impl T for X`, `: T`,
`extends`/`implements`, Python bases), then a low-confidence section of
type-position hits on definition lines (`struct Coop<F: Future>`).

### `greeg outline FILE`

The file's symbols as a tree: kind, name, `:line`, `[pub,doc,test]`. When
the tree exceeds the budget, deeper levels collapse into `(+N nested)`.

### `greeg map [DIR]`

Subdirectories by best file rank, then files by import PageRank with
`←N` importers, symbol counts by kind and the top exported symbols.

### `greeg impact NAME`

Definitions, then files split into WILL BREAK (calls, type uses, imports in
source), MAY BREAK (member or bare identifier uses) and REVIEW (tests,
comments, strings, demoted files and files without a grammar), each with two
sample lines, followed by `callers --depth 2`.

### Verb JSON

`--json` emits one record per entry and a `footer`: `def`/`impl` records
(`path, line, kind, name, container, signature, doc, flags, supertypes,
score, reach, start, end`), `ref` (`kind, path, line, text, symbol,
file_flags, score`), `caller` (`path, symbol, kind, def_line, count, lines,
called_by`), `symbol` (outline), `dir`/`file` (map) and a single `impact`
record.

## JSON Lines (`--json`)

ripgrep's schema is preserved: `begin`, `match`, `end` per file, with
`data.path.text`, `data.lines.text`, `data.line_number`,
`data.absolute_offset` and `data.submatches[{match,start,end}]`. `start`/`end`
index into `lines.text`, which is the trimmed and clipped line; the untrimmed
byte offset of the match is `absolute_match_offset`.

Additional fields on `match`:

```json
"kind": "def|import|call|type|member|ident|doc|comment|string",
"symbol": {"name": "get_queryset", "kind": "method", "container": "BaseManager"},
"file_flags": ["test"],
"score": 0.73,
"clipped": false
```

Additional record types:

* `facets`: `{total, files, exact_length, by_kind, by_dir, by_lang, by_flag, definitions_total}` emitted before the shown files in facets layout.
* `footer`: `{hits_shown, hits_total, files_shown, files_total, demoted_files, demoted_hits, skipped_binary, skipped_huge, rung, rung_names, ignored_only, est_tokens, elapsed_ms, hints, layout}` (`rung_names` lists the names substituted by the split-token or fuzzy rung).

`--budget 0 --json` prints every hit in path order, which is the parity mode
checked against `rg --json` by `bench/parity.py`.

## Exit codes

0 hits found, 1 no hits (after the ladder), 2 error.
