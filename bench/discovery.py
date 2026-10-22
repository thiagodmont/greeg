#!/usr/bin/env python3
"""Discovery evaluation: questions without a known identifier (phase 7).

    bench/discovery.py sample GREEG OUT.json      draw the written queries' targets
    bench/discovery.py fetch                      SWE-bench Verified django rows → cache
    bench/discovery.py run METHOD GREEG [--split dev|heldout|all --final] [--set written|swebench|all] --output R.json
    bench/discovery.py score R.json [R2.json …] [--vs BASELINE.json]

GREEG is the greeg binary; rg must be on PATH. `score` gives Acc@1/5/10 and
MRR with 95 % bootstrap intervals, and with `--vs` the paired difference in
Acc@5 and MRR from the baseline over the same queries.

METHOD is `rg` (files ranked by how many of the query's words they hold, then
by hits) or `greeg-search` (greeg's ranked search over the same words). The
dataset is bench/discovery/dataset.toml, frozen before any ranker work.
Written queries run on the pinned corpora in ~/.cache/greeg-bench/corpora;
each SWE-bench row runs on django checked out at its base commit in a
disposable worktree, with its own index directory.
"""
import argparse, hashlib, json, os, random, re, shutil, statistics, subprocess, sys, tempfile, time, tomllib, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
DATASET = os.path.join(HERE, "discovery", "dataset.toml")
CACHE = os.path.expanduser("~/.cache/greeg-bench")
CORPORA = os.path.join(CACHE, "corpora")
ROWS = os.path.join(CACHE, "discovery", "swebench_verified_django.json")
TOP = 20

STOP = set("""
a about above after again against all also an and any are as at be because been before being below between
both but by can could did do does doing down during each few for from further had has have having here how
if in into is it its itself just more most no nor not now of off on once only or other our out over own same
should so some such than that the their them then there these they this those through to too under until up
very was we were what when where which while who whom why will with would you your i me my he she his her
code function method file files where which does how what use used uses using get gets set sets way
""".split())


def words(text):
    """The query's distinct words, as an agent would grep them: identifiers and
    words of three or more characters, lowercased, stop words left out."""
    out, seen = [], set()
    for w in re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", text):
        w = w.lower()
        if w not in STOP and w not in seen:
            seen.add(w)
            out.append(w)
    return out


def env_for(index_dir):
    e = dict(os.environ, GREEG_STATS="0", GREEG_CONFIG_DIR="/dev/null/greeg-config", GREEG_INDEX_DIR=index_dir)
    e.pop("GREEG_BUDGET", None)
    return e


def rank_rg(root, ws):
    """Files by distinct query words matched, then total hits, then path."""
    if not ws:
        return []
    args = ["rg", "-o", "-i", "-w", "-F", "--no-heading", "--no-filename", "--with-filename", "--no-line-number"]
    for w in ws:
        args += ["-e", w]
    r = subprocess.run(args + ["."], cwd=root, capture_output=True, stdin=subprocess.DEVNULL)
    if r.returncode > 1:
        sys.exit(f"rg failed ({r.returncode}): {r.stderr.decode(errors='replace')[:300]}")
    per = {}
    for line in r.stdout.decode("utf-8", "replace").splitlines():
        path, _, hit = line.rpartition(":")
        d = per.setdefault(path.removeprefix("./"), [set(), 0])
        d[0].add(hit.lower())
        d[1] += 1
    return sorted(per, key=lambda p: (-len(per[p][0]), -per[p][1], p))[:TOP]


def rank_greeg_search(greeg, root, ws, index_dir):
    """greeg's ranked search for any of the words: files in answer order."""
    if not ws:
        return []
    pattern = "|".join(re.escape(w) for w in ws)
    r = subprocess.run([greeg, "--no-session", "-i", "-w", "--json=greeg", "--budget", "20000", "-e", pattern],
                       cwd=root, capture_output=True, stdin=subprocess.DEVNULL, env=env_for(index_dir))
    if r.returncode > 1:
        sys.exit(f"greeg failed ({r.returncode}): {r.stderr.decode(errors='replace')[:300]}")
    order = []
    for line in r.stdout.splitlines():
        rec = json.loads(line)
        if rec["type"] == "begin":
            p = rec["data"]["path"]["text"]
            if p not in order:
                order.append(p)
    return order[:TOP]


def rank(method, greeg, root, text, index_dir):
    ws = words(text)
    if method == "rg":
        return rank_rg(root, ws)
    if method == "greeg-search":
        return rank_greeg_search(greeg, root, ws, index_dir)
    sys.exit(f"unknown method {method}")


def digest(s):
    return hashlib.sha256(s.encode()).hexdigest()


def load(split, which):
    """The dataset's rows for a split; `gold` is a row's answer files, or their
    digests for a held-out row (`hashed`)."""
    d = tomllib.load(open(DATASET, "rb"))
    keep = lambda x: split == "all" or x["split"] == split
    rows = []
    for x in d["query"] + d["swebench"]:
        x["gold"], x["hashed"] = (x["files"], False) if "files" in x else (x["files_sha256"], True)
        rows.append(x)
    written = [x for x in d["query"] if keep(x)] if which in ("written", "all") else []
    swe = [x for x in d["swebench"] if keep(x)] if which in ("swebench", "all") else []
    return d, written, swe


def clean_at(root, corpus):
    """`root` holds exactly the corpus's revision: no local edits, and no
    untracked files but those the dataset names, with their digests."""
    if git("rev-parse", "HEAD", cwd=root) != corpus["revision"]:
        sys.exit(f"{root} is not at {corpus['revision']}")
    known = corpus.get("untracked", {})
    for line in git("status", "--porcelain", "--untracked-files=all", cwd=root).splitlines():
        path = line[3:]
        if not (line.startswith("?? ") and path in known
                and hashlib.sha256(open(os.path.join(root, path), "rb").read()).hexdigest() == known[path]):
            sys.exit(f"{root} has a local change: {line}")


def git(*args, cwd):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def run(args):
    if args.split != "dev" and not args.final:
        sys.exit("held-out rows are scored once, after tuning: pass --final")
    d, written, swe = load(args.split, args.set)
    out = {"method": args.method, "greeg": subprocess.run([args.greeg, "--version"], capture_output=True, text=True).stdout.strip(),
           "split": args.split, "set": args.set, "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "results": []}
    tmp = tempfile.mkdtemp(prefix="greeg-discovery-")
    try:
        for c in sorted({x["corpus"] for x in written}):
            root = os.path.join(CORPORA, c)
            clean_at(root, d["corpora"][c])
            idx = os.path.join(tmp, "idx-" + c)
            if args.method.startswith("greeg"):
                subprocess.run([args.greeg, "index", "--quiet"], cwd=root, env=env_for(idx), check=True, stdin=subprocess.DEVNULL)
            for x in (x for x in written if x["corpus"] == c):
                t = time.perf_counter()
                got = rank(args.method, args.greeg, root, x["query"], idx)
                out["results"].append(result(x, "written", c, got, t))
        if swe:
            rows = {r["instance_id"]: r for r in json.load(open(ROWS))}
            for x in swe:
                if digest(rows[x["id"]]["problem_statement"]) != x["statement_sha256"]:
                    sys.exit(f"{x['id']}: the cached issue text is not the frozen one")
            repo = os.path.join(CORPORA, "django")
            wt = os.path.join(tmp, "django")
            git("worktree", "add", "--detach", "--quiet", wt, swe[0]["base_commit"], cwd=repo)
            try:
                for x in swe:
                    git("checkout", "--quiet", "--detach", x["base_commit"], cwd=wt)
                    idx = os.path.join(tmp, "idx-django")
                    shutil.rmtree(idx, ignore_errors=True)
                    if args.method.startswith("greeg"):
                        subprocess.run([args.greeg, "index", "--quiet"], cwd=wt, env=env_for(idx), check=True, stdin=subprocess.DEVNULL)
                    t = time.perf_counter()
                    got = rank(args.method, args.greeg, wt, rows[x["id"]]["problem_statement"], idx)
                    out["results"].append(result(x, "swebench", "django", got, t))
            finally:
                git("worktree", "remove", "--force", wt, cwd=repo)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    json.dump(out, open(args.output, "w"), indent=1)
    print(f"wrote {len(out['results'])} results to {args.output}")


def result(x, which, corpus, ranked, t0):
    return {"id": x["id"], "set": which, "corpus": corpus, "split": x["split"], "gold": x["gold"],
            "hashed": x["hashed"], "ranked": ranked, "ms": (time.perf_counter() - t0) * 1e3}


def first_hit(ranked, gold, hashed=False):
    """1-based rank of the first gold file, or None past the list."""
    for i, p in enumerate(ranked, 1):
        if (digest(p) if hashed else p) in gold:
            return i
    return None


def metrics(rs):
    ranks = [first_hit(r["ranked"], r["gold"], r.get("hashed", False)) for r in rs]
    m = {f"acc@{k}": [float(x is not None and x <= k) for x in ranks] for k in (1, 5, 10)}
    m["mrr"] = [1 / x if x else 0.0 for x in ranks]
    return m


def boot(xs, n=4000, seed=7):
    rnd = random.Random(seed)
    means = sorted(statistics.fmean(rnd.choices(xs, k=len(xs))) for _ in range(n))
    return statistics.fmean(xs), means[int(0.025 * n)], means[int(0.975 * n) - 1]


def groups_of(results):
    g = {}
    for x in results:
        g.setdefault((x["set"], x["corpus"]), []).append(x)
        g.setdefault((x["set"], "all"), []).append(x)
        g.setdefault(("pooled", "all"), []).append(x)
    return g


def paired(rs, base, key, n=4000, seed=7):
    """Mean per-query difference in `key` from the baseline, 95 % bootstrap."""
    b = {x["id"]: x for x in base}
    pairs = [(x, b[x["id"]]) for x in rs if x["id"] in b]
    if len(pairs) != len(rs):
        sys.exit("the baseline does not hold the same queries")
    d = [a - c for a, c in zip(metrics([p[0] for p in pairs])[key], metrics([p[1] for p in pairs])[key])]
    return boot(d, n, seed)


def score(paths, vs=None):
    base = json.load(open(vs))["results"] if vs else None
    for p in paths:
        r = json.load(open(p))
        print(f"## {r['method']} · {r['greeg']} · split {r['split']}" + (f" · vs {json.load(open(vs))['method']}" if vs else ""))
        for (s, c), rs in sorted(groups_of(r["results"]).items()):
            m = metrics(rs)
            cells = []
            for k in ("acc@1", "acc@5", "acc@10", "mrr"):
                mean, lo, hi = boot(m[k])
                cells.append(f"{k} {mean:.2f} [{lo:.2f}–{hi:.2f}]")
            ms = statistics.median(x["ms"] for x in rs)
            print(f"{s:9} {c:15} n={len(rs):3}  " + "  ".join(cells) + f"  median {ms:.0f} ms")
            if base is not None:
                ids = {x["id"] for x in rs}
                b = [x for x in base if x["id"] in ids]
                diffs = [f"Δ{k} {m:+.2f} [{lo:+.2f}–{hi:+.2f}]" for k in ("acc@5", "mrr") for m, lo, hi in [paired(rs, b, k)]]
                print(f"{'':9} {'':15} {'':5}  " + "  ".join(diffs))


TS_SKIP = r"(^|/)(testRunner|harness|tests?)/|\.d\.ts$|^src/lib/"
SPEC = {
    "tokio": (r"^\s*(pub(\([a-z]+\))?\s+)?(const\s+)?(async\s+)?(unsafe\s+)?fn\s+([a-z_][a-z0-9_]*)", "*.rs",
              lambda p: "/src/" in "/" + p and not re.search(r"(^|/)(tests?|benches|examples|fuzz|stress-test)/", p)),
    "TypeScript-5.9": (r"^\s*(export\s+)?function\s+([A-Za-z_][A-Za-z0-9_]*)", "*.ts",
                       lambda p: p.startswith("src/") and not re.search(TS_SKIP, p)),
    "ktor": (r"^\s*((public|internal|private|protected|override|suspend|inline|operator|actual|expect)\s+)*fun\s+(<[^>]+>\s+)?([A-Za-z_][\w<>, ?]*\.)?([a-zA-Z_][A-Za-z0-9_]*)\s*\(", "*.kt",
             lambda p: "/src/" in "/" + p and not re.search(r"(^|/)(test|jvmTest|commonTest|jsTest|posixTest|nativeTest|test-utils|samples?)/|Test[A-Za-z]*\.kt$|/build/", p)),
}
BORING = {"new", "fmt", "drop", "from", "default", "clone", "eq", "hash", "deref", "deref_mut", "into", "as_ref",
          "toString", "equals", "hashCode", "main", "invoke", "get", "set", "len", "is_empty", "next", "close"}


def sample(greeg, out_path):
    """30 definitions per corpus: non-test files, 10–60 lines, one per file,
    seeded, each with its code as `greeg show` prints it. A test function
    inside a source file can be drawn (tokio-09 is one)."""
    d = tomllib.load(open(DATASET, "rb"))
    out = {}
    for corpus, (rx, glob, keep) in SPEC.items():
        root = os.path.join(CORPORA, corpus)
        clean_at(root, d["corpora"][corpus])
        r = subprocess.run(["rg", "-n", "--no-heading", "-g", glob, "-e", rx, "."], cwd=root, capture_output=True, text=True, stdin=subprocess.DEVNULL)
        cands = []
        for line in r.stdout.splitlines():
            path, ln, text = line.split(":", 2)
            path = path.removeprefix("./")
            name = re.search(rx, text).groups()[-1]
            if not keep(path) or name in BORING or len(name) < 4 or "test" in name.lower() \
                    or re.search(r"(^|/)tests?\.rs$|_tests?\.rs$", path):
                continue
            cands.append((path, int(ln), name))
        # rg prints in search order, which varies
        cands.sort()
        rnd = random.Random(f"phase7-{corpus}")
        rnd.shuffle(cands)
        picked, seen = [], set()
        idx = os.path.join(CACHE, "discovery", "idx")
        for path, ln, name in cands:
            if path in seen:
                continue
            s = subprocess.run([greeg, "show", f"{path}:{ln}", "--no-session"], cwd=root, capture_output=True, text=True, env=env_for(idx), stdin=subprocess.DEVNULL)
            if s.returncode or not 10 <= s.stdout.count("\n") <= 60 or name not in s.stdout:
                continue
            picked.append({"corpus": corpus, "path": path, "line": ln, "name": name, "code": s.stdout})
            seen.add(path)
            if len(picked) == 30:
                break
        if len(picked) < 30:
            sys.exit(f"{corpus}: only {len(picked)} definitions qualify")
        out[corpus] = picked
    json.dump(out, open(out_path, "w"), indent=1)


def fetch():
    rows = []
    for off in range(0, 500, 100):
        u = f"https://datasets-server.huggingface.co/rows?dataset=princeton-nlp%2FSWE-bench_Verified&config=default&split=test&offset={off}&length=100"
        rows += [r["row"] for r in json.load(urllib.request.urlopen(u, timeout=60))["rows"]]
    os.makedirs(os.path.dirname(ROWS), exist_ok=True)
    json.dump([r for r in rows if r["repo"] == "django/django"], open(ROWS, "w"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sample"); s.add_argument("greeg"); s.add_argument("out")
    sub.add_parser("fetch")
    r = sub.add_parser("run"); r.add_argument("method"); r.add_argument("greeg")
    r.add_argument("--split", default="dev", choices=["dev", "heldout", "all"])
    r.add_argument("--set", default="all", choices=["written", "swebench", "all"])
    r.add_argument("--final", action="store_true", help="tuning is over: score held-out rows")
    r.add_argument("--output", required=True)
    c = sub.add_parser("score"); c.add_argument("results", nargs="+"); c.add_argument("--vs")
    a = ap.parse_args()
    if a.cmd == "sample":
        sample(a.greeg, a.out)
    elif a.cmd == "fetch":
        fetch()
    elif a.cmd == "run":
        run(a)
    else:
        score(a.results, a.vs)


if __name__ == "__main__":
    main()
