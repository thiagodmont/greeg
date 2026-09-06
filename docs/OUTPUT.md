# greeg output formats (contract v2)

Every text layout follows one rule: **the path is printed once as a header,
hits sit beneath it as `  <line> <kind>  <text>  ‹ container`**. Nothing else
repeats the path. Examples below are real output (tokio, django corpora).

Row anatomy:

* `line` — right-aligned within its file group.
* `kind` — one of `def import call type member doc comment string`; blank for
  `ident`; the column is omitted when every row in the group is `ident` and in
  the definitions block (every row is a `def`).
* `text` — the line, leading indentation removed, clipped to `--max-columns`
  around the match with `…`.
* `‹ container` — the enclosing `Class.method` (last two links of the chain; a
  definition's own name is already in the text). Printed only when it differs
  from the previous row's. `--chain` prints the full `Outer › Inner › fn` chain.
* File header: `path` plus `[test]`, `[vendored]`, `[generated]`, `[mock]` when
  the file is demoted. No size, no age.
* Context lines (adaptive or `-A/-B/-C`) are `  <line>  <text>`, dedented by
  the hit line's own indentation; `...` marks a gap.

## Text (default)

Chosen by the shaper (DESIGN.md §6.4) from the mode and the budget.

### Content layout

Files in rank order, hits by score inside the file, then `+N more (…)`.
Adaptive context (never with `-A/-B/-C`, always clipped to the enclosing
definition): one **definition** hit → 20 lines; one call/ident hit → the
enclosing definition's signature line plus 6 lines; ≤ 3 hits → 2 before and
2 after; more → none. The hits counted here are the ones the answer is about,
after near-misses left it (see **related**).

```
tokio/src/net/windows/named_pipe.rs
   388 def  pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {  ‹ NamedPipeServer
   389          self.io.registration().poll_read_ready(cx).map_ok(|_| ())
  ...
  1182 def  pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {  ‹ NamedPipeClient
  1183          self.io.registration().poll_read_ready(cx).map_ok(|_| ())

tokio/src/io/async_fd.rs
  375 def  pub fn poll_read_ready<'a>(  ‹ AsyncFd
  376      &'a self,
  ...
  412 def  pub fn poll_read_ready_mut<'a>(
  413      &'a mut self,

tokio/tests/tcp_stream.rs  [test]
  231 def  async fn poll_read_ready() {
  232      let (mut client, mut server) = create_pair().await;

10/10 hits · 8/8 files · 2 files demoted (2) · ~572 tokens
```

`(shown before)` after a row means its context was printed earlier in this
session and is not repeated (only for adaptive context; `-C`, `--mode block`
and `--all` are never deduplicated).

### Facets layout (broad queries)

Used when content for every hit would exceed the budget and there are more
than 12 hits. Order: **definitions first** (tokens-to-first-definition is
what an agent pays for), then a two-line header, then top hits.

```
definitions (24 of 72)
django/contrib/contenttypes/fields.py
  667  def get_queryset(self):  ‹ create_generic_related_manager.GenericRelatedObjectManager
django/forms/models.py
   769  def get_queryset(self):  ‹ BaseModelFormSet
  1539  def _get_queryset(self):  ‹ ModelChoiceField
django/db/models/manager.py
   83  def _get_queryset_methods(cls, queryset_class):  ‹ BaseManager
  150  def get_queryset(self):
  212  def get_queryset(self):  ‹ EmptyManager
…
tests/backends/models.py  [test]
  23  def get_queryset(self):  ‹ SchoolClassManager
  36  def get_queryset(self):  ‹ SchoolBusManager
  +48 test definitions (--all)

get_queryset  357 hits · 73 files · def 72 call 159 ident 94 doc 12 comment 11 string 9 · test 190
areas  docs/topics 36  docs/ref 26  django/contrib 25  django/db 24  docs/releases 20  django/views 12
langs  py 266  txt 91

top hits
django/contrib/contenttypes/fields.py
  671 call  queryset = super().get_queryset()  ‹ GenericRelatedObjectManager.get_queryset
  685 call  queryset = super().get_queryset()._disable_cloning()  ‹ GenericRelatedObjectManager.get_prefetch_querysets
django/db/models/query.py
  3007 call  qs = manager.get_queryset()  ‹ prefetch_one_level

34/357 hits · 16/73 files · 37 files demoted (190) · ~947 tokens
next: --kind def | -g 'docs/topics/**' | --no-tests
```

* **Definitions**: source files first, ranked. Demoted definitions
  (test/vendored/generated/mock files) take at most a quarter of the block
  (three when there is no source definition) and the rest collapse to one
  line per group: `+48 test definitions (--all)`. Definitions take at most
  60 % of the budget when other hits exist.
* **Header line**: `<pattern>  N hits · M files · <kind counts> · <demoted
  flag counts>`. No `exact-length`.
* **areas**: the shortest distinguishing directory prefixes — every group
  holding more than a fifth of the hits is split into its subdirectories —
  top 6, source directories before test/vendored ones (a test directory is
  never first).
* **langs**: only when at least two languages hold ≥ 5 % of the hits each.
* **top hits**: up to 10 non-definition, non-import hits, best first, grouped
  by file. Import hits collapse into one line, `imported by 17 files:
  sync/mod.rs, bounded.rs, … (+11)` (up to six short names; the full path when
  a short name would repeat), and calls/types take the freed slots.

### related (near-misses)

A bare identifier searched literally and case-sensitively — no regex
metacharacters, no `-i`, no case-insensitive `-S` — is a question about that
identifier. When at least one match is the whole word, matches that sit inside
a *longer* identifier stop competing for answer lines and collapse to one line,
in every ranked layout, before the footer:

```
related  createSourceFileAndAssertInvariants 3  createSourceFileLike 2  createSourceFileWithText 2
```

Up to four identifiers, most hits first. The header and footer counts still
describe every match (`15/97 hits`), so the near-misses are visible as the
difference, and querying one of the named identifiers reaches them. Nothing is
suppressed when no match is a whole word (the near-misses are then the answer),
under `-w` (there are none), or in parity mode (`--budget 0`, `-l`, `-c`),
which keeps ripgrep's match set exactly.

### Outline layout (`--mode outline`)

Grouped by file: `  <line> <kind>  container › text`, then `+N more`.

### Block layout (`--mode block`)

Content layout, but each definition hit prints its whole body (capped at 200
lines, deduplicated per definition), dedented by the signature's
indentation. Bodies count against the budget: a body that does not fit is
cut (at least three lines are kept) and ends with `… body clipped (--budget)`.

```
tokio/src/io/async_fd.rs
  ── method AsyncFd › poll_read_ready (lines 375–377)
  375  pub fn poll_read_ready<'a>(
  376      &'a self,
  377      cx: &mut Context<'_>,
  … body clipped (--budget)
```

### Files / count (`-l`, `-c`, `--mode files|count`)

Pipe-safe, exactly ripgrep's shape on stdout: `-l` prints one bare path per
line, `-c` prints `path:count` (matched lines per file). The list is never
truncated by `--budget`. Order: source files first (file prior), then path;
`--sort path` or `--budget 0` gives path order. The footer goes to
**stderr** as one line without `next:`:

```
$ greeg -l -w poll_read_ready
tokio/src/net/windows/named_pipe.rs
tokio/src/net/udp.rs
…
16/16 hits · 16/16 files · 4 files demoted (4) · ~242 tokens        (stderr)
```

### `--budget 0` (parity mode)

Unlimited, path order, ripgrep's text shape: `path:line:text` with the
untrimmed line, `path-line-text` for `-A/-B/-C` context lines and `--`
between non-adjacent groups. No headers on stdout; the footer goes to stderr.

```
tokio/src/net/tcp/stream.rs-555-    /// [`readable`]: method@Self::readable
tokio/src/net/tcp/stream.rs:556:    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
tokio/src/net/tcp/stream.rs-557-        self.io.registration().poll_read_ready(cx).map_ok(|_| ())
```

### stdin

With no path argument and a readable non-tty stdin (pipe, socket, or a
regular file with data — `grep_cli::is_readable_stdin`), greeg searches stdin
like ripgrep: lines only (`-n` adds `N:`), `-c` prints the count, `-l` prints
`<stdin>`, `--json` uses `"path":{"text":"<stdin>"}`. No index, no ladder, no
facets, no footer. `printf 'a\nb\n' | greeg a` prints `a`.

### Footer

```
34/357 hits · 16/73 files · 37 files demoted (190) · skipped 3 binary, 0 huge · matched case-insensitive · ~947 tokens
next: --kind def | -g 'docs/topics/**' | --no-tests
```

* `shown/total hits · shown/total files`, `N files demoted (hits)`, `skipped`
  only when non-zero, `matched <rung>` when the escalation ladder relaxed the
  query (`without word boundary`, `case-insensitive`, `split tokens → names`,
  `fuzzy → names`) or hits exist only in ignored/hidden files.
* `~N tokens` is the estimate for the whole output including the footer.
* `· N ms` only with `--stats`.
* `next:` lists **flags only**, never the pattern: `-w`, `--kind def`,
  `-g '<area>/**'` (never a test or vendored area), `--no-tests`, `--budget
  2N` when hits were cut. `no definition in searched files (a dependency or
  generated code?)` appears only when the pattern is an identifier and no
  definition matched.

### Budget accounting

Every emitted line is charged: file headers (plus a reserve for `+N more`),
hit rows (text, line number, kind, container), context and signature lines,
collapsed-definition lines, the `imported by` and `related` lines, the facets
header, and the footer with its hints. Block bodies are charged line by line and cut to
fit. The `-l`/`-c` lists are exempt.

### Token estimate

`greeg_query::tokens::estimate` is a byte-feature model fitted against
o200k_base on 20 greeg outputs (five corpora, four layouts): word pieces
split at `_` and case changes, bucketed by length and case (short lowercase
1.82, mid 0.72, long 4.49, capitalised 1.01), digit groups of three 1.80,
punctuation 0.20 per character plus 0.52 per run, non-ASCII characters 3.35,
underscores −0.45. Fitted ratio estimate/o200k: min 0.945, max 1.054, mean
1.00; on the review query set after fitting: 0.98–1.06. The same function
prices hit rows during shaping. Plain byte divisors were dropped: Rust
output tokenizes at ~3.0 bytes/token, Kotlin/Python prose at ~4.0, which a
single divisor cannot reconcile.

## Verbs

Every verb accepts the search flags (`--budget`, `--json`, `--no-tests`,
`--root`, `--fresh`, `--chain`, `--stats`, …) before or after the verb. All
work without an index through scan mode except `map`. Timings appear only
with `--stats`.

### `greeg def NAME [--from FILE] [--def-kind KIND]`

```
def JoinHandle  7 of 18 definitions · index
tokio/src/runtime/task/join.rs
  163 struct  pub struct JoinHandle<T> {  [exported]
              "An owned permission to join on a task (await its termination)."
tokio/src/blocking.rs
  37 struct  pub(crate) struct JoinHandle<R> {
  41 impl    unsafe impl<T: Send> Send for JoinHandle<T> {}  : Send
  44 impl    impl<R> Future for JoinHandle<R> {  : Future
tokio/src/fs/mocks.rs  [mock]
  127 struct  pub(super) struct JoinHandle<T> {
tokio/src/runtime/tests/task_combinations.rs  [test]
  63 variant  CombiAbortSource › JoinHandle,
  +11 more (raise --budget)
next: refs JoinHandle | callers JoinHandle | outline tokio/src/runtime/task/join.rs
```

Rows: `line kind  [name  ]container › signature  [exported,test] : supertypes`;
file flags live in the header. A doc first line follows in quotes. `reach`
is printed only when `--from` was given. `impl` blocks are capped at three
when other kinds exist. With no exact name the ladder tries case-insensitive,
split tokens and fuzzy names and reports `matched fuzzy → spawn_blocking`.

### `greeg refs NAME`

Hits grouped by kind, then by file; the budget is split across kinds in
proportion to √count. Imports collapse to one line.

```
refs Semaphore  345 hits · 33 files · 60% resolved · index
defined at  tokio/src/sync/semaphore.rs:427 (struct)  tokio/src/sync/batch_semaphore.rs:35 (struct)  …
imported by 17 files: sync/mod.rs, bounded.rs, rwlock.rs, once_cell.rs, poll_semaphore.rs, read_guard.rs (+11)

call (2)
tokio/src/sync/mpsc/bounded.rs
  161  let semaphore = Semaphore {  ‹ channel

type (171)
tokio/src/sync/batch_semaphore.rs
   73  semaphore: &'a Semaphore,  ‹ Acquire.semaphore
  622  fn new(semaphore: &'a Semaphore, num_permits: usize) -> Self {  ‹ Acquire.new
  +159 more

75/345 hits · next: callers Semaphore | impact Semaphore | refs Semaphore --kind call
```

`resolved` is the share of hits whose file imports (directly or within two
hops) a file defining the name.

### `greeg callers NAME [--depth 2]`

Grouped by file: `  <def line> <kind>  Class.method ×N (lines …)`; `--depth 2`
adds `← called by …` beneath each caller.

```
callers spawn_blocking  67 call sites in 60 functions · 27 files · index
tokio/src/fs/mod.rs
  320 fn  asyncify ×1 (lines 325)
tokio/src/task/join_set.rs
  254 method  JoinSet.spawn_blocking ×1 (lines 260)
  269 method  JoinSet.spawn_blocking_on ×1 (lines 275)
```

### `greeg impls NAME`

Definitions whose supertype list contains NAME (`impl T for X`, `: T`,
`extends`/`implements`, Python bases), grouped by file, then a low-confidence
section of type-position hits on definition lines (`struct Coop<F: Future>`).

### `greeg outline FILE [--imports]`

The file's symbols as a tree: `kind name  :line  [pub,doc,test]`. The
`imports` line is printed only with `--imports` or when there are at most
three. When the tree exceeds the budget, deeper levels collapse into
`(+N nested)`.

```
outline tokio/src/sync/oneshot.rs  83 symbols · 21 imports · rust · index
  struct Sender  :222  [pub,doc]
    field inner  :223
  mod error  :339  [pub]
    struct RecvError  :348  [doc]
```

### `greeg map [DIR]`

Subdirectories by best file rank, then files by import PageRank with
`←N` importers, symbol counts by kind and the top exported symbols; ranks
have two decimals.

### `greeg impact NAME`

Definitions (grouped by file), then files split into WILL BREAK (calls, type
uses, imports in source), MAY BREAK (member or bare identifier uses) and
REVIEW (tests, comments, strings, demoted files and files without a grammar),
each file with its kind counts and two sample lines, followed by
`callers --depth 2` grouped by file.

```
WILL BREAK (14 files, 216 hits) — calls, type uses or imports in source
tokio-util/src/sync/poll_semaphore.rs  import 1, type 6, doc 9
   6  use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, TryAcquireError};
  14  semaphore: Arc<Semaphore>,
```

### Verb JSON

`--json` emits one record per entry and a `footer`: `def`/`impl` records
(`path, line, kind, name, container, signature, doc, flags, supertypes,
score, reach, start, end`), `ref` (`kind, path, line, text, symbol,
file_flags, score`), `caller` (`path, symbol, kind, def_line, count, lines,
called_by`), `symbol` (outline), `dir`/`file` (map) and a single `impact`
record.

## JSON Lines (`--json`)

ripgrep's schema: `begin`, `match`, `context`, `end` per file, then
`summary`, then greeg's `footer`. stdout carries only JSON records.

* `match.data`: `path.text`, `lines.text` (the **raw untrimmed line including
  its terminator**, as ripgrep), `line_number`, `absolute_offset` (byte offset
  of the line start), `submatches[{match.text, start, end}]` — every match
  on the line, offsets relative to `lines.text`. Extra fields:

  ```json
  "kind": "def|import|call|type|member|ident|doc|comment|string",
  "symbol": {"name": "get_queryset", "kind": "method", "container": "BaseManager"},
  "file_flags": ["test"],
  "score": 0.73,
  "clipped": false
  ```

* `context` records (`-A/-B/-C`): `{"type":"context","data":{"path","lines","line_number","absolute_offset","submatches":[]}}`,
  interleaved with matches in line order.
* `end.data.stats`: `matched_lines` (the file's matched lines), `matches`
  (submatches printed), `bytes_searched` (file size), `shown`.
* `summary`: `{"type":"summary","data":{"elapsed_total":{secs,nanos,human},"stats":{elapsed,searches,searches_with_match,bytes_searched,bytes_printed,matched_lines,matches,matched_lines_shown}}}`.
* `facets`: `{total, files, by_kind, by_dir, by_lang, by_flag, definitions_total, demoted_definitions, imported_by}` emitted before the shown files in facets layout.
* `footer`: `{hits_shown, hits_total, files_shown, files_total, demoted_files, demoted_hits, skipped_binary, skipped_huge, rung, rung_names, ignored_only, est_tokens, elapsed_ms, hints, related, layout}`; `related` is `[[identifier, hits], …]`, the near-misses of the **related** section (empty in parity mode).

`--budget 0 --json` prints every hit in path order with the same record
count and (path, line) set as `rg --json`, which `bench/parity.py` checks.

## Ripgrep flags

Accepted and honoured: `-i -S -s -w -x -F -U -n -l -c -A -B -C -g -t -T -j
--no-ignore --hidden --max-columns --max-filesize --json -e/--regexp
(repeatable; patterns are joined as `(?:p1)|(?:p2)`, each escaped under
`-F`; leading-dash patterns work) --sort path -u/-uu`.

Accepted and ignored (cosmetic in ripgrep, meaningless here): `-N
--no-line-number`, `-H --with-filename`, `-h --no-filename` (help is
`--help`), `--heading --no-heading`, `--color`, `--colors`, `-p --pretty`,
`--column --no-column`, `--sort`/`--sortr` with a kind other than `path`
(stderr note), `--no-messages`, `--trim`, `--line-buffered`,
`--block-buffered`, `-a --text`, `--no-config`.

## Exit codes

0 the pattern matched as given; 1 no hits, **including** when only an
escalation rung (without word boundary, case-insensitive, split tokens,
fuzzy) produced results — the answer is still printed with `matched <rung>`;
2 error (bad regex, missing path).
