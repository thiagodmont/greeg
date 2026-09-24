#!/usr/bin/env python3
"""Paired index costs on pinned corpora: build, disk, memory, open, freshness and post-edit refresh.

Usage: python3 bench/index_costs.py BASELINE CANDIDATE --corpora tokio,django --output costs.json

Each corpus is copied to a disposable snapshot; each binary gets its own HOME,
cache and index directory, with sessions and statistics disabled. Samples of
the two binaries alternate in a seeded random order. Every process is timed
by wall clock, and by its own CPU time and peak RSS (wait4).
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import shutil
import statistics
import subprocess
import tempfile
import time

from bench import CORPORA, corpus_path
from corpus import Corpus, interrupted_cleanup

EXTENSIONS = {"rust": ".rs", "python": ".py", "kotlin": ".kt", "typescript": ".ts", "javascript": ".js"}
MISS = "nonexistent_zzzz_symbol"
WARMUPS = 3


def sample(argv, cwd, env):
    """Run one process; (wall ms, cpu ms, peak RSS MB, exit, stdout, stderr)."""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        start = time.perf_counter_ns()
        p = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        _, status, usage = os.wait4(p.pid, 0)
        wall = (time.perf_counter_ns() - start) / 1e6
        p.returncode = os.waitstatus_to_exitcode(status)
        out.seek(0)
        err.seek(0)
        rss = usage.ru_maxrss / (1e6 if platform.system() == "Darwin" else 1e3)
        return wall, (usage.ru_utime + usage.ru_stime) * 1e3, rss, p.returncode, out.read(), err.read()


def p95(values):
    return sorted(values)[math.ceil(len(values) * .95) - 1]


def summary(values):
    return {"median": statistics.median(values), "p95": p95(values)}


def change(before, after):
    return {k: 100 * (after[k] / before[k] - 1) if before[k] else None for k in before}


def flagged(wall_change):
    return (wall_change["median"] or 0) > 10 or (wall_change["p95"] or 0) > 20


def edit_target(files, lang):
    """A deterministic mid-listing source file of the corpus language."""
    ext = EXTENSIONS.get(lang)
    candidates = sorted(f for f in files if ext and f.endswith(ext)) or sorted(files)
    return candidates[len(candidates) // 2]


def tree_listing(path):
    """Every file under the index directory: {relative path: bytes}."""
    listing = {}
    for dirpath, _, names in os.walk(path):
        for n in names:
            full = os.path.join(dirpath, n)
            try:
                listing[os.path.relpath(full, path)] = os.lstat(full).st_size
            except OSError:
                pass
    return dict(sorted(listing.items()))


def manifest_path(index):
    """The manifest of the layout directory, `v<N>/`, a binary from 0.8 on
    writes; a top-level one (releases before 0.8, which newer binaries leave
    in place) only when there is none."""
    found = sorted(p for p in Path(index).glob("v*/manifest") if p.parent.name[1:].isdigit())
    if len(found) > 1:
        raise RuntimeError(f"expected one layout manifest in {index}, found {found}")
    if found:
        return found[0]
    top = Path(index) / "manifest"
    if not top.exists():
        raise RuntimeError(f"no manifest in {index}")
    return top


def settle(index, timeout=30.0):
    """Wait until no background build or refresh marker is left in the index."""
    deadline = time.monotonic() + timeout
    quiet = 0
    time.sleep(0.1)
    while quiet < 3:
        busy = any(p.name in ("BUILDING", "REFRESHING") for p in Path(index).rglob("*"))
        quiet = 0 if busy else quiet + 1
        if time.monotonic() > deadline:
            raise RuntimeError(f"background index work did not finish: {index}")
        time.sleep(0.02)


def environment(corpus, label):
    private = corpus.base / label
    private.mkdir()
    env = dict(corpus.env)
    env.update(HOME=str(private), XDG_CONFIG_HOME=str(private / "config"),
               XDG_CACHE_HOME=str(private / "cache"), GREEG_INDEX_DIR=str(private / "index"))
    return env


def measure(name, binaries, args, rng):
    spec = CORPORA[name]
    source = Path(corpus_path(name))
    queries = spec.get("queries", {})
    cases = {"open_miss": ["-l", MISS, "--fresh", "none"],
             "fresh_miss": ["-l", MISS, "--fresh", "stat"]}
    if queries.get("word"):
        cases["word_count"] = ["-c", "-w", queries["word"], "--sort", "path", "--fresh", "stat"]
    for verb in ("def", "refs"):
        if queries.get(verb):
            cases[verb] = [verb, queries[verb], "--fresh", "stat"]
    with Corpus(source) as corpus:
        root = corpus.root
        envs = {label: environment(corpus, label) for label in binaries}
        files = corpus.run(["rg", "--files"]).stdout.decode().splitlines()
        size = sum(os.path.getsize(root / f) for f in files if (root / f).is_file())
        target = edit_target(files, spec.get("lang"))
        entry = {"sha": spec.get("sha"), "files": len(files), "bytes": size, "edit_target": target,
                 "cases": {k: v for k, v in cases.items()}, "build": {}, "queries": []}

        # builds: one full foreground build per sample, from an empty index directory
        builds = {label: [] for label in binaries}
        for _ in range(args.build_runs):
            order = list(binaries)
            rng.shuffle(order)
            for label in order:
                shutil.rmtree(envs[label]["GREEG_INDEX_DIR"], ignore_errors=True)
                wall, cpu, rss, code, _, err = sample([binaries[label], "index", "--quiet"], root, envs[label])
                if code != 0:
                    raise RuntimeError(f"{name}/{label}: index failed: {err[-400:]!r}")
                builds[label].append((wall, cpu, rss))
        for label in binaries:
            idx = Path(envs[label]["GREEG_INDEX_DIR"])
            listing = tree_listing(idx)
            manifest = json.loads(manifest_path(idx).read_text())
            walls, cpus, rsss = zip(*builds[label])
            entry["build"][label] = {
                "wall_ms": summary(walls), "cpu_ms": summary(cpus), "peak_rss_mb": summary(rsss),
                "samples_wall_ms": list(walls), "index_bytes": sum(listing.values()), "index_files": listing,
                "manifest": {k: manifest.get(k) for k in ("files", "build_ms", "phase2_ms", "peak_rss")},
            }
        entry["build"]["wall_change_percent"] = change(entry["build"]["baseline"]["wall_ms"], entry["build"]["candidate"]["wall_ms"])
        entry["build"]["cpu_change_percent"] = change(entry["build"]["baseline"]["cpu_ms"], entry["build"]["candidate"]["cpu_ms"])
        entry["build"]["index_bytes_change_percent"] = 100 * (entry["build"]["candidate"]["index_bytes"] / entry["build"]["baseline"]["index_bytes"] - 1)

        # warm queries against the built index
        for case, query in cases.items():
            argv = {label: [binaries[label], "--no-session", *query] for label in binaries}
            got = {label: [] for label in binaries}
            first = {}
            for i in range(WARMUPS + args.runs):
                order = list(binaries)
                rng.shuffle(order)
                for label in order:
                    wall, cpu, rss, code, out, err = sample(argv[label], root, envs[label])
                    if code not in (0, 1):
                        raise RuntimeError(f"{name}/{label}/{case}: exit {code}: {err[-400:]!r}")
                    first.setdefault(label, (code, out))
                    if i >= WARMUPS:
                        got[label].append((wall, cpu, rss))
            entry["queries"].append(row(name, case, query, got, first))

        # post-edit: a fresh copy of the pristine index and the restored file,
        # then an edit; time the first query (it notices the change) and, once
        # background work settles, the next one (it reads the stored change).
        # A detached writer may still hold an earlier copy, so each sample
        # gets its own directory.
        pristine = {label: corpus.base / label / "pristine" for label in binaries}
        for label in binaries:
            shutil.copytree(envs[label]["GREEG_INDEX_DIR"], pristine[label], symlinks=True)
        path = corpus.path(target)
        original, stat = path.read_bytes(), path.stat()
        first_q = {label: [] for label in binaries}
        second_q = {label: [] for label in binaries}
        outputs = {}
        for i in range(WARMUPS + args.runs):
            order = list(binaries)
            rng.shuffle(order)
            for label in order:
                index = corpus.base / label / f"edit-{i}"
                shutil.copytree(pristine[label], index, symlinks=True)
                env = dict(envs[label], GREEG_INDEX_DIR=str(index))
                path.write_bytes(original)
                os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
                time.sleep(0.02)
                needle = f"greegeditneedle{i}"
                path.write_bytes(original + f"\n// {needle}\n".encode())
                query = [binaries[label], "--no-session", "-l", needle, "--fresh", "stat"]
                a = sample(query, root, env)
                settle(index)
                b = sample(query, root, env)
                settle(index)
                shutil.rmtree(index, ignore_errors=True)
                for r in (a, b):
                    if (r[3], r[4].decode().strip()) != (0, target):
                        raise RuntimeError(f"{name}/{label}: post-edit query missed the edit: {r[3]} {r[4][:200]!r} {r[5][-300:]!r}")
                outputs.setdefault(label, (a[3], a[4]))
                if i >= WARMUPS:
                    first_q[label].append(a[:3])
                    second_q[label].append(b[:3])
        path.write_bytes(original)
        os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        entry["queries"].append(row(name, "post_edit_first", ["-l", "<edit>", "--fresh", "stat"], first_q, outputs))
        entry["queries"].append(row(name, "post_edit_next", ["-l", "<edit>", "--fresh", "stat"], second_q, outputs))
        return entry


def row(corpus, case, query, got, first):
    r = {"corpus": corpus, "case": case, "query": query}
    for label, samples in got.items():
        walls, cpus, rsss = zip(*samples)
        code, out = first[label]
        r[label] = {"median_ms": statistics.median(walls), "p95_ms": p95(walls), "samples_ms": list(walls),
                    "cpu_ms": summary(cpus), "peak_rss_mb": summary(rsss), "exit": code,
                    "stdout_bytes": len(out), "stdout_sha256": hashlib.sha256(out).hexdigest()}
    r["same_output"] = first["baseline"] == first["candidate"]
    if not r["same_output"]:
        # both answers, for diagnosis (the digests alone cannot show what differs)
        r["outputs"] = {label: first[label][1][:65536].decode(errors="replace") for label in first}
    r["wall_change_percent"] = change(*({"median": r[l]["median_ms"], "p95": r[l]["p95_ms"]} for l in ("baseline", "candidate")))
    r["cpu_change_percent"] = change(r["baseline"]["cpu_ms"], r["candidate"]["cpu_ms"])
    r["flagged"] = flagged(r["wall_change_percent"])
    return r


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--corpora", default="tokio,django", help="comma-separated names from bench/corpora.toml")
    parser.add_argument("--runs", type=int, default=21)
    parser.add_argument("--build-runs", type=int, default=5)
    parser.add_argument("--seed", type=int, default=20260924)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.runs < 5 or args.build_runs < 1:
        parser.error("--runs must be at least 5 and --build-runs at least 1")
    names = args.corpora.split(",")
    for name in names:
        if name not in CORPORA or CORPORA[name].get("kind") == "file":
            parser.error(f"not a repository corpus: {name}")
        if not os.path.isdir(corpus_path(name)):
            parser.error(f"{name} is not fetched (python3 bench/bench.py fetch {name})")
    binaries = {"baseline": str(args.baseline.resolve()), "candidate": str(args.candidate.resolve())}
    result = {
        "protocol": 1, "platform": platform.platform(), "machine": platform.machine(), "cpu_count": os.cpu_count(),
        "python": platform.python_version(), "runs": args.runs, "build_runs": args.build_runs, "warmups": WARMUPS,
        "order_seed": args.seed, "load_before": os.getloadavg(),
        "scope": "disposable corpus snapshots, warm page cache, one foreground build per build sample; "
                 "post-edit restores the pristine index and file before each edit",
        "binaries": {}, "corpora": {},
    }
    for label, binary in binaries.items():
        version = subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.strip()
        result["binaries"][label] = {"path": binary, "version": version,
                                     "sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest()}
    rng = random.Random(args.seed)
    with interrupted_cleanup():
        for name in names:
            entry = measure(name, binaries, args, rng)
            result["corpora"][name] = entry
            b = entry["build"]
            print(f"{name}: build wall {b['wall_change_percent']['median']:+.1f}% cpu {b['cpu_change_percent']['median']:+.1f}% "
                  f"({b['baseline']['wall_ms']['median']:.0f} → {b['candidate']['wall_ms']['median']:.0f} ms)  "
                  f"disk {b['index_bytes_change_percent']:+.1f}% ({b['baseline']['index_bytes'] / 1e6:.1f} MB)  "
                  f"rss {b['baseline']['peak_rss_mb']['median']:.0f} → {b['candidate']['peak_rss_mb']['median']:.0f} MB")
            for r in entry["queries"]:
                w = r["wall_change_percent"]
                print(f"  {r['case']:16} {r['baseline']['median_ms']:8.2f} → {r['candidate']['median_ms']:8.2f} ms  "
                      f"median {w['median']:+6.1f}%  p95 {w['p95']:+6.1f}%  cpu {r['cpu_change_percent']['median']:+6.1f}%"
                      + ("  FLAG" if r["flagged"] else "") + ("" if r["same_output"] else "  output differs"))
    result["load_after"] = os.getloadavg()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=1) + "\n")
    print(f"load {result['load_before']} → {result['load_after']}; wrote {args.output}")


if __name__ == "__main__":
    main()
