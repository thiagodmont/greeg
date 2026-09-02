# greeg index on-disk format (version 2)

Location: `~/Library/Caches/greeg/<name>-<hash16>/` on macOS,
`$XDG_CACHE_HOME/greeg/...` elsewhere (override: `GREEG_INDEX_DIR`,
`--index-dir`). `<hash16>` is the first 16 hex chars of blake3 of the
canonical repository path.

```
manifest            JSON (see Manifest in greeg-index/src/lib.rs)
BUILDING            marker with the pid of a running build (create_new; stale after 10 min)
files.<gen>.bin     file table (republished by phase 2 with ranks and parse flags)
grams.<gen>.bin     trigram dictionary + postings
symbols.<gen>.bin   symbols, names, split tokens (phase 2)
spans.<gen>.bin     noncode spans and imports per file (phase 2)
graph.<gen>.bin     import graph CSR + PageRank (phase 2)
delta/NNNN.bin      delta segments, applied in name order
session/<id>.jsonl  session memory (DESIGN.md §9)
```

Every `.bin` starts with a 16-byte header: `"GREEG"`, format `u16` LE,
component `u8` (1 files, 2 grams, 3 delta, 5 symbols, 6 spans, 7 graph),
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
```

File ids are the position in `FileRec[]`, assigned in sorted-path order.
`flags` is the `greeg_lang::FileFlags` bitset (test, generated, vendored,
minified, binary, lockfile, huge, parse_errors, ...). `rank` is the
quantized PageRank: 0 unknown, else `1 + rank_norm × 65534`. Files flagged
binary or huge have no grams but stay in the table so the freshness pass
tracks them.

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
14 field, 15 variant. `flags`: 1 exported, 2 has_doc, 4 test. `[start, end)`
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
u32 first_id, n_files, files_len, grams_len, symbols_len, spans_len   (24 bytes)
files section, grams section, symbols section, spans section (layouts as above;
  file ids are first_id + position; symbol `file` fields are segment-local;
  imports in deltas are unresolved and ranks are 0)
roaring bitmap of tombstoned ids (files superseded or deleted by this delta)
```

A query evaluates `(base ∪ deltas) − tombstones`; symbol lookups visit the
base then each delta. A full build removes `delta/` and starts a new
generation; older generations are deleted after the manifest is written.

## Manifest fields

`format, root, generation, phase1, phase2, symbols, edges, parse_fallbacks,
phase2_ms, files, dirs, source_bytes, built_unix_ms, build_ms, fsevents_id,
verified_unix_ms, deltas, tombstones`. `fsevents_id` is the FSEvents event
id captured before the walk began and refreshed after every successful
check; `verified_unix_ms` drives the 100 ms TTL. `phase2 = false` means
queries plan with grams and classify with the regex extractor until the
build finishes.
