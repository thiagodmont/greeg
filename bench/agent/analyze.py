#!/usr/bin/env python3
"""Paired bootstrap over runs.json (from extract.py): each arm vs arm A.

    bench/agent/analyze.py runs.json [--pairs 10000] [--tasks tasks.toml]

For each metric (success rate, turns, search calls, tool output, tokens,
cache writes and reads, cost, wall time) pairs runs by (task, run number)
across arms and reports the mean paired difference with two 95 % bootstrap
intervals: over pairs, and over tasks (whole tasks resampled, so runs of one
task do not count as independent evidence). Each arm's success rate also gets
a 95 % Wilson interval. Runs marked invalid by extract.py are listed and left
out, and so are their pairs and any (task, run) that lacks a run in some arm.
Tasks marked `held_out` in tasks.toml are reported apart: greeg is not tuned
on them, so they are the ones a decision rests on.
"""
import json, math, os, random, statistics, sys, tomllib

METRICS = [("success", "success rate"), ("turns", "turns"), ("search_calls", "search calls"), ("tool_output_chars", "tool output ch"), ("output_tokens", "output tokens"), ("input_tokens", "input tokens"), ("cache_creation", "cache writes"), ("cache_read", "cache reads"), ("cost_usd", "cost USD"), ("duration_s", "wall time s")]


def value(r, m):
    if m == "success":
        return None if r["success"] is None else float(r["success"])
    if m == "search_calls":
        return float(sum(r["search"].values()))
    if m == "duration_s":
        return None if r["duration_ms"] is None else r["duration_ms"] / 1000
    v = r.get(m)
    return None if v is None else float(v)


def validity(runs):
    """Runs left out of the comparison, and why."""
    bad = [r for r in runs if r.get("invalid")]
    for r in bad:
        print(f"left out: {r['task']} {r['arm']} #{r['run']}: {'; '.join(r['invalid'])}")
    # every arm averages the same (task, run) pairs
    unpaired = {(r["task"], r["run"]) for r in bad}
    for r in runs:
        if not r.get("invalid") and (r["task"], r["run"]) in unpaired:
            print(f"left out: {r['task']} {r['arm']} #{r['run']}: a run it pairs with is invalid")
    arms = {r["arm"] for r in runs}
    have = {}
    for r in runs:
        have.setdefault((r["task"], r["run"]), set()).add(r["arm"])
    for r in runs:
        key = (r["task"], r["run"])
        if key not in unpaired and have[key] != arms:
            print(f"left out: {r['task']} {r['arm']} #{r['run']}: no run in arm {', '.join(sorted(arms - have[key]))}")
    unpaired |= {k for k, a in have.items() if a != arms}
    ev = {}
    for r in runs:
        for k, v in r["search_evidence"].items():
            ev.setdefault(r["arm"], {}).setdefault(k, 0)
            ev[r["arm"]][k] += v
    for arm, e in sorted(ev.items()):
        print(f"arm {arm} search calls decided by: " + ", ".join(f"{k} {v}" for k, v in e.items()))
    return [r for r in runs if (r["task"], r["run"]) not in unpaired]


def per_task(runs, arms):
    print(f"\n{'task':34} " + " ".join(f"{a + ' ok':>6} {a + ' $':>7} {a + ' turns':>8}" for a in arms))
    for task in sorted({r["task"] for r in runs}):
        cells = []
        for a in arms:
            rs = [r for r in runs if r["task"] == task and r["arm"] == a]
            scored = [r for r in rs if r["success"] is not None]
            ok = sum(1 for r in scored if r["success"])
            costs = [r["cost_usd"] for r in rs if r["cost_usd"] is not None]
            cost = statistics.mean(costs) if costs else float("nan")
            turns = statistics.mean([r["turns"] for r in rs]) if rs else float("nan")
            cells.append(f"{ok:>3}/{len(scored):<2} {cost:7.3f} {turns:8.1f}")
        print(f"{task:34} " + " ".join(cells))
    print()


def wilson(k, n, z=1.96):
    """95 % Wilson score interval for k successes out of n."""
    if n == 0:
        return float("nan"), float("nan")
    p = k / n
    centre = (p + z * z / (2 * n)) / (1 + z * z / n)
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return centre - half, centre + half


def interval(samples, pairs, rnd, draw):
    boots = sorted(statistics.mean(draw(samples, rnd)) for _ in range(pairs))
    return boots[int(0.025 * pairs)], boots[int(0.975 * pairs)]


def over_pairs(diffs, rnd):
    return rnd.choices([d for ds in diffs.values() for d in ds], k=sum(map(len, diffs.values())))


def over_tasks(diffs, rnd):
    """All pairs of tasks drawn with replacement."""
    tasks = list(diffs)
    return [d for t in rnd.choices(tasks, k=len(tasks)) for d in diffs[t]]


def held_out(path):
    try:
        with open(path, "rb") as fh:
            return {t["id"] for t in tomllib.load(fh)["task"] if t.get("held_out")}
    except OSError as e:
        # without it, held-out runs would read as development ones
        sys.exit(f"{path}: {e.strerror}")


def report(runs, pairs, title):
    by = {}
    for r in runs:
        by.setdefault((r["task"], r["run"]), {})[r["arm"]] = r
    arms = sorted({r["arm"] for r in runs})
    base = "A"
    rnd = random.Random(7)
    if title:
        print(f"== {title}: {len({r['task'] for r in runs})} tasks")
    per_task(runs, arms)
    for a in arms:
        scored = [r for r in runs if r["arm"] == a and r["success"] is not None]
        k, n = sum(1 for r in scored if r["success"]), len(scored)
        lo, hi = wilson(k, n)
        print(f"success {a}: {k}/{n} ({k / n * 100 if n else float('nan'):.0f} %, 95 % Wilson [{lo * 100:.0f}, {hi * 100:.0f}])")
    print()
    print(f"{'metric':14} " + " ".join(f"{a:>10}" for a in arms) + "   paired difference vs " + base + " (95 % bootstrap: over pairs | over tasks)")
    for m, label in METRICS:
        means = {a: statistics.mean([v for v in (value(r, m) for r in runs if r["arm"] == a) if v is not None] or [float('nan')]) for a in arms}
        line = f"{label:14} " + " ".join(f"{means[a]:10.2f}" for a in arms)
        for a in arms[1:]:
            diffs = {}
            for (task, _), d in by.items():
                if base in d and a in d:
                    x, y = value(d[base], m), value(d[a], m)
                    if x is not None and y is not None:
                        diffs.setdefault(task, []).append(y - x)
            if not diffs:
                continue
            n = sum(map(len, diffs.values()))
            mean = statistics.mean(d for ds in diffs.values() for d in ds)
            lo, hi = interval(diffs, pairs, rnd, over_pairs)
            tlo, thi = interval(diffs, pairs, rnd, over_tasks)
            line += f"   {a}: {mean:+.2f} [{lo:+.2f}, {hi:+.2f}] | [{tlo:+.2f}, {thi:+.2f}] n={n} tasks={len(diffs)}"
        print(line)


def main():
    with open(sys.argv[1]) as fh:
        runs = json.load(fh)
    pairs = int(sys.argv[sys.argv.index("--pairs") + 1]) if "--pairs" in sys.argv else 10000
    tasks = sys.argv[sys.argv.index("--tasks") + 1] if "--tasks" in sys.argv else os.path.join(os.path.dirname(__file__), "tasks.toml")
    runs = validity(runs)
    if "A" not in {r["arm"] for r in runs}:
        sys.exit("no valid arm A runs: nothing to compare against")
    kept = held_out(tasks)
    if not kept:
        return report(runs, pairs, None)
    parts = [("development tasks", [r for r in runs if r["task"] not in kept]),
             ("held-out tasks", [r for r in runs if r["task"] in kept])]
    for i, (title, rs) in enumerate((t, rs) for t, rs in parts if rs):
        if i:
            print()
        report(rs, pairs, title)


if __name__ == "__main__":
    main()
