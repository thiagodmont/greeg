# Agent protocol

Measures whether a coding agent does better with greeg than with ripgrep on
real tasks. It is run by hand: it spends API credits and needs a Claude Code
login, so it is not part of CI. Tasks live in `tasks.toml`, ten across tokio,
django, ktor, TypeScript 5.9 and kotlinx.coroutines, each with a mechanical
check on the agent's final answer.

## Arms

| arm | search available to the agent | how |
|---|---|---|
| A | ripgrep via Bash only | greeg not on `PATH`, no hook |
| B | greeg through the hook | `greeg hook run` as the run's PreToolUse hook; plain `rg`/`grep` calls are rewritten |
| C | greeg through the hook, named in the prompt | as B, and the prompt starts with "Use greeg for code search." |

Same model, prompt, turn limit, tools (`Bash`, `Read`) and allowed commands
for every arm.

## Running

```sh
python3 bench/bench.py fetch tokio django      # the tasks' corpora, pinned
python3 bench/agent/run.py --greeg target/release/greeg --out RUN_DIR \
    --tasks tokio-joinhandle-abort,django-csrf-token --repeats 2
```

`run.py` runs each (task, arm, repeat) cell headless (`claude -p`) in a
shuffled, seeded order, and writes `RUN_DIR/runs/<task>/<arm>/<n>/` with the
stream, the answer, the check result and `meta.json`. A cell already written
is skipped, so an interrupted run resumes. Each run is isolated:

- **settings:** none of the user's, the project's or local ones (no hooks,
  CLAUDE.md, skills or MCP servers); only the arm's own settings file;
- **PATH:** a directory with `rg` (and `greeg` in B and C), then the system
  directories; nothing else the user installed;
- **workspace:** a copy-on-write clone of the corpus, checked with
  `git status` after every run; a run that changed it is restored and left out;
- **greeg:** a fresh copy of the corpus's prebuilt index per run (no session
  memory carries over), statistics off, budget pinned to 2000;
- **permissions:** `dontAsk`, with read-only search commands allowed. A
  rewritten command is checked as rewritten, so `greeg` is on the list.

`RUN_DIR/setup.json` records the Claude Code and greeg versions, the model,
the corpora's commits and the allowed commands.

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
invalid, with its reasons, when the workspace changed, the stream has no
result, arm A ran greeg, or a search in B or C ran without a hook response.

`analyze.py` lists the invalid runs and leaves them out, along with their
pairs. It then prints each task's successes, cost and turns per arm, and the
mean paired difference of each metric for arms B and C against A with a 95 %
bootstrap interval (10,000 resamples). Pairs are matched on (task, run number).

What decides: arm B vs A on success rate and search calls answers "does the
model use it and does it help"; arm C vs B answers whether the model needs to
be told. If B does not beat A, the fix is in greeg's output or the hook's
rewrite rules, not in the prompt.
