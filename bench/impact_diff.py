#!/usr/bin/env python3
"""Offline precision and recall of `greeg impact --diff` on the agent eval's
diff tasks: the uses it marks direct, against each task's verified sites and
traps.

    bench/impact_diff.py --greeg BIN [--tasks ID,…] [--runs 5] [--output R.json]

Each task's corpus is cloned (copy-on-write) into a scratch directory, indexed
there before the task's patch is applied (the files the corpus setup left
untracked are excluded first, as the agent harness does) (the edits are newer than the index,
as an agent finds them), and asked `impact --diff --json=greeg --budget 0`.
A site counts as found when a direct use is within two lines of it, in a file
whose path ends with the site's; a trap counts when a direct use is that close
to it. Direct uses in the changed files, or near a neutral place, are neither
right nor wrong; any other is an extra. Latency is the median and p95 wall
time of `--runs` warm runs. Uses the bench cache's corpora and its own index
directory; statistics and sessions are off.
"""
import argparse, json, os, shutil, statistics, subprocess, sys, tempfile, time, tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "agent"))
from run import apply_patches, corpus_path, exclude_untracked, patches  # noqa: E402
from check_sites import NEAR, changed_files  # noqa: E402


def near(path, line, place):
    p, n = place.rsplit(":", 1)
    return (path == p or path.endswith("/" + p) or p.endswith("/" + path)) and abs(line - int(n)) <= NEAR


def records(out):
    return [json.loads(l) for l in out.splitlines() if l.strip()]


def score(task, recs):
    uses = [(u["path"]["text"], u["line"]) for r in recs if r["type"] == "change"
            for u in r["data"]["uses"] if u["direct"]]
    changed = changed_files(task["id"])
    found = [s for s in task["sites"] if any(near(p, n, s) for p, n in uses)]
    traps = sorted({t for t in task.get("traps", []) for p, n in uses if near(p, n, t)})
    extras = sorted({f"{p}:{n}" for p, n in uses
                     if not any(near(p, n, s) for s in task["sites"] + task.get("traps", []) + task.get("neutral", []))
                     and p not in changed})
    return {"sites": len(task["sites"]), "found": len(found),
            "missed": [s for s in task["sites"] if s not in found],
            "traps": traps, "extras": extras, "direct_uses": len(uses)}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--greeg", required=True)
    ap.add_argument("--tasks")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--output")
    a = ap.parse_args()
    greeg = str(Path(a.greeg).resolve())
    tasks = [t for t in tomllib.load(open(HERE / "agent" / "tasks.toml", "rb"))["task"] if patches(t)]
    if a.tasks:
        tasks = [t for t in tasks if t["id"] in a.tasks.split(",")]
    scratch = Path(tempfile.mkdtemp(prefix="greeg-impact-diff-"))
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GREEG_", "RIPGREP_"))}
    env.update(GREEG_INDEX_DIR=str(scratch / "index"), GREEG_STATS="0",
               GREEG_CONFIG_DIR="/dev/null/greeg-config")
    rows = []
    try:
        for t in tasks:
            work = scratch / t["id"]
            subprocess.run(["cp", "-cR", str(corpus_path(t["corpus"])), str(work)], check=True)
            exclude_untracked(work)
            subprocess.run([greeg, "index"], cwd=work, env=env, check=True, capture_output=True)
            apply_patches(work, t)
            cmd = [greeg, "impact", "--diff", "--json=greeg", "--budget", "0", "--no-session"]
            times, out = [], ""
            for _ in range(a.runs + 1):  # the first run applies the edits to the index
                t0 = time.perf_counter()
                p = subprocess.run(cmd, cwd=work, env=env, capture_output=True, text=True)
                times.append((time.perf_counter() - t0) * 1e3)
                out = p.stdout
            s = score(t, records(out))
            warm = sorted(times[1:])
            s.update(id=t["id"], held_out=t.get("held_out", False), exit=p.returncode,
                     ms_median=round(statistics.median(warm), 1),
                     ms_p95=round(warm[min(len(warm) - 1, int(0.95 * len(warm)))], 1),
                     ms_first=round(times[0], 1))
            rows.append(s)
            print(f"{t['id']:36} found {s['found']:>2}/{s['sites']:<2} traps {len(s['traps'])} "
                  f"extras {len(s['extras'])}  {s['ms_median']} ms (p95 {s['ms_p95']}, first {s['ms_first']})",
                  flush=True)
            shutil.rmtree(work)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    for split in (False, True):
        part = [r for r in rows if r["held_out"] == split]
        if not part:
            continue
        sites = sum(r["sites"] for r in part)
        found = sum(r["found"] for r in part)
        direct = sum(r["direct_uses"] for r in part)
        wrong = sum(len(r["traps"]) + len(r["extras"]) for r in part)
        print(f"{'held out' if split else 'development'}: recall {found}/{sites} = {found / sites:.3f}, "
              f"traps+extras {wrong} of {direct} direct uses")
    if a.output:
        version = subprocess.run([greeg, "--version"], capture_output=True, text=True).stdout.strip()
        json.dump({"greeg": version, "results": rows}, open(a.output, "w"), indent=1)


if __name__ == "__main__":
    main()
