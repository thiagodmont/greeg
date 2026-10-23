#!/usr/bin/env python3
"""Run paired agent tasks headless: rg and grep only (arm A) against greeg through
its hook (arm B) and, if asked, the hook plus a prompt that names greeg (arm C)
or the hook plus greeg's own skill in the system prompt (arm S).

    bench/agent/run.py --greeg BIN --out DIR [--tasks ID,…] [--repeats 2]
        [--model sonnet] [--max-turns 25] [--max-budget-usd 2] [--seed 7]
    bench/agent/run.py --out DIR --recheck    # score saved answers with the current checks
    bench/agent/extract.py DIR/runs > runs.json
    bench/agent/analyze.py runs.json

Every run gets the same prompt, model, turn limit, tools and allowed commands.
The arms differ only in greeg: arms B and C have it on PATH, a prebuilt index
and `greeg hook run` as their PreToolUse hook. Isolation:

- no user, project or local settings, hooks, CLAUDE.md or MCP servers
  (`--setting-sources ""`, `--strict-mcp-config`); the arm's own settings file;
- PATH holds a directory with `rg` (and `greeg` in arms B and C), then /usr/bin
  and /bin;
- each corpus is cloned once into DIR/work, and its `git status` must not change
  during a run (a changed clone is made again and the run is marked);
- each run copies the corpus's index into its own GREEG_INDEX_DIR, so no session
  memory carries over; statistics are off and the budget is pinned;
- cells run in a shuffled order, seeded, so drift over time hits both arms.

Each run directory holds `stream.jsonl`, `answer.txt`, `check.txt` and
`meta.json`. It needs a Claude Code login and spends API credits.
"""
import argparse, contextlib, hashlib, json, os, random, shutil, signal, subprocess, sys, time, tomllib
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
with open(HERE.parent / "corpora.toml", "rb") as fh:
    CORPORA = tomllib.load(fh)
# ignored files count too: an agent's write to one would otherwise go unseen
STATUS = ["git", "status", "--porcelain", "--ignored"]


def status(work):
    """The clone's `git status`, or None when git cannot read it."""
    p = subprocess.run(STATUS, cwd=work, capture_output=True, text=True)
    return p.stdout if p.returncode == 0 else None


def corpus_path(name):
    cache = os.environ.get("GREEG_BENCH_CACHE") or os.path.expanduser("~/.cache/greeg-bench/corpora")
    return Path(cache) / CORPORA[name].get("dir", name)

ALLOWED = ["Read"] + [f"Bash({c}:*)" for c in (
    "rg", "greeg", "grep", "find", "ls", "cat", "head", "tail", "sed", "wc",
    "sort", "uniq", "cut", "tr", "git status", "git diff", "git show", "git log",
    "git ls-files", "git blame", "git grep", "git rev-parse", "git merge-base")]
# the allowed commands' common ways to write; the clone and the index
# template are also checked after every run
DENIED = [f"Bash({c}:*)" for c in ("sed -i", "sed --in-place", "sort -o", "sort --output",
                                     "git diff --output", "git show --output", "git log --output")]
HOOK = {"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "greeg hook run"}]}]}}


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw).stdout


def clone(src, dst):
    """A copy-on-write clone where the file system has one, else a copy."""
    if subprocess.run(["cp", "-cR", str(src), str(dst)], capture_output=True).returncode != 0:
        shutil.rmtree(dst, ignore_errors=True)
        shutil.copytree(src, dst, symlinks=True)


def tool_bin(out, arm, greeg):
    d = out / "bin" / arm
    d.mkdir(parents=True, exist_ok=True)
    tools = {"rg": shutil.which("rg")}
    if arm != "A":
        tools["greeg"] = greeg
    for name, path in tools.items():
        if not path:
            sys.exit(f"{name} not found")
        link = d / name
        if not link.exists():
            link.symlink_to(path)
    return d


def greeg_env(index_dir, stats_dir):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("GREEG_", "CLAUDE_CODE_", "CLAUDECODE", "RIPGREP_"))}
    env.update(GREEG_INDEX_DIR=str(index_dir), GREEG_STATS="0", GREEG_STATS_DIR=str(stats_dir),
               GREEG_BUDGET="2000", GREEG_CONFIG_DIR="/dev/null/greeg-config")
    return env


def patches(task):
    """A diff task's patches, staged first: the checkout it asks about."""
    d = HERE / "diff" / task["id"]
    return [(d / f, f == "staged.patch") for f in ("staged.patch", "unstaged.patch") if (d / f).exists()]


def exclude_untracked(work):
    """Exclude the files the corpus setup left untracked, so a task's patch
    is the only change `git status` shows. Before indexing: the exclude file
    is one of greeg's ignore inputs."""
    untracked = [l[3:] for l in sh(["git", "status", "--porcelain"], cwd=work).splitlines() if l.startswith("?? ")]
    with open(work / ".git" / "info" / "exclude", "a") as fh:
        fh.writelines(f"/{f}\n" for f in untracked)


def apply_patches(work, task):
    for patch, staged in patches(task):
        sh(["git", "apply", *(["--index"] if staged else []), str(patch)], cwd=work)


def build_template(greeg, work, template, out):
    shutil.rmtree(template, ignore_errors=True)
    env = greeg_env(template, out / "stats")
    subprocess.run([greeg, "index"], cwd=work, env=env, check=True, capture_output=True)


def prepare(out, task, greeg):
    """The task's clone, its `git status` when made, and its prebuilt
    index. Tasks share their corpus's clone, except a diff task, whose clone
    carries its patch, applied after the index was built (the edits an agent
    is asked about are newer than the index). A clone an interrupted run left
    changed is made again, and so is its index: an index is tied to the
    files it was built from."""
    corpus = task["corpus"]
    diff = bool(patches(task))
    key = task["id"] if diff else corpus
    work = out / "work" / key
    baseline = out / "work" / f"{key}.status"
    template = out / "index" / key
    if work.exists() and (not baseline.exists() or status(work) != baseline.read_text()):
        shutil.rmtree(work)
    if not work.exists() or not template.exists():
        shutil.rmtree(work, ignore_errors=True)
        work.parent.mkdir(parents=True, exist_ok=True)
        clone(corpus_path(corpus), work)
        if diff:
            exclude_untracked(work)
        build_template(greeg, work, template, out)
        apply_patches(work, task)
        baseline.write_text(sh(STATUS, cwd=work))
    return work, baseline.read_text(), template


def skill_text(out, greeg):
    """The skill `greeg hook claude` installs, written into a scratch home once
    per run directory, so every arm-S run gets the same text."""
    saved = out / "skill.md"
    if not saved.exists():
        home = out / "skill-home"
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CLAUDE", "CODEX", "GREEG_"))}
        env.update(HOME=str(home), GREEG_CONFIG_DIR=str(home / "greeg-config"))
        subprocess.run([greeg, "hook", "claude"], env=env, check=True, capture_output=True,
                       stdin=subprocess.DEVNULL)
        saved.write_text((home / ".claude" / "skills" / "greeg" / "SKILL.md").read_text())
    return saved.read_text()


def tree_state(root):
    """Paths, sizes and modification times under `root`."""
    return sorted((str(p.relative_to(root)), p.stat().st_size, p.stat().st_mtime_ns)
                  for p in root.rglob("*") if p.is_file())


def run_cell(a, out, task, arm, n, order):
    d = out / "runs" / task["id"] / arm / str(n)
    if (d / "meta.json").exists():
        return json.loads((d / "meta.json").read_text())
    d.mkdir(parents=True, exist_ok=True)
    work, before, template = prepare(out, task, a.greeg)
    template_before = tree_state(template)
    index = d / "index"
    shutil.rmtree(index, ignore_errors=True)
    clone(template, index)
    env = greeg_env(index, d / "stats")
    env["PATH"] = f"{tool_bin(out, arm, a.greeg)}:/usr/bin:/bin:/usr/sbin:/sbin"
    settings = d / "settings.json"
    settings.write_text(json.dumps({} if arm == "A" else HOOK))
    prompt = ("Use greeg for code search. " if arm == "C" else "") + task["prompt"]
    skill = ["--append-system-prompt", skill_text(out, a.greeg)] if arm == "S" else []
    cmd = [a.claude, "-p", prompt, *skill, "--model", a.model, "--max-turns", str(a.max_turns),
           "--max-budget-usd", str(a.max_budget_usd), "--output-format", "stream-json", "--verbose",
           "--include-hook-events", "--no-session-persistence", "--setting-sources", "",
           "--strict-mcp-config", "--settings", str(settings), "--tools", "Bash,Read",
           "--permission-mode", "dontAsk", "--allowedTools", *ALLOWED,
           "--disallowedTools", *DENIED]
    started = time.time()
    with open(d / "stream.jsonl", "w") as fh:
        # its own process group, so a timeout also stops the commands it started
        p = subprocess.Popen(cmd, cwd=work, env=env, stdin=subprocess.DEVNULL, stdout=fh,
                             stderr=subprocess.PIPE, text=True, start_new_session=True)
        try:
            _, stderr = p.communicate(timeout=a.timeout)
            exit_code = p.returncode
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            _, stderr = p.communicate()
            exit_code = "timeout"
        except BaseException:
            # an interrupted runner leaves no agent writing to the clone it resumes with
            with contextlib.suppress(ProcessLookupError):
                os.killpg(p.pid, signal.SIGKILL)
            raise
    wall = time.time() - started
    answer = ""
    for line in open(d / "stream.jsonl"):
        try:
            j = json.loads(line)
        except ValueError:
            continue
        if j.get("type") == "result":
            answer = j.get("result") or ""
    (d / "answer.txt").write_text(answer)
    check = run_check(task, d)
    (d / "check.txt").write_text(f"{check}\n")
    dirty = status(work) != before
    if dirty:
        # made again, with its patch, by the next run on it
        shutil.rmtree(work)
    if tree_state(template) != template_before:
        sys.exit(f"{task['id']} {arm} #{n} changed the index template {template}; "
                 "delete it and the run, then resume")
    shutil.rmtree(index, ignore_errors=True)
    meta = {"task": task["id"], "arm": arm, "run": n, "order": order, "exit": exit_code,
            "stderr": stderr[-2000:], "wall_s": round(wall, 1), "check": check,
            "workspace_dirty": bool(dirty), "model_flag": a.model,
            "started": datetime.fromtimestamp(started, timezone.utc).isoformat(timespec="seconds")}
    (d / "meta.json").write_text(json.dumps(meta, indent=1))
    return meta


def run_check(task, d):
    """The task's check on the answer in `d`; a check may call the scripts
    here through $AGENT_DIR."""
    env = {**os.environ, "AGENT_DIR": str(HERE)}
    return subprocess.run(["sh", "-c", task["check"]], cwd=d, env=env).returncode


def recheck(out, tasks):
    """Score every saved answer again with the task's current check."""
    by_id = {t["id"]: t for t in tasks}
    for meta_path in sorted((out / "runs").glob("*/*/*/meta.json")):
        d = meta_path.parent
        meta = json.loads(meta_path.read_text())
        if meta["task"] not in by_id:
            print(f"{meta['task']} {meta['arm']} #{meta['run']}: no such task now, left as it was")
            continue
        check = run_check(by_id[meta["task"]], d)
        if check != meta["check"]:
            print(f"{meta['task']} {meta['arm']} #{meta['run']}: check {meta['check']} -> {check}")
            meta.setdefault("first_check", meta["check"])
        meta["check"] = check
        (d / "check.txt").write_text(f"{check}\n")
        meta_path.write_text(json.dumps(meta, indent=1))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--greeg", help="greeg binary for arms B and C")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--tasks", help="comma-separated task ids (default: all)")
    ap.add_argument("--arms", default="A,B", help="A, B, C and S; A is the baseline")
    ap.add_argument("--repeats", type=int, default=2)
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--max-turns", type=int, default=25)
    ap.add_argument("--max-budget-usd", type=float, default=2.0)
    ap.add_argument("--timeout", type=int, default=900, help="seconds per run")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--claude", default=shutil.which("claude"))
    ap.add_argument("--recheck", action="store_true", help="only score saved answers again")
    a = ap.parse_args()
    a.out = a.out.resolve()
    if a.recheck:
        return recheck(a.out, tomllib.load(open(HERE / "tasks.toml", "rb"))["task"])
    if not a.greeg:
        ap.error("--greeg is required")
    a.greeg = str(Path(a.greeg).resolve())
    for name, path in (("--greeg", a.greeg), ("--claude", a.claude)):
        if not path or not os.access(path, os.X_OK):
            ap.error(f"{name}: not an executable: {path}")
    if not set(a.arms.split(",")) <= {"A", "B", "C", "S"} or not a.arms.startswith("A"):
        sys.exit("--arms: A first, then any of B, C and S")
    tasks = tomllib.load(open(HERE / "tasks.toml", "rb"))["task"]
    if a.tasks:
        want = a.tasks.split(",")
        missing = set(want) - {t["id"] for t in tasks}
        if missing:
            sys.exit(f"unknown tasks: {', '.join(sorted(missing))}")
        tasks = [t for t in tasks if t["id"] in want]
    for t in tasks:
        if t["corpus"] not in CORPORA:
            sys.exit(f"{t['id']}: unknown corpus {t['corpus']}")
    cells = [(t, arm, n) for n in range(1, a.repeats + 1) for t in tasks for arm in a.arms.split(",")]
    random.Random(a.seed).shuffle(cells)
    a.out.mkdir(parents=True, exist_ok=True)
    setup = {
        "claude": sh([a.claude, "--version"]).strip(), "greeg": sh([a.greeg, "--version"]).strip(),
        "model": a.model, "max_turns": a.max_turns, "max_budget_usd": a.max_budget_usd,
        "seed": a.seed, "allowed": ALLOWED, "denied": DENIED, "tasks": [t["id"] for t in tasks],
        "corpora": {c: sh(["git", "rev-parse", "--short", "HEAD"], cwd=corpus_path(c)).strip()
                    for c in sorted({t["corpus"] for t in tasks})},
    }
    if "S" in a.arms.split(","):
        setup["skill_sha256"] = hashlib.sha256(skill_text(a.out, a.greeg).encode()).hexdigest()
    saved = a.out / "setup.json"
    if saved.exists():
        old = json.loads(saved.read_text())
        # tasks and arms may be added to a run; anything else would mix setups
        changed = [k for k in setup if k not in ("tasks", "seed", "corpora") and old.get(k) != setup[k]]
        old_corpora = old.get("corpora", {})
        changed += [c for c, rev in setup["corpora"].items() if old_corpora.get(c, rev) != rev]
        if changed:
            sys.exit(f"{a.out} was run with another {', '.join(changed)}; use a new --out")
        setup["tasks"] = sorted(set(old.get("tasks", [])) | set(setup["tasks"]))
        setup["corpora"] = {**old_corpora, **setup["corpora"]}
    saved.write_text(json.dumps(setup, indent=1))
    for i, (t, arm, n) in enumerate(cells, 1):
        m = run_cell(a, a.out, t, arm, n, i)
        print(f"[{i}/{len(cells)}] {t['id']} {arm} #{n}: check {m['check']}, exit {m['exit']}, "
              f"{m['wall_s']} s{', workspace changed' if m['workspace_dirty'] else ''}", flush=True)


if __name__ == "__main__":
    main()
