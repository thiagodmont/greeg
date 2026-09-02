#!/usr/bin/env python3
"""Paired bootstrap over runs.json (from extract.py): arm B and C vs arm A.

    bench/agent/analyze.py runs.json [--pairs 10000]

For each metric (success rate, turns, search calls, output tokens, cost, wall
time) pairs runs by (task, run number) across arms, reports the mean paired
difference and a 95 % bootstrap interval. Success is reported as a rate with
the interval on the difference in rates.
"""
import json, random, statistics, sys

METRICS = [("success", "success rate"), ("turns", "turns"), ("search_calls", "search calls"), ("output_tokens", "output tokens"), ("input_tokens", "input tokens"), ("cost_usd", "cost USD"), ("duration_s", "wall time s")]


def value(r, m):
    if m == "success":
        return None if r["success"] is None else float(r["success"])
    if m == "search_calls":
        return float(sum(r["search"].values()))
    if m == "duration_s":
        return None if r["duration_ms"] is None else r["duration_ms"] / 1000
    v = r.get(m)
    return None if v is None else float(v)


def main():
    runs = json.load(open(sys.argv[1]))
    pairs = int(sys.argv[sys.argv.index("--pairs") + 1]) if "--pairs" in sys.argv else 10000
    by = {}
    for r in runs:
        by.setdefault((r["task"], r["run"]), {})[r["arm"]] = r
    arms = sorted({r["arm"] for r in runs})
    base = arms[0]
    rnd = random.Random(7)
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
