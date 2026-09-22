# Exact matching and discovery

W01a separates query matching from output shaping. This is the first PR of
the reliability plan; hook eligibility and shell translation are W01b.

`--matching exact` searches only under the requested flags. It honors `-i`,
`-S`, `-w`, regexes and fixed strings; “exact” does not mean case-sensitive or
literal. `--matching discover` first runs the same search, then tries the
existing relaxation ladder only if no hits were found. Neither policy changes
the token budget, file selection, classification or freshness policy.

## File-search defaults

| Invocation | Default policy | stdout on an exact miss | Diagnostics / discovery provenance |
|---|---|---|---|
| Ranked text with positive budget | Discover | Suggestions if found; otherwise no-hit footer | Text footer on stdout names the rung |
| `-l` / `--mode files` | Exact | Empty | Footer on stderr |
| `-c` / `--mode count` | Exact | Empty | Footer on stderr |
| `--budget 0` | Exact | Empty | Footer on stderr |
| `--json` (any budget) | Exact | Summary/footer records, no match records | Existing JSON footer includes `rung` |
| Any file-search mode with `--matching exact` or `--no-ladder` | Exact | As above for its format | No claim that the ladder ran |
| Any file-search mode with `--matching discover` | Discover | Relaxed results may be emitted | Rung in that format's footer |
| Piped/file stdin without positional paths | Exact | Empty text, or JSON summary/footer | Explicit `--matching discover` is rejected with exit 2 |

The mode aliases have the same defaults as `-l` and `-c`. Exact takes priority
when several default conditions apply; an explicit policy overrides them.
`--no-ladder` conflicts with `--matching` to avoid ambiguous precedence.
An unknown policy is a CLI error. Defaults are independent of TTY detection.

File searches exit 0 only for an exact match under the supplied flags, 1 for
no exact matches (even when discovery produces results), and 2 for errors.
Output write failures, including broken pipes, keep the existing error path
and exit 2. Exactness does not promise complete enumeration under a positive
token budget or fix the known selection/classification gaps planned for W03.
The existing JSON dialect is unchanged; this is not a new ripgrep JSON parity
claim. Symbol-verb output/status consistency remains part of W03/W07; their
existing optional name suggestions use the same selected policy.

## Migration

Previously the CLI enabled discovery even for file lists, counts, unlimited
output and JSON. Those calls can now correctly produce no matches where they
previously emitted relaxed results with exit 1. Callers intentionally consuming
suggestions can restore that behavior with `--matching discover`. For scripts
requiring strict matching, explicitly use `--matching exact` (or the existing
`--no-ladder`). Ranked text retains its existing default discovery behavior.
Library callers use `Options::matching`; `Options::default()` is exact.

The generated agent skill describes these defaults after a future hook
installation. This PR does not reinstall hooks or edit user configuration.
It also does not certify the current hook's shell parser or grep/rg rewrite
subset; those require the separate W01b capability matrix and refusal tests.

## Measurement and gates

`crates/greeg/tests/cli.rs` exercises actual scan and full-index processes for
case, word-boundary, split-name and fuzzy misses; explicit policies; the legacy
flag; requested case/word semantics; and unchanged ranked discovery. No network
or external corpus is required for these regression tests.

`bench/matching.py BASELINE CANDIDATE --output result.json` creates a disposable
256-file corpus, builds separate indexes, asserts indexed execution, warms each
case, and measures paired runs in randomized order. It records binary digests,
versions, corpus digest, raw timing samples, p50/p95 latency, both output stream
sizes, and stdout/status parity with installed ripgrep for machine-text cases.
Use release binaries built with the same locked dependencies and compiler.
Optionally pass `--tokens` to count both streams using installed `tiktoken`'s
`o200k_base` dictionary (cache it before the timed run; no model/API requests).

Token counts describe this output, not downstream agent task savings. Warm
local process timings are evidence, not a portable performance guarantee.
Investigate a >10% median or >20% p95 regression, including absolute differences
and machine noise, before drawing a conclusion. Cold cache, peak RSS,
concurrent queries and agent tasks require the broader W08 evaluation work.
