# greeg JSON output

greeg writes JSON Lines in three dialects:

| Flag | Dialect | For |
|---|---|---|
| `--json=greeg` | greeg's own records, schema 2 (this page) | agents and tools that read greeg |
| `--json=rg` | ripgrep's records, nothing else | tools written for `rg --json` |
| `--json=legacy` | ripgrep's records plus greeg's fields | consumers of greeg 0.9 and earlier |

`--json=rg` and `--json=legacy` are described under
[JSON output](ARCHITECTURE.md#json-output).

## Migrating from bare `--json`

Bare `--json` names no dialect, so its meaning changes:

| Release | Bare `--json` | `--json=legacy` |
|---|---|---|
| 0.10 | `legacy`; on a terminal, stderr says it will change | available |
| 0.11 | `greeg` | available |
| 0.12 | `greeg` | removed |

Name a dialect so the release does not choose for you: `--json=greeg` and
`--json=rg` stay, and `--json=legacy` keeps today's records through 0.11 only. The notice goes only to a terminal, never to a pipe, so scripts and
agents see no change in 0.10. `greeg stats --json` is a report of its own and
does not change.

From `legacy` to `greeg`:
- Read the header first; `command` says which records follow.
- A search's `match` records no longer carry the path: take it from the
  `begin` before them. `line_number` is `line`, `absolute_offset` is
  `byte_offset` (check `begin.coordinates`), `lines` is `text` without its
  terminator, and `submatches` are `[start, end]` pairs.
- `end` and `summary` records are gone; `begin.matched_lines` and the footer
  hold the counts. `hits_total`, `hits_shown`, `rung` and `source` are in the
  footer's `outcome` (`total`, `shown`, `rung`, `source`).
- `def` and `impl` records are `{"type", "data"}` like the rest, and every
  path is `{"text"}` or `{"bytes"}`.
- `impact`'s `will_break` and `may_break` are `likely` and `possible`. Legacy
  keeps its names but fills them with the same groups.

## Schema 2 (`--json=greeg`)

Every line is one record, `{"type": …, "data": {…}}`. The first record names
the schema and the command:

```json
{"type":"greeg","data":{"schema":2,"dialect":"greeg","command":"search","version":"0.11.0"}}
```

`command` is `search` (also for stdin) or the verb: `def`, `refs`, `callers`,
`impls`, `impact`, `show`, `outline`, `map`. Other commands, such as `stats`
and `index`, refuse `--json=greeg` (exit 2).

The last record is a `footer` with an `outcome`. A run that fails before it
answers writes nothing to stdout and exits 2. The exception is `map` while the
index it needs is rebuilt: its footer then holds only
`"outcome":{"exit":2,"rebuilding":{"reason":…,"estimate_ms":…}}`.

**Compatibility.** A new record type or field keeps the schema number;
consumers should ignore what they do not know. Removing or renaming a field
or a record type, or changing a field's meaning, bumps it.

**From schema 1** (greeg 0.10): only the `impact` record changed. Its
`will_break` and `may_break` became `likely` and `possible`, graded by import
evidence rather than syntax alone, and it gained `import_graph`. Every other
record is as in schema 1.

### Text

Paths and file content are a `Text`: `{"text": "…"}` when the bytes are
UTF-8, otherwise `{"bytes": "<base64>"}`, so the exact bytes always come back.
Paths are relative to the root.

Identifiers and summaries are plain strings: symbol `name`, `container`,
`symbol`, `signature`, `doc`, `imports`, `hints`, and the directory and
language names in `facets`. They come from the symbol index and display
text, which hold UTF-8 (a byte that is not UTF-8 shows as U+FFFD).

Scores, ranks and `elapsed_ms` are rounded to three decimals.

### Outcome

Every footer has `outcome`:

| Field | Meaning |
|---|---|
| `exit` | the exit status of the run: 0, or 1 when nothing exact was found |
| `exact` | the query matched as given (`rung` is `exact`) |
| `rung` | `exact`, or the relaxed rung that answered |
| `total` | eligible results: matched lines for a search, entries for a verb (for `impact`, referring files plus callers) |
| `shown` | results the answer shows |
| `complete` | `shown` is all of `total`; false only when the budget or `--max-bytes` left results out (or, for `show`, clipped a body) |
| `truncated_by` | `"bytes"` when `--max-bytes` cut the answer, `"budget"` when it is otherwise not complete, else null |
| `source` | `index`, `index (phase 1)` or `scan`; `parse` or `regex` for a file `outline` read itself; `text` for `show` read from the file, or whose locations were not all read one way |
| `fresh` | the freshness check an index answer ran (`ttl`, `stat`, `fsevents`, `none`; for `show`, the weakest of its locations), empty when files were read directly (a scan, or `show` and `outline` without the index) |
| `deferred` | files changed since the index was published, read from disk |

`show`, `outline` and `map` answer a location, a file or a directory: they
exit 0 whenever they answer, even with nothing to show.

### Budget

A non-zero `--budget` bounds the estimated tokens of the JSON as written, not
of the text answer: JSON shows fewer results than text at the same budget. A
search is shaped to a budget whose JSON fits; a verb lists the rows that fit.
Below the smallest answer (header, counts and footer), that answer is written
anyway. `outcome.complete` says whether results were left out. `--budget 0`,
`-l`, `-c` and searches of piped input are never budgeted: they write every
result, as ripgrep does.

`outcome` counts a verb's results. The definitions `refs` and `impact` list
beside them are context: `definitions_total` says how many there are.

`--max-bytes N` bounds the JSON's bytes too: the answer is fitted to the budget,
then cut to the most results whose records fit in N bytes, with
`"truncated_by": "bytes"` and a `--max-bytes` hint. Records are never split.
When even the smallest answer is larger, nothing is written and greeg exits `2`.

### Search

**`facets`** (ranked layouts only, before the files): `total`, `files`,
`by_kind`, `by_dir`, `by_lang`, `by_flag` (`[name, count]` pairs),
`definitions_total`, `demoted_definitions`, and `imported_by` (`Text` paths).

**`begin`**, one per file, before its lines:

| Field | Meaning |
|---|---|
| `path` | `Text` |
| `encoding` | `utf-8`, `utf-8-bom`, `utf-16le` or `utf-16be` |
| `coordinates` | `bytes`: offsets index the file's bytes, a UTF-8 BOM included. `decoded`: they index the UTF-8 decoded from UTF-16, without its BOM |
| `file_flags` | `test`, `vendored`, `generated`, … |
| `matched_lines` | matched lines in the file, shown or not |
| `binary_offset` | only when a NUL byte past the first 64 KiB (counted after a UTF-8 BOM) ended the search at the start of its line: the NUL's offset ([binary files](ARCHITECTURE.md#binary-files)) |

Then the file's lines in line order. They belong to the file of the
`begin` before them.

**`match`**: `line` (1-based), `byte_offset` (start of the line, in the file's
coordinates), `text` (`Text`, the line without its `\n` or `\r\n`),
`submatches` (`[start, end]` byte ranges within `text`), `kind` (`def`,
`call`, `type`, …), `symbol` (`{name, kind, container}` of the enclosing
definition, or null), `score`, `clipped`.

**`context`** (with `-A`, `-B`, `-C`): `line`, `byte_offset`, `text`.

**`file`** (with `-l` or `-c`, in place of `begin` and lines): `path`,
`count` (matched lines) under `-c`, and `binary_offset` as in `begin`.

**`footer`**: `files_shown`, `files_total`, `demoted_files`, `demoted_hits`,
`skipped_binary`, `binary_tails` (files whose search a later NUL ended; only
when non-zero), `skipped_huge`, `rung_names` (the names a relaxed rung
used), `ignored_only` (`[files, hits]` found only in ignored or hidden files,
or null), `ignored_partial` (those counts are a lower bound), `est_tokens`, `elapsed_ms`, `hints`, `related` (`[name, count]` longer
identifiers), `layout`, `outcome`. In a budgeted answer `est_tokens` is the
JSON's own estimate, so it varies with the digits of `elapsed_ms`; unbudgeted
answers (`-l`, `-c`, `--budget 0`) keep the text's estimate.

### Verbs

**`def`** records (`def`, and the definitions `refs` found): `path`, `line`,
`kind`, `name`, `container`, `signature`, `doc`, `flags` (`exported`, `test`,
`generated`, `vendored`), `supertypes`, `score`, `reach`, `start` and `end`
(byte offsets of the definition). Footer: `name`, `suggestions` (near names
when nothing matched, cut to the budget), `suggestions_total`, `elapsed_ms`,
`outcome`.

**`impl`** records (`impls`): a `def` record's fields plus `confidence`
(`high` for a direct implementation, `low` for a type-position use). Footer:
`name`, `direct`, `extras`, `elapsed_ms`, `outcome`.

**`ref`** records (`refs`, after its `def` records): `kind`, `path`, `line`,
`text` (`Text`), `symbol` (enclosing definition), `file_flags`, `score`.
Footer: `name`, `definitions_total` (at least three of them are listed when
there are; more as the budget allows), `files_total`, `by_kind`, `resolved`,
`classified`, `elapsed_ms`, `outcome`.

**`caller`** records (`callers`): `path`, `symbol`, `kind`, `def_line`, `count`,
`lines`, `file_flags`, `called_by`. Footer: `name`, `call_sites`, `files`,
`elapsed_ms`, `outcome`.

**`impact`**, one record: `name`, `definitions` (`def` records' fields), the
files that use the name in three groups, `import_graph`, and `callers`
(`caller` records' fields, matched by name; cut to the budget, as
`outcome.shown` counts).
- `likely`: a call, type use or import, in a source file that is a
  definition's file, imports one, or imports a module that does;
- `possible`: any other use in source;
- `review`: tests, demoted files, files without a grammar, comments and
  strings.

Each file has `path`, `hits`, `kinds`, `file_flags` and `sample` (`[line,
Text]`). `import_graph` is false when the answer had no import graph (a scan, or
an index without symbols): `likely` is then empty. The budget cuts the groups
and `definitions` as text cuts them. Footer: `name`, `definitions_total`,
`files`, `callers_total`, `total_hits`, `elapsed_ms`, `outcome`.

**`show`** records: `path`, `line` (as asked), `symbol` (the enclosing
definition or null), `start_line`, `end_line`, `shown_to`, `clipped`, `text`
(`Text`, the lines joined by `\n`). Footer: `elapsed_ms`, `outcome`.

**`symbol`** records (`outline`): `name`, `kind`, `line`, `start`, `end`,
`container`, `depth`, `flags`. Footer: `path`, `imports`, `parse_errors`,
`elapsed_ms`, `outcome`.

**`dir`** and **`file`** records (`map`): `dir` has `path`, `files`,
`symbols`, `rank`; `file` has `path`, `rank`, `symbols`, `imported_by`,
`by_kind`, `top` (`[kind, name]`), `file_flags`. Footer: `dir`, `files_total`,
`symbols_total`, `dirs_total`, `graph_changes`, `elapsed_ms`, `outcome`.

## Capabilities

`greeg --capabilities` prints one record, `{"type":"capabilities","data":…}`,
so an adapter can check what the binary on PATH supports before it relies on a
flag. Its `schema` (1) is versioned apart from the answers'. It takes no
other argument (exit 2), reads no index and records nothing.

| Field | Meaning |
|---|---|
| `version`, `index_format` | the build and the index format it reads and writes |
| `json` | `dialects`, `greeg_schema`, and what bare `--json` means (`bare`) |
| `global` | long flags every search and command accepts (`--budget`, `--json`, `--max-bytes`, …) |
| `search` | `args` (positional names) and `flags` (its own long flags) of a search |
| `commands` | the same for each command, with nested `commands` (`hook claude`, `stats replay`, …); a flag is listed once, by the command that declares it, and its subcommands accept it too (`stats --since`) |
| `budget` | `levels`, the `default` in effect and its `source` (`env`, `config`, `default`), `max_bytes` |
| `matching`, `fresh` | matching policies; freshness modes, and whether `fsevents` is native (macOS) or falls back to `stat` |
| `languages` | `builtin` languages with full syntax support; `extra` runtime languages with `name`, `extensions` and `ready` (grammar and tags query load) |
| `agents` | `greeg hook` targets |

Commands and flags come from greeg's argument parser, so every one listed
parses. A run can still refuse a combination, or a flag a command has no use
for (`--json=rg` with a command, for one). Hidden compatibility flags are left
out. New
fields do not change `schema`.

## Explain

`--explain` on a search adds one `explain` record before the footer, in
`--json=greeg` and `--json=legacy` (text writes the same as `explain:` lines on
stderr). It says what the answer was built from and changes nothing else: the
other records, the budget fit and the byte ceiling are as without it, and it is
not counted in either. Under `--max-bytes` the record goes to stderr, so
stdout keeps its ceiling. Statistics do not record it. `--json=rg` and commands
refuse it (exit 2).

| Field | Meaning |
|---|---|
| `source` | as the outcome's |
| `index_skipped` | why the index did not answer, or null: `not used` (`--no-index`), `ignored or hidden files asked for`, a rebuild reason (`no-index`, `derivation`, `threshold`, `corrupt`, …), `an unindexed file named src/x.rs` (or directory), `a path outside the index root or not resolvable: PATH`, `files the index skipped`, `error`, … |
| `fresh` | index answers: `method`, `ms`, `changed`, `deferred` |
| `plan` | index answers: the candidate plan (words and trigram hashes) |
| `rung` | the rung that matched |
| `candidates` | `walked`, `candidates` (index), `searched`, `matched_files`, `hits_before_kind`, `hits` |
| `filters` | `kinds`, `excluded` (`--no-tests` …), `globs`, `types`, `types_not`, `demoted` with `demoted_files`/`demoted_hits`, `skipped_binary`, `binary_tails`, `skipped_huge`, `ignored_only` |
| `ranking` | `terms` and, for the first ten shown hits, `path`, `line`, `kind`, `score`, `kind_weight`, `exact_boost`, `prior` (location, `--near` and PageRank); `score` is the product of the terms |
| `parse_errors` | index answers: matched files parsed with errors or by the regex fallback |

The plan holds the query's words; the record goes only to the caller.
