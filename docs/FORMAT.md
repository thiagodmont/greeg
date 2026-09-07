# greeg index on-disk format (version 5)

Location: `~/Library/Caches/greeg/<name>-<hash16>/` on macOS,
`$XDG_CACHE_HOME/greeg/...` elsewhere (override: `GREEG_INDEX_DIR`,
`--index-dir`). `<hash16>` is the first 16 hex chars of blake3 of the
canonical repository path.

```
manifest            JSON (see Manifest in greeg-index/src/lib.rs)
LOCK                flock target: every writer (build publish, delta apply) holds it exclusively
BUILDING            marker with the pid of a running build (create_new; stale after 10 min)
REFRESHING          marker with the pid of a running `index --refresh` (create_new; stale after 30 s)
files.<gen>.bin     file table (republished by phase 2 with ranks and parse flags)
grams.<gen>.bin     trigram dictionary + postings
words.<gen>.bin     word dictionary + postings (whole-word queries)
symbols.<gen>.bin   symbols, names, split tokens (phase 2)
spans.<gen>.bin     noncode spans and imports per file (phase 2)
graph.<gen>.bin     import graph CSR + PageRank (phase 2)
delta/NNNN.bin      delta segments 0001..manifest.deltas, applied in order
session/<id>.jsonl  session memory (DESIGN.md §9)
```

Every component is written to a unique temp name (`<name>.<pid>.<n>.tmp`)
and renamed into place. A build publishes its components, then the manifest,
and only then deletes `delta/` and the previous generation; a delta apply
writes `delta/NNNN.bin`, then the manifest. Both run under `LOCK` and re-read
the manifest first: an apply whose generation, phase or delta count no longer
matches the index it was computed from is dropped (the query reopens the
index instead); phase 2 keeps the delta count that queries added while it
was parsing. Readers never lock: `Index::open` maps only the deltas the
manifest names and ignores stray files, and any truncated section or corrupt
tombstone bitmap fails the open (the query answers in scan mode and a rebuild
is spawned).

Every `.bin` starts with a 16-byte header: `"GREEG"`, format `u16` LE,
component `u8` (1 files, 2 grams, 3 delta, 5 symbols, 6 spans, 7 graph, 8 words),
payload length `u64` LE. A format mismatch means rebuild; there is no
migration. All integers are little-endian; every array is padded to 8 bytes
so the tables can be viewed as `&[u32]`/`&[Rec]` directly from the mmap
(`bytemuck`), with nothing deserialized eagerly. `NONE = 0xFFFF_FFFF`.

## files.<gen>.bin (component 1)

```
u32 n_files, u32 n_dirs, u32 arena_len, u32 pad
FileRec[n_files]  32 bytes: path_off u32, path_len u16, lang u8, flags8 u8,
                  size u64, mtime_ns i64, dir u32, flags u16, rank u16
DirRec[n_dirs]    16 bytes: path_off u32, path_len u16, pad u16, mtime_ns i64
arena             UTF-8 relative paths ('/'-separated), padded to 8
u32 n_huge, u32 n_hidden
u32 huge[n_huge]      segment-local ids of files above 4 MiB (no grams; always a
                      candidate so verification opens or counts them)
u32 hidden[n_hidden]  segment-local ids of tracked-only files, padded to 8
```

File ids are the position in `FileRec[]`, assigned in sorted-path order.
`flags` is the `greeg_lang::FileFlags` bitset (test, generated, vendored,
minified, binary, lockfile, huge, parse_errors, ...). `rank` is the
quantized PageRank: 0 unknown, else `1 + rank_norm × 65534`. Files flagged
binary or huge have no grams but stay in the table so the freshness pass
tracks them. Tracked-only files are the ignore files (`.gitignore`,
`.ignore`, `.rgignore`) the walker would otherwise skip as hidden: they are
never candidates, but a change to one forces a rebuild so the ignore rules
are re-evaluated.

## grams.<gen>.bin (component 2)

```
u32 n_grams, u32 pad, u64 postings_len
u32 keys[n_grams]        trigram key = b0<<16 | b1<<8 | b2 (ASCII case-folded), sorted
u32 counts[n_grams]      documents per gram
u64 offsets[n_grams+1]   byte offsets into postings (8-byte aligned by construction)
postings               roaring bitmap (portable serialization) per gram, file ids
```

Grams never span a line terminator. Lookups binary-search `keys` and
deserialize one bitmap.

## words.<gen>.bin (component 8)

```
u32 n_words, u32 arena_len, u64 postings_len
u32 word_off[n_words+1]   into arena; words sorted bytewise, case preserved
u8  arena                 padded to 8
u32 counts[n_words]       documents per word, padded to 8
u64 offsets[n_words+1]    byte offsets into postings
postings                  roaring bitmap (portable serialization) per word, file ids
```

A word is a maximal run of `[A-Za-z0-9_]` of 2 to 64 bytes; bytes ≥ 0x80
separate words. Written in phase 1 next to the grams (DESIGN.md §3.2). A
whole-word query (`-w NAME`, a bare identifier in a ranked layout, `refs`,
`\bNAME\b`) binary-searches the dictionary and reads one bitmap per
alternative instead of intersecting trigram lists; `-i` and words the
dictionary cannot hold keep the trigram plan. The `related` line of a bare
identifier is a `memmem` pass over the arena for words containing the query.

## symbols.<gen>.bin (component 5)

```
u32 n_files, n_syms, n_names, n_super, n_tokens, n_tok_names, arena_len, tok_arena_len
u32 sym_off[n_files+1]       CSR: symbols of file f are syms[sym_off[f]..sym_off[f+1]]
SymRec[n_syms]               40 bytes, file order, start order within a file:
                               name_id u32, file u32 (segment-local), start u32, end u32,
                               name_start u32, line u32, parent u32 (symbol id or NONE),
                               super_off u32, kind u8, flags u8, name_len u16,
                               super_len u8, pad[3]
u32 supers[n_super]          name ids of supertypes (per symbol: super_off, super_len)
u32 name_off[n_names+1]      into arena; names sorted bytewise (binary search = exact
                               and prefix lookup); name_id = position
u8  arena[arena_len]
u32 by_name_off[n_names+1]   into by_name
u32 by_name[n_syms]          symbol ids grouped by name id, best first (kind weight,
                               exported, not test, file rank)
u32 tok_off[n_tokens+1]      split tokens (camel/snake parts, lowercase, ≥ 3 chars), sorted
u8  tok_arena[tok_arena_len]
u32 tok_names_off[n_tokens+1]
u32 tok_names[n_tok_names]   name ids containing each token
```

`kind` codes: 0 fn, 1 method, 2 class, 3 struct, 4 enum, 5 trait,
6 interface, 7 type, 8 mod, 9 object, 10 impl, 11 const, 12 var, 13 macro,
14 field, 15 variant. `flags`: 1 exported, 2 has_doc, 4 test, 8 object-literal member (JS/TS). `[start, end)`
is the declaration node (annotations and decorators included); `line` is the
line of the name. The per-file slice doubles as the `defs` span table of
DESIGN.md §3.4: the enclosing symbol of an offset is a `partition_point` on
`start` plus a backward scan on `end`.

## spans.<gen>.bin (component 6)

```
u32 n_files, u32 n_nc, u32 n_imp, u32 arena_len
u32 nc_off[n_files+1]; NcRec[n_nc]    8 bytes: start u32, end_kind u32 = end | kind<<30
                                        (kind 0 comment, 1 string, 2 docstring)
u32 imp_off[n_files+1]; ImpRec[n_imp] 20 bytes: start u32, end u32, target u32 (file id
                                        or NONE), raw_off u32, raw_len u16,
                                        info u16 (bit 0 wildcard, bits 1.. name count)
u8  arena                             raw module strings (`a.b.c`, `./x`, `crate::a::b`)
```

Both per-file tables are sorted by `start`; lookups are one `partition_point`.

## graph.<gen>.bin (component 7)

```
u32 n_files, u32 n_edges, u32 pad, u32 pad
u32 out_off[n_files+1]; u32 out_to[n_edges]; u16 out_w[n_edges]
u32 in_off[n_files+1];  u32 in_from[n_edges]
f32 rank[n_files]                      PageRank, damping 0.85, 20 iterations, max = 1
```

Edges come from resolved imports (weight = 10 × imported names) and, for
Kotlin, same-package peers (weight 1, packages of ≤ 30 files). The query
path reads ranks from `files.bin`; `graph.bin` is mapped lazily by verbs.

## delta/NNNN.bin (component 3)

```
u32 first_id, n_files, files_len, grams_len, symbols_len, spans_len, graph_len, words_len  (32 bytes)
files section, grams section, symbols section, spans section (layouts as above;
  file ids are first_id + position; symbol `file` fields are segment-local)
graph section:
  u32 n_files, u32 n_edges, u32 pad, u32 pad
  u32 prev[n_files]                    id of the file version this one supersedes (NONE = new file)
  u32 out_off[n_files+1]; u32 out_to[n_edges]; u16 out_w[n_edges]   imports, absolute target ids
words section (layout as above; postings hold absolute file ids)
roaring bitmap of tombstoned ids (files superseded or deleted by this delta)
```

Delta imports are resolved when the delta is written, against the live base
plus the delta's own files (which shadow the versions they replace), so
`ImpRec.target` and the graph section carry real ids; targets are the ids
current at that time and may themselves be superseded by a later delta. A
modified file keeps the `rank` of the record it supersedes (and delta symbols
are ordered by it); new files have rank 0 (unknown). PageRank is not
recomputed until the next build.

The query path never reads `graph.bin` or a delta graph section directly:
`Index::out_edges` and `Index::in_edges` fold them together. `prev` builds the
version chains (`canon` = oldest id of a path, `latest` = newest); an
outgoing edge is mapped to the newest live version of its target, and the
importers of a file are the live base importers of its oldest version (an
importer edited since carries its own edges in its delta) plus every live
delta file with an edge to any id on the chain.

A query evaluates `(base ∪ huge ∪ deltas) − tombstones − hidden`, each
segment answering from its word postings when the plan is a whole word and
from its grams otherwise; symbol lookups visit the base then each delta. Every section length is checked
against the payload and the tombstone bitmap is validated on open. A full
build starts a new generation; `delta/` and older generations are deleted
after the new manifest is written.

## Manifest fields

`format, root, generation, phase1, phase2, symbols, edges, parse_fallbacks,
phase2_ms, files, dirs, source_bytes, built_unix_ms, build_ms, fsevents_id,
verified_unix_ms, deltas, tombstones`. `fsevents_id` is the FSEvents event
id captured before the walk began and refreshed after every successful
check; `verified_unix_ms` drives the 100 ms TTL; a check that finds nothing
refreshes it at most once per second. `phase2 = false` means queries plan
with grams and classify with the regex extractor until the build finishes.
