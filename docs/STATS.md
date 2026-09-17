# Measuring greeg on your own traffic

`greeg stats` records what your agent actually searched for, then replays those
searches against `rg`/`grep`. So "is this saving me anything?" gets answered
from your repositories, on your machine.

It's **off by default**, because the records hold your search patterns and
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

Once it's on, the hook records every `rg`/`grep` call it rewrote, and every
greeg run records its wall time and output size. `greeg stats replay` runs the
original `rg`/`grep` commands and their greeg rewrites side by side, same
machine, same tree. Then `greeg stats` leads with the savings:

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

Read the two halves together. The totals are what your agent actually paid, and
the typical query says how that total is distributed.

Coding agents mostly grep one file at a time. On those, greeg lands within a
few dozen tokens of grep (the path header, the kind column and the containers,
none of which grep prints) and runs a couple of milliseconds slower. The
savings come from the repository-wide searches, where grep dumps every match
and greeg budgets them. A handful of queries hold most of the total.

The report also counts the things that aren't savings. Queries where greeg
found nothing and grep did (that's a retry for your agent; `--verbose` marks
them with `!`), rewritten queries with no replay yet, and hook rewrites that no
greeg run followed.

That last one happens when another PreToolUse hook rewrites the same Bash call.
Hooks run in parallel and the last `updatedInput` to finish wins, so keep one
rewriter for `rg`/`grep`. RTK users: `exclude_commands = ["grep", "rg"]` under
`[hooks]` in its config.

Below the savings come the latency and token distributions (avg, min, p50, p95,
p99, max, total) for rewritten searches, direct searches, symbol verbs, both
replayed sides, and the per-query saving. Direct searches and verbs have no rg
counterfactual to compare against. `--verbose` lists every replayed query,
largest saving first, and the runs per directory.

## Per session

`greeg stats sessions` splits the same numbers by agent session, newest first.

The hook records the Claude Code session id of every rewrite, and a greeg run
reads `CLAUDE_CODE_SESSION_ID` (or `GREEG_SESSION`) from its environment, so
direct searches and symbol verbs land in their session too. `--session-id ID`
narrows the full report to one session; a prefix will do, or `current` from
inside an agent session.

```
sessions · newest first · saved = replayed rewrites vs rg/grep capped at 30,000 bytes · lost = hook rewrites no greeg run followed
   started (UTC)  session   rewritten replayed saved tokens saved time  lost direct verbs  directory
2026-09-06 15:59  136e3c48          8        7        4,370     -19 ms     9      0     0  ~/Documents/projects/greeg
2026-09-04 22:02  41fcc201         13       13          216     -15 ms     5      0     0  ~/Documents/projects/uisper/buddy
2026-09-04 20:23  3c540ed2         38       38       10,116     3.95 s     2      0     0  ~/Documents/projects/agentpipe
2026-09-04 19:59  aba03685         13       13         -388     -17 ms     8      0     0  ~/Documents/projects/claybound
```

A session that only grepped single files comes out slightly negative. The ones
that searched a whole repository carry the total.

## Comparing two builds

Every record names the greeg build that made it, exactly as `greeg --version`
prints it: the release, or `0.4.0+ff9011a` for a build from that commit, with
`.dirty` appended if the tree had uncommitted changes.

A query keeps one replay per build. `greeg stats replay --binary PATH` replays
with another build (it gets an index directory of its own, since index formats
change between versions), and `greeg stats compare 0.3.0 0.4.0` puts the two
side by side on the queries replayed under both: tokens and time, the per-query
wins and losses, and what each saved against rg. Replay both builds back to
back if you want the time comparison to mean anything.

`--greeg VERSION` narrows any stats command to one build's records, and
`greeg stats status` counts records per build.

## Privacy and storage

The records hold search patterns and paths, which is why this is opt-in. They
live under the user cache dir (`~/Library/Caches/greeg/stats` on macOS,
`~/.cache/greeg/stats` elsewhere), mode 0600, never leave the machine, rotate
at 16 MB, and `greeg stats clear` deletes them.

Output itself is never stored. Only its size and token estimate.

Claude Code truncates Bash output at 30,000 characters, so rg tokens get
reported raw and capped at that size, and the savings use the capped figure.
`--cap N`, `greeg stats enable --cap N` or `stats_cap = N` in
`~/.config/greeg/config.toml` change it. `GREEG_STATS=0` overrides the config
file, and the benchmark scripts set it so their own runs stay out of your
report.

