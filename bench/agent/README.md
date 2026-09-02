# Agent protocol (manual)

Measures whether a coding agent does better with greeg than with ripgrep on
real tasks. It is run by hand: it spends API credits and needs a Claude Code
login, so it is not part of CI. Tasks live in `tasks.toml`, ten across tokio,
django, ktor, TypeScript 5.9 and kotlinx.coroutines, each with a mechanical
check on the agent's final answer.

## Arms

| arm | search available to the agent | how |
|---|---|---|
| A | ripgrep via Bash only | `--disallowedTools Grep`, greeg not on `PATH` |
| B | greeg through the hook | `greeg hook claude` installed in the run's settings; plain `rg`/`grep` calls are rewritten |
| C | greeg through the hook plus the skill | as B, and the prompt starts with "Use `greeg` for code search" |

Same model, same `--max-turns`, same corpus checkout for every arm. Three
paired runs per (task, arm), so 90 runs for the full table.

## Running one cell

```sh
task=tokio-joinhandle-abort arm=B n=1
corpus=$(python3 -c "import tomllib;print([t for t in tomllib.load(open('bench/agent/tasks.toml','rb'))['task'] if t['id']=='$task'][0]['corpus'])")
prompt=$(python3 -c "import tomllib;print([t for t in tomllib.load(open('bench/agent/tasks.toml','rb'))['task'] if t['id']=='$task'][0]['prompt'])")
out=bench/agent/runs/$task/$arm/$n; mkdir -p $out
cd $GREEG_BENCH_CACHE/$corpus
case $arm in
  A) claude -p "$prompt" --output-format stream-json --verbose --disallowedTools Grep --max-turns 25 > $out/stream.jsonl ;;
  B) claude -p "$prompt" --output-format stream-json --verbose --max-turns 25 > $out/stream.jsonl ;;
  C) claude -p "Use greeg for code search. $prompt" --output-format stream-json --verbose --max-turns 25 > $out/stream.jsonl ;;
esac
python3 -c "import json,sys; [print(j['result']) for j in map(json.loads, open('$out/stream.jsonl')) if j.get('type')=='result']" > $out/answer.txt
check=$(python3 -c "import tomllib;print([t for t in tomllib.load(open('$OLDPWD/bench/agent/tasks.toml','rb'))['task'] if t['id']=='$task'][0]['check'])")
(cd $out && sh -c "$check"; echo $? > check.txt)
```

For arm A, run with a `PATH` that does not contain greeg and a settings file
without the hook (`claude --settings /path/to/settings-A.json`). For arms B and
C, install the hook into that settings file with
`greeg hook claude` (it edits `~/.claude/settings.json`; point `HOME` at a
scratch directory to keep the run isolated).

## Scoring

```sh
python3 bench/agent/extract.py bench/agent/runs > bench/agent/runs.json
python3 bench/agent/analyze.py bench/agent/runs.json
```

`extract.py` reads each stream: tool calls by name, search calls split into
`rg`/`grep`/`greeg` (Bash commands are split on pipes and `&&`; the built-in
Grep tool counts as `rg`), input and output tokens from the assistant usage
records, turns, wall time and cost from the final `result` record, and the
check outcome. `analyze.py` pairs runs by (task, run number) and prints the
mean paired difference of each metric for arms B and C against A with a 95 %
bootstrap interval (10,000 resamples).

What decides: arm B vs A on success rate and search calls answers "does the
model use it and does it help"; arm C vs B answers whether the model needs to
be told. If B does not beat A, the fix is in greeg's output or the hook's
rewrite rules, not in the prompt.
