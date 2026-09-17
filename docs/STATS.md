# Measuring greeg on your own traffic

`greeg stats` records what your agent actually searched for and replays those
searches against `rg`/`grep`, so the question "is this saving me anything?" has
an answer from your repositories rather than from a benchmark suite.

It is **off by default**, because the records hold your search patterns and
paths. They live under the user cache directory (`~/Library/Caches/greeg/stats`
on macOS, `~/.cache/greeg/stats` elsewhere), mode 0600, never leave the machine,
rotate at 16 MB, and `greeg stats clear` deletes them.

```bash
greeg stats enable              # or GREEG_STATS=1 for one shell
# ... work with your agent for a while ...
greeg stats replay --runs 3     # on a quiet machine
greeg stats --since 7d --repo . # --json for machine output, --verbose for every query
```

## What it records

Enabling it makes the hook record every `rg`/`grep` call it rewrote, and every
greeg run record its wall time and output size. `greeg stats replay` then runs
the original `rg`/`grep` commands and their greeg rewrites side by side, on the
same machine and the same tree, and `greeg stats` leads with the savings:

```
savings vs rg/grep · 145 replayed queries · same machine and tree · rg output capped at 30,000 bytes
                              rg/grep        greeg        saved
  tokens, total                86,258       66,310       19,948  23%
  tokens, typical query           311          318          -30  greeg smaller on 37 queries, larger on 108
  time, total                  4.64 s       866 ms       3.77 s  81%
  time, typical query          3.4 ms       6.1 ms      -2.3 ms  greeg faster on 53 queries, slower on 92
  typical = median; on those rows `saved` is the median of the per-query differences
  the 3 largest token wins hold 19,754 tokens; the other 142 queries net 194
  the 3 largest time wins hold 3.25 s; the other 142 queries net 517 ms
```

Read the two halves together. The totals are what the agent actually paid;
the typical query says how that total is distributed. Coding agents mostly
grep one file at a time, and on those greeg is within a few dozen tokens of
grep (the path header, the kind column and the containers, which grep does
not print) and a couple of milliseconds slower. The savings come from the
repository-wide searches, where grep dumps every match and greeg budgets
them, so a handful of queries hold most of the total. The report also
counts what a saving is not: queries where greeg found nothing but grep did
(a retry for the agent; `--verbose` marks them with `!`), rewritten queries
with no replay yet, and hook rewrites that no greeg run followed. That last
one happens when another PreToolUse hook rewrites the same Bash call: hooks
run in parallel and the last `updatedInput` to finish wins, so keep one
rewriter for `rg`/`grep` (RTK users: `exclude_commands = ["grep", "rg"]`
under `[hooks]` in its config).

Below the savings come the latency and token distributions (avg, min, p50,
p95, p99, max, total) for rewritten searches, direct searches, symbol verbs,
both replayed sides, and the per-query saving. Direct searches and verbs
have no rg counterfactual. `--verbose` lists every replayed query, largest
saving first, and the runs per directory.

## Per session

`greeg stats sessions` splits the same numbers by agent session, newest
first. The hook records the Claude Code session id of every rewrite, and a
greeg run records `CLAUDE_CODE_SESSION_ID` (or `GREEG_SESSION`) from its
environment, so direct searches and symbol verbs land in their session too.
`--session-id ID` (a prefix will do, or `current` inside an agent session)
narrows the full report to one session:

```
sessions · newest first · saved = replayed rewrites vs rg/grep capped at 30,000 bytes · lost = hook rewrites no greeg run followed
   started (UTC)  session   rewritten replayed saved tokens saved time  lost direct verbs  directory
2026-09-06 15:59  136e3c48          8        7        4,370     -19 ms     9      0     0  ~/Documents/projects/greeg
2026-09-04 22:02  41fcc201         13       13          216     -15 ms     5      0     0  ~/Documents/projects/uisper/buddy
2026-09-04 20:23  3c540ed2         38       38       10,116     3.95 s     2      0     0  ~/Documents/projects/agentpipe
2026-09-04 19:59  aba03685         13       13         -388     -17 ms     8      0     0  ~/Documents/projects/claybound
```

A session that only grepped single files comes out slightly negative; the
ones that searched a whole repository carry the total.

## Comparing two builds

Every record names the greeg build that made it, as `greeg --version` prints
it: the release, or `0.4.0+ff9011a` for a build from that commit and
`.dirty` with uncommitted changes. A query keeps one replay per build, so
`greeg stats replay --binary PATH` can replay with another build (it gets an
index directory of its own, since index formats change between versions) and
`greeg stats compare 0.3.0 0.4.0` then puts the two side by side on the
queries replayed under both: tokens and time, the per-query wins and losses,
and what each saved against rg. Replay both builds back to back for a fair
time comparison. `--greeg VERSION` narrows any stats command to one build's
records, and `greeg stats status` counts records per build.

## Privacy and storage

The records hold search patterns and paths, which is why this is opt-in.
They live under the user cache dir (`~/Library/Caches/greeg/stats` on
macOS, `~/.cache/greeg/stats` elsewhere), mode 0600, never leave the machine,
rotate at 16 MB, and `greeg stats clear` deletes them. Output is never
stored, only its size and token estimate. Claude Code truncates Bash output
at 30 000 characters, so rg tokens are reported raw and capped at that size;
the savings use the capped figure. `--cap N`, `greeg stats enable --cap N` or
`stats_cap = N` in `~/.config/greeg/config.toml` change it. `GREEG_STATS=0`
overrides the config file; the benchmark scripts set it so their runs stay
out of the report.

