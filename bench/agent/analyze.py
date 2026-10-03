#!/usr/bin/env python3
"""Paired bootstrap over runs.json (from extract.py): each arm vs arm A.

    bench/agent/analyze.py runs.json [--pairs 10000]

For each metric (success rate, turns, search calls, tool output, tokens,
cache writes and reads, cost, wall time) pairs runs by (task, run number) across arms, reports the mean paired
difference and a 95 % bootstrap interval. Success is reported as a rate with
the interval on the difference in rates. Runs marked invalid by extract.py
are listed and left out, and so are their pairs.
"""
import json, random, statistics, sys

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
    ev = {}
    for r in runs:
        for k, v in r["search_evidence"].items():
            ev.setdefault(r["arm"], {}).setdefault(k, 0)
            ev[r["arm"]][k] += v
    for arm, e in sorted(ev.items()):
        print(f"arm {arm} search calls decided by: " + ", ".join(f"{k} {v}" for k, v in e.items()))
    return [r for r in runs if not r.get("invalid")]


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


def main():
    runs = json.load(open(sys.argv[1]))
    pairs = int(sys.argv[sys.argv.index("--pairs") + 1]) if "--pairs" in sys.argv else 10000
    runs = validity(runs)
    by = {}
    for r in runs:
        by.setdefault((r["task"], r["run"]), {})[r["arm"]] = r
    arms = sorted({r["arm"] for r in runs})
    if "A" not in arms:
        sys.exit("no valid arm A runs: nothing to compare against")
    base = "A"
    rnd = random.Random(7)
    per_task(runs, arms)
    print(f"{'metric':14} " + " ".join(f"{a:>10}" for a in arms) + "   paired difference vs " + base + " (95 % bootstrap)")
    for m, label in METRICS:
        means = {a: statistics.mean([v for v in (value(r, m) for r in runs if r["arm"] == a) if v is not None] or [float('nan')]) for a in arms}
        line = f"{label:14} " + " ".join(f"{means[a]:10.2f}" for a in arms)
        for a in arms[1:]:
            diffs = []
            for k, d in by.items():
                if base in d and a in d:
                    x, y = value(d[base], m), value(d[a], m)
                    if x is not None and y is not None:
                        diffs.append(y - x)
            if not diffs:
                continue
            boots = sorted(statistics.mean(rnd.choices(diffs, k=len(diffs))) for _ in range(pairs))
            lo, hi = boots[int(0.025 * pairs)], boots[int(0.975 * pairs)]
            line += f"   {a}: {statistics.mean(diffs):+.2f} [{lo:+.2f}, {hi:+.2f}] n={len(diffs)}"
        print(line)


if __name__ == "__main__":
    main()
