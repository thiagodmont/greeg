# Agent protocol

Measures whether a coding agent does better with greeg than with ripgrep on
real tasks. It is run by hand: it spends API credits and needs a Claude Code
login, so it is not part of CI. Tasks live in `tasks.toml`, twenty-eight
across tokio, django, ktor, TypeScript 5.9 and kotlinx.coroutines: ten single
lookups, ten that take several hops (five held out), and eight that pick which
of many same-named definitions a call site reaches or map the files around one
anchor (four held out). Each has a mechanical check on the agent's final
answer: every item the prompt asks for, with its file:line. An ambiguous-name
task's `incomplete` answers include one that follows the wrong definition. A check must pass the task's verified `answer`, and fail it without
its line numbers and each `incomplete` answer.

## Arms

| arm | search available to the agent | how |
|---|---|---|
| A | ripgrep and grep via Bash, no greeg | greeg not on `PATH`, no hook |
| B | greeg through the hook | `greeg hook run` as the run's PreToolUse hook; plain `rg` calls are rewritten, `grep` is left alone |
| C | greeg through the hook, named in the prompt | as B, and the prompt starts with "Use greeg for code search." |

Same model, task prompt (C adds its prefix), turn limit, tools (`Bash`,
`Read`) and allowed commands for every arm.

## Running

```sh
python3 bench/bench.py fetch tokio django      # the tasks' corpora, pinned
python3 bench/agent/run.py --greeg target/release/greeg --out RUN_DIR \
    --tasks tokio-joinhandle-abort,django-csrf-token --arms A,B,C --repeats 2
```

`--arms` defaults to `A,B`. `run.py` runs each (task, arm, repeat) cell headless (`claude -p`) in a
shuffled, seeded order, and writes `RUN_DIR/runs/<task>/<arm>/<n>/` with the
stream, the answer, the check result and `meta.json`. A cell already written
is skipped, so an interrupted run resumes, or grows by tasks and arms. A
resume with another model, limit, tool version, allowed or denied command, or
corpus revision is refused. Each run is isolated:

- **settings:** none of the user's, the project's or local ones (no hooks,
  CLAUDE.md, skills or MCP servers); only the arm's own settings file;
- **PATH:** a directory with `rg` (and `greeg` in B and C), then the system
  directories; nothing else the user installed;
- **workspace:** a copy-on-write clone of the corpus, whose
  `git status --ignored` must not change during a run; a run that changed it is
  left out, and the clone is made again (so is a clone an interrupted run left
  changed);
- **greeg:** a fresh copy of the corpus's prebuilt index per run (no session
  memory carries over), statistics off, budget pinned to 2000, and no
  `RIPGREP_*` variables (a ripgrep config file makes the hook decline);
- **permissions:** `dontAsk`, with search and viewing commands allowed and
  their common write forms denied (`sed -i`, `sort -o`). A rewritten command
  is checked as rewritten, so `greeg` is on the list. This is not a sandbox
  (`find -exec` is allowed), so the index template is checked after every
  run too, and a change stops the run; `extract.py` leaves out a run whose
  commands could write files.

`RUN_DIR/setup.json` records the Claude Code and greeg versions, the model,
the corpora's commits and the allowed and denied commands.

A check that turns out to reject a right answer can be fixed in `tasks.toml`.
`run.py --out RUN_DIR --recheck` then scores every saved answer again with the
current checks, and records each changed verdict in `meta.json`
(`first_check`).

## Scoring

```sh
python3 bench/agent/extract.py RUN_DIR/runs > RUN_DIR/runs.json
python3 bench/agent/analyze.py RUN_DIR/runs.json
```

`extract.py` reads each stream: tool calls by name, search calls split into
`rg`/`grep`/`greeg` (Bash commands are split on pipes and `&&`; the built-in
Grep tool counts as `rg`), input and output tokens from the assistant usage
records, turns, wall time and cost from the final `result` record, and the
check outcome. The executed command comes from the hook responses in the
stream. Calls the permission rules denied are counted apart. A run is marked
invalid, with its reasons, when it has no `meta.json` (only `run.py` runs are
compared), the workspace changed, the stream has no result, arm A ran greeg, a
search in B or C ran without a hook response, a command could write files (a
redirect to a file, `find -exec` or `-delete`, `sed -i`, `sort -o`, `rm`, …),
or a hook response could belong to more than one call.

`analyze.py` lists the invalid runs and leaves them out, along with the runs
of the other arms they pair with, so every column averages the same pairs. It
then prints each task's successes, cost and turns per arm, each arm's success
rate with a 95 % Wilson interval, and the mean paired difference of each metric
for arms B and C against A with two 95 % bootstrap intervals (10,000
resamples): over pairs, and over tasks. Pairs are matched on (task, run
number). The task interval draws whole tasks, so the repeats of one task do not
count as independent evidence; with few tasks it is rough too. Read a
difference as shown only when both intervals exclude zero.

Tasks with `held_out = true` are reported in their own section. greeg's
output and the hook's rules are not tuned on them, so a decision rests on
them; the development tasks are the ones to look at when changing greeg.

What decides: arm B vs A on success rate and search calls answers "does the
model use it and does it help"; arm C vs B answers whether the model needs to
be told. If B does not beat A, the fix is in greeg's output or the hook's
rewrite rules, not in the prompt.

## Pilot (2026-10-03)

Six tasks (one per corpus, two from tokio) × arms A, B and C × 2 repeats, with
Claude Code 2.1.288 on Sonnet (`claude-sonnet-5-5`) and greeg at 22de3c6.
`results/pilot-2026-10-03.json` holds the extracted records, `.txt` the
analysis, and `-setup.json` the setup. 36 runs cost $1.13 in all. The pilot
predates the denied commands: its setup has no deny list, and `awk` and
`xargs` were still allowed.

- **Every run passed its check**, in every arm. The tasks are too easy to
  separate the arms: 1 to 7 tool calls each. The checks have since been made
  to require file:line and every item the prompt asks for; these verdicts are
  from the checks at the time.
- **Arm A never ran ripgrep.** All 39 of its searches were `grep`, which is on
  `PATH` and allowed in every arm, so this baseline measured grep, not rg.
- **Arm B never ran greeg.** The model searched with `grep` only (no Grep tool
  in this Claude Code), mostly in pipelines (`| head`), command lists (`;`),
  `cd … &&` chains or with `--include=*.kt`. The hook declined all 36 calls,
  as its rules say, so B was in effect a second run of A. Even so, B's tool
  output came out about 1,000 characters larger, with an interval that excludes
  zero. With 12 pairs and ten metrics, such an interval is weak evidence.
- **Arm C ran greeg** for 79 of 81 searches, mostly `greeg def` (53). It used
  more Bash calls (42 against 30) and more cache reads (+14k tokens a run):
  it spends its first call on `greeg --help`, and chains several `greeg def`
  calls in one command. Cost and success did not change.

Next: harder tasks that need several hops, and a decision on whether the hook
should rewrite the commands this model writes (a search piped into `head`, a
list of searches).

## Multi-hop run (2026-10-16)

The ten multi-hop tasks × arms A and B × 2 repeats, with Claude Code 2.1.301 on
Sonnet and greeg at a206030 (the hook stack up to `| head` and stderr
redirections). `results/multihop-2026-10-16.json` holds the records, `.txt`
the analysis, `-setup.json` the setup. 40 runs, all valid, cost $1.99.

The checks were rebuilt after the run, and every saved answer was scored again
(`--recheck`). The first checks rejected right answers that cite `base.py:275`
after naming the directory once. Each cited item now has to appear by name and
with a line within two of the verified one, under its file name (with the
parent where two cited files share a name). The answers that still fail each
miss a requested item: a definition given as a call site, a path step left
out, a line not cited.

- **Success:** development tasks 10/10 in both arms. Held out: A 7/10
  (Wilson 40–89 %), B 8/10 (49–94 %), difference +0.10, interval −0.40…+0.60
  over tasks. The arms are not separated; two tasks per corpus and two repeats
  are too few, and the development tasks are still too easy for this model.
- **The hook now takes part.** In B the hook answered all 74 search calls and
  rewrote 9 to greeg. In the pilot it rewrote none. Most of the rest are greps
  of named files (25 of 54 commands) and pipelines into other readers (12),
  which stay unchanged by design.
- **Search calls, held out:** B made 3.0 fewer a run (6.6 → 3.6), interval
  −8.2…+0.2 over tasks: a hint, not a result. Cost, turns and tool output show
  no difference.

Next: more held-out tasks (and repeats) before any decision, and arm C to
see whether naming greeg in the prompt changes the share of searches it runs.

## Ambiguous-definition run (2026-10-16)

The eight ambiguous-definition and focused-map tasks × arms A and B × 2
repeats, with Claude Code 2.1.302 on Sonnet and greeg at the 0.12.0 release
commit. `results/ambiguous-2026-10-16.json` holds the records, `.txt` the
analysis, `-setup.json` the setup. 32 runs, all valid, cost $1.58. It asks
whether agents pick the wrong one of many same-named definitions today, which
an ambiguity-aware `def` would fix.

The checks were loosened after the run, and every saved answer was scored
again (`--recheck`). The first checks rejected right answers that list files
in a table (`` `actions.py` | 8 ``), cite `main.py:464` after naming
`views/main.py`, or give bare lines after "all lines are in parser.ts". A
citation may now separate the file and its line by a table cell or "line",
and the single-file TypeScript task takes lines once the file is named.
Eight verdicts changed, all from fail to pass: the importers map 4 (two per
arm), `get_queryset` 3 (A 2, B 1) and the TypeScript task 1 (B). Before the
change, held out was A 8/8 and B 7/8, development A 3/8 and B 5/8. Every path
the answers cite (176) exists in the pinned corpus. The one answer that still
fails names `safeDispatch` without its file:line (`DispatchedContinuation.kt`
:250–254), so the ordered path check fails too.

- **Success:** held out 8/8 in both arms (Wilson 68–100 %). Development
  tasks: A 7/8, B 8/8. Every passing answer names the definition that runs
  and each step after it; the failing one names the right definition and
  leaves out one step's file:line.
  The agents worked it out from the receiver's type, the class hierarchy or
  the method resolution order with `grep` and `Read`, in four to eleven turns.
- **The hook:** it answered all 71 of B's search calls and rewrote 10 to
  greeg; the other 61 are greps of named files and pipelines.
- **Held out, search calls:** B made 2.0 fewer a run (3.9 → 1.9), interval
  −6.4…+0.4 over tasks. Cost, turns and output show no difference.

No wrong-definition failure was observed: none in the 24 ambiguous-name runs
(a 95 % upper bound of about 14 %). Held out is at the ceiling in both arms,
so these tasks cannot show a gain from an ambiguity-aware `def`; they show no
headroom for one with this model.
