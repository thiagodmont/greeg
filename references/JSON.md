# greeg JSON output

greeg writes JSON Lines in three dialects:

| Flag | Dialect | For |
|---|---|---|
| `--json=greeg` | greeg's own records, schema 1 (this page) | agents and tools that read greeg |
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

## Schema 1 (`--json=greeg`)

Every line is one record, `{"type": …, "data": {…}}`. The first record names
the schema and the command:

```json
{"type":"greeg","data":{"schema":1,"dialect":"greeg","command":"search","version":"0.10.0"}}
```

`command` is `search` (also for stdin) or the verb: `def`, `refs`, `callers`,
`impls`, `impact`, `show`, `outline`, `map`. Other commands, such as `stats`
and `index`, refuse `--json=greeg` (exit 2).

The last record is a `footer` with an `outcome`. A run that fails before it
answers writes nothing to stdout and exits 2. The exception is `map` while the
index it needs is rebuilt: its footer then holds only
`"outcome":{"exit":2,"rebuilding":{"reason":…,"estimate_ms":…}}`.

**Compatibility.** A new record type or field keeps `schema: 1`; consumers
should ignore what they do not know. Removing or renaming a field or a record
type, or changing a field's meaning, bumps the schema.

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
| `total` | eligible results: matched lines for a search, entries for a verb |
| `shown` | results the answer shows |
| `complete` | `shown` is all of `total`; false only when the budget left results out |
| `source` | `index`, `index (phase 1)`, `scan`, or `text` (`show`) |
| `fresh` | the freshness check an index answer ran (`ttl`, `stat`, `fsevents`, `none`), empty for a scan |
| `deferred` | files changed since the index was published, read from disk |

`show`, `outline` and `map` answer a location, a file or a directory: they
exit 0 whenever they answer, even with nothing to show.

### Search

Searches are shaped by the budget, as text is: `outcome.complete` says
whether matches were left out, and `--budget 0` shows every one.

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
identifiers), `layout`, `outcome`.

### Verbs

**`def`** records (`def`, and the definitions `refs` found): `path`, `line`,
`kind`, `name`, `container`, `signature`, `doc`, `flags` (`exported`, `test`,
`generated`, `vendored`), `supertypes`, `score`, `reach`, `start` and `end`
(byte offsets of the definition). Footer: `name`, `suggestions`, `elapsed_ms`,
`outcome`.

**`impl`** records (`impls`): a `def` record's fields plus `confidence`
(`high` for a direct implementation, `low` for a type-position use). Footer:
`name`, `direct`, `extras`, `elapsed_ms`, `outcome`.

**`ref`** records (`refs`, after its `def` records): `kind`, `path`, `line`,
`text` (`Text`), `symbol` (enclosing definition), `file_flags`, `score`.
Footer: `name`, `files_total`, `by_kind`, `resolved`, `classified`,
`elapsed_ms`, `outcome`.

**`caller`** records (`callers`): `path`, `symbol`, `kind`, `def_line`, `count`,
`lines`, `file_flags`, `called_by`. Footer: `name`, `call_sites`, `files`,
`elapsed_ms`, `outcome`.

**`impact`**, one record: `name`, `definitions` (`def` records' fields),
`will_break`, `may_break`, `review` (files: `path`, `hits`, `kinds`,
`file_flags`, `sample` as `[line, Text]`), `callers` (`caller` records'
fields). Footer: `name`, `files`, `total_hits`, `elapsed_ms`, `outcome`.

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
