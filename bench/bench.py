#!/usr/bin/env python3
"""greeg benchmark suite (PLAN.md M6).

    bench/bench.py fetch  [NAME...]                    shallow-clone pinned corpora into the cache
    bench/bench.py speed  [--corpora a,b] [--cold]     hyperfine protocol vs grep / rg, index build, RSS
    bench/bench.py oracle CORPUS...                    SCIP-based quality protocol (definitions, refs, context)
    bench/bench.py gate   speed|kernels [--tolerance]  compare with the saved baseline for this host
    bench/bench.py report                              write docs/BENCH.md from bench/results

Corpora and per-corpus queries live in bench/corpora.toml. Results are JSON
under bench/results/ (one file per protocol and host), and `report` renders
them. External tools: git, hyperfine, rg, grep; for the oracle rust-analyzer
(`rust-analyzer scip`), `npx @sourcegraph/scip-typescript`,
`npx @sourcegraph/scip-python`, and scip-java for Kotlin when installed.
"""
import argparse, datetime, json, math, os, platform, random, re, shlex, shutil, statistics, subprocess, sys, tempfile, time, tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RESULTS = os.path.join(HERE, "results")
BASELINES = os.path.join(HERE, "baselines")
DEVNULL = subprocess.DEVNULL
MAC = platform.system() == "Darwin"

with open(os.path.join(HERE, "corpora.toml"), "rb") as fh:
    CORPORA = tomllib.load(fh)


# ───────────────────────────── helpers ─────────────────────────────

def cache_dir():
    return os.environ.get("GREEG_BENCH_CACHE") or os.path.expanduser("~/.cache/greeg-bench/corpora")


def corpus_path(name):
    spec = CORPORA[name]
    return os.path.join(cache_dir(), spec.get("dir", name))


def host_key():
    cpu = ""
    try:
        if MAC:
            cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
        else:
            for line in open("/proc/cpuinfo"):
                if line.startswith("model name"):
                    cpu = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    slug = re.sub(r"[^a-z0-9]+", "-", cpu.lower()).strip("-") or "unknown"
    return f"{platform.system().lower()}-{platform.machine().lower()}-{slug}"


def host_info():
    return {"key": host_key(), "os": platform.platform(), "python": platform.python_version(), "cpus": os.cpu_count()}


def run(args, cwd=None, timeout=600, env=None):
    return subprocess.run(args, cwd=cwd, stdin=DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, env=env)


def out_of(args, cwd=None, timeout=600):
    return run(args, cwd, timeout).stdout.decode("utf8", "replace")


def version_of(args):
    try:
        return out_of(args, timeout=20).strip().splitlines()[0]
    except (OSError, IndexError):
        return "missing"


def fmt_ms(x):
    return "–" if x is None else (f"{x*1000:.1f} ms" if x < 1 else f"{x:.2f} s")


def geomean(xs):
    xs = [x for x in xs if x and x > 0]
    return math.exp(sum(math.log(x) for x in xs) / len(xs)) if xs else None


def load_json(path, default=None):
    try:
        with open(path) as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return default


def save_json(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        json.dump(obj, fh, indent=1, sort_keys=True)
    print(f"wrote {os.path.relpath(path, ROOT)}")


try:
    import tiktoken
    _ENC = tiktoken.get_encoding("o200k_base")

    def tokens(s):
        return len(_ENC.encode(s, disallowed_special=()))
except Exception:  # noqa: BLE001
    def tokens(s):
        return int(len(s.encode("utf8", "replace")) / 3.7)


HIT_RE = re.compile(r"^\s*(?:[a-z]+\s+)?(\.?/?[^\s:]+?):(\d+)(?=[:\s])")


def text_hits(text):
    """(path, line) for each output line that names a location, in order; None entries for other lines."""
    out = []
    for line in text.splitlines():
        m = HIT_RE.match(line)
        out.append((m.group(1).removeprefix("./"), int(m.group(2))) if m else None)
    return out


GREEG_FILE_RE = re.compile(r"^([^\s:]+/[^\s:]+|[^\s:]+\.\w+)  ")
GREEG_LINE_RE = re.compile(r"^\s+(\d+) (?:def|import|call|type|member|ident|doc|comment|string)\b")


def greeg_text_hits(text):
    """Like text_hits, for greeg's grouped text output: a file header line, then
    `<line> <kind> <text>` hit lines (context lines carry no kind); the
    `definitions` section uses `kind path:line text`."""
    out, cur = [], None
    for line in text.splitlines():
        m = HIT_RE.match(line)
        if m and not line.startswith(" " * 5):
            out.append((m.group(1).removeprefix("./"), int(m.group(2))))
            continue
        f = GREEG_FILE_RE.match(line)
        if f:
            cur = f.group(1).removeprefix("./")
            out.append(None)
            continue
        h = GREEG_LINE_RE.match(line)
        out.append((cur, int(h.group(1))) if h and cur else None)
    return out


def json_hits(text):
    """(path, line) set from rg/greeg --json match records."""
    s = set()
    for line in text.splitlines():
        if not line.startswith("{"):
            continue
        try:
            j = json.loads(line)
        except ValueError:
            continue
        if j.get("type") == "match":
            d = j["data"]
            s.add((d["path"]["text"].removeprefix("./"), d["line_number"]))
    return s


# ───────────────────────────── fetch ─────────────────────────────

def fetch(names):
    os.makedirs(cache_dir(), exist_ok=True)
    for name in names:
        spec = CORPORA[name]
        dst = corpus_path(name)
        if spec.get("kind") == "file":
            if os.path.exists(dst):
                print(f"{name}: present")
                continue
            print(f"{name}: downloading {spec['url']}")
            gz = dst + ".gz"
            subprocess.run(["curl", "-L", "-o", gz, spec["url"]], check=True)
            subprocess.run(["gunzip", "-f", gz], check=True)
            continue
        sha = spec["sha"]
        if os.path.isdir(os.path.join(dst, ".git")):
            head = out_of(["git", "rev-parse", "HEAD"], dst).strip()
            tag = out_of(["git", "describe", "--tags", "--exact-match"], dst).strip()
            if head.startswith(sha) or tag == sha:
                print(f"{name}: present at {sha}")
                continue
            print(f"{name}: at {head[:7]}, want {sha}; refetching")
        else:
            os.makedirs(dst, exist_ok=True)
            subprocess.run(["git", "init", "-q"], cwd=dst, check=True)
            subprocess.run(["git", "remote", "add", "origin", spec["url"]], cwd=dst, check=True)
        ref = f"refs/tags/{sha}" if sha.startswith("v") else sha
        print(f"{name}: fetching {ref} from {spec['url']}")
        r = subprocess.run(["git", "fetch", "-q", "--depth", "1", "origin", ref], cwd=dst)
        if r.returncode != 0 and ref == sha:
            # servers that refuse fetch-by-SHA: fall back to a full fetch of the default branch
            subprocess.run(["git", "fetch", "-q", "origin"], cwd=dst, check=True)
        subprocess.run(["git", "checkout", "-q", "--detach", "FETCH_HEAD" if r.returncode == 0 else sha], cwd=dst, check=True)
        print(f"{name}: {out_of(['git', 'rev-parse', '--short', 'HEAD'], dst).strip()}")


# ───────────────────────────── speed ─────────────────────────────

FAMILIES = ["ident", "word", "phrase", "regex"]
VERBS = ["def", "refs", "callers"]
FAMILY_FLAGS = {"ident": [], "word": ["-w"], "phrase": ["-F"], "regex": []}


def tool_cmd(tool, greeg, family, pat, corpus_kind):
    fl = FAMILY_FLAGS[family]
    if tool == "grep":
        return ["grep", "-rnI", "--exclude-dir=.git", "--exclude-dir=node_modules", *fl, "-e", pat, "."]
    if tool == "grep-E":
        return ["grep", "-rnIE", "--exclude-dir=.git", "--exclude-dir=node_modules", *fl, "-e", pat, "."]
    if tool == "rg":
        return ["rg", "-n", *fl, "-e", pat, "."]
    if tool == "rg-j4":
        return ["rg", "-j4", "-n", *fl, "-e", pat, "."]
    if tool == "greeg-scan":
        return [greeg, "--no-index", "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", *fl, "-e", pat, "."]
    if tool == "greeg-full":
        return [greeg, "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", *fl, "-e", pat, "."]
    if tool == "greeg":
        return [greeg, "--no-session", *fl, "-e", pat, "."]
    raise KeyError(tool)


TOOLS = ["grep", "rg", "rg-j4", "greeg-scan", "greeg-full", "greeg"]


def hyperfine(cmds, cwd, runs, warmup, prepare=None, timeout=1800):
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tf:
        path = tf.name
    args = ["hyperfine", "-N", "--warmup", str(warmup), "--runs", str(runs), "--ignore-failure", "--export-json", path, "--style", "none"]
    if prepare:
        args += ["--prepare", prepare]
    args += [" ".join(shlex.quote(x) for x in c) for c in cmds]
    r = run(args, cwd, timeout)
    if r.returncode != 0:
        sys.stderr.write(r.stderr.decode("utf8", "replace")[-500:] + "\n")
    data = load_json(path, {"results": []})
    os.unlink(path)
    out = []
    for res in data.get("results", []):
        out.append({"mean": res["mean"], "median": res["median"], "stddev": res.get("stddev") or 0.0, "min": res["min"], "max": res["max"], "user": res["user"], "system": res["system"]})
    while len(out) < len(cmds):
        out.append(None)
    return out


def max_rss_mb(cmd, cwd):
    if MAC:
        r = run(["/usr/bin/time", "-l", *cmd], cwd)
        m = re.search(r"(\d+)\s+maximum resident set size", r.stderr.decode("utf8", "replace"))
        return int(m.group(1)) / 1e6 if m else None
    if shutil.which("/usr/bin/time"):
        r = run(["/usr/bin/time", "-v", *cmd], cwd)
        m = re.search(r"Maximum resident set size \(kbytes\): (\d+)", r.stderr.decode("utf8", "replace"))
        return int(m.group(1)) / 1e3 if m else None
    return None


def corpus_stats(cwd):
    files = out_of(["rg", "--files"], cwd).splitlines()
    total = 0
    for f in files:
        try:
            total += os.path.getsize(os.path.join(cwd, f))
        except OSError:
            pass
    return len(files), total


def doctor_index(greeg, cwd):
    txt = out_of([greeg, "doctor"], cwd)
    m = re.search(r"total ([\d.]+ \w+) \(([\d.]+)× of ([\d.]+ \w+) source\)", txt)
    g = re.search(r"generation (\d+)", txt)
    return {"size": m.group(1) if m else None, "ratio": float(m.group(2)) if m else None, "source": m.group(3) if m else None, "generation": int(g.group(1)) if g else None}


def speed(args):
    greeg = os.path.abspath(args.greeg)
    names = args.corpora.split(",") if args.corpora else [n for n, s in CORPORA.items() if s.get("size") == "small"]
    cold = args.cold and run(["sudo", "-n", "true"]).returncode == 0
    if args.cold and not cold:
        print("cold runs skipped: passwordless sudo is not available (sudo -n)")
    purge = "sudo purge" if MAC else "sync; echo 3 | sudo tee /proc/sys/vm/drop_caches"
    result = {"host": host_info(), "date": datetime.datetime.now().isoformat(timespec="seconds"), "greeg": version_of([greeg, "--version"]), "rg": version_of(["rg", "--version"]), "grep": version_of(["grep", "--version"]), "runs": args.runs, "corpora": {}}
    prev = load_json(os.path.join(RESULTS, f"speed-{host_key()}.json"))
    if prev and args.corpora:
        result["corpora"] = prev.get("corpora", {})  # a subset run refreshes only its corpora
    for name in names:
        spec = CORPORA[name]
        cwd = corpus_path(name)
        if not os.path.isdir(cwd):
            print(f"{name}: not fetched (bench/bench.py fetch {name}); skipped")
            continue
        nfiles, nbytes = corpus_stats(cwd)
        print(f"\n== {name}: {nfiles} files, {nbytes/1e6:.0f} MB")
        entry = {"files": nfiles, "bytes": nbytes, "sha": spec.get("sha"), "index": {}, "queries": {}}
        # index build: fresh dir each run
        if spec.get("kind") != "file":
            idx = tempfile.mkdtemp(prefix="greeg-bench-idx-")
            build = hyperfine([[greeg, "index", "--index-dir", idx, "--root", "."]], cwd, runs=max(2, args.runs // 3), warmup=0, prepare=f"rm -rf {idx}")[0]
            shutil.rmtree(idx, ignore_errors=True)
            rss = max_rss_mb([greeg, "index", "--index-dir", idx, "--root", "."], cwd)
            shutil.rmtree(idx, ignore_errors=True)
            run([greeg, "index", "--root", "."], cwd)  # the default index used by the queries
            info = doctor_index(greeg, cwd)
            info.update({"build_s": build["mean"] if build else None, "build_stddev": build["stddev"] if build else None, "rss_mb": rss})
            entry["index"] = info
            print(f"   index build {fmt_ms(info['build_s'])} ± {fmt_ms(info.get('build_stddev') or 0)}  size {info['size']} ({info['ratio']}× of source)  RSS {rss and round(rss)} MB")
        for fam in FAMILIES:
            pat = spec["queries"].get(fam)
            if not pat:
                continue
            tools = [t for t in TOOLS if not (spec.get("kind") == "file" and t in ("greeg-full", "greeg"))]
            if fam == "regex":
                tools = [("grep-E" if t == "grep" else t) for t in tools]
            cmds = [tool_cmd(t, greeg, fam, pat, spec.get("kind")) for t in tools]
            # match-count verification: rg --json vs greeg --json (path, line) sets; grep by text
            rgj = json_hits(out_of(["rg", "--json", "-n", *FAMILY_FLAGS[fam], "-e", pat, "."], cwd))
            ggj = json_hits(out_of([greeg, "--json", "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", *FAMILY_FLAGS[fam], "-e", pat, "."], cwd)) if spec.get("kind") != "file" else rgj
            grp = {h for h in text_hits(out_of(cmds[0], cwd)) if h}  # grep has no gitignore support: counts may exceed rg's
            verified = {"rg": len(rgj), "greeg": len(ggj), "grep": len(grp), "rg_eq_greeg": rgj == ggj, "grep_eq_rg": grp == rgj}
            states = [("warm", None)] + ([("cold", purge)] if cold else [])
            fam_entry = {"pattern": pat, "matches": verified, "tools": {}}
            for state, prep in states:
                res = hyperfine(cmds, cwd, runs=args.runs if state == "warm" else max(3, args.runs // 3), warmup=3 if state == "warm" else 0, prepare=prep)
                for t, r in zip(tools, res):
                    key = t.replace("grep-E", "grep")
                    d = fam_entry["tools"].setdefault(key, {})
                    d[state] = r
            for t in tools:
                key = t.replace("grep-E", "grep")
                if t in ("rg", "greeg"):
                    fam_entry["tools"][key]["rss_mb"] = max_rss_mb(tool_cmd(t, greeg, fam, pat, spec.get("kind")), cwd)
            entry["queries"][fam] = fam_entry
            line = "  ".join(f"{t}={fmt_ms(fam_entry['tools'][t.replace('grep-E','grep')]['warm']['mean'] if fam_entry['tools'][t.replace('grep-E','grep')]['warm'] else None)}" for t in tools)
            print(f"   {fam:6} {pat!r:34} {line}  matches rg={verified['rg']} greeg={verified['greeg']} grep={verified['grep']}{'' if verified['rg_eq_greeg'] else '  MISMATCH rg/greeg'}")
        if spec.get("kind") != "file":
            for verb in VERBS:
                nm = spec["queries"].get(verb)
                if not nm:
                    continue
                cmd = [greeg, verb, nm, "--no-session"]
                r = hyperfine([cmd], cwd, runs=args.runs, warmup=3)[0]
                entry["queries"][verb] = {"pattern": nm, "tools": {"greeg": {"warm": r}}}
                print(f"   {verb:6} {nm!r:34} greeg={fmt_ms(r['mean'] if r else None)}")
        result["corpora"][name] = entry
    # geometric-mean speedups vs rg per family (warm)
    gm = {}
    for fam in FAMILIES:
        for tool in TOOLS:
            ratios = []
            for e in result["corpora"].values():
                q = e["queries"].get(fam)
                if not q or tool not in q["tools"] or "rg" not in q["tools"]:
                    continue
                a, b = q["tools"]["rg"].get("warm"), q["tools"][tool].get("warm")
                if a and b:
                    ratios.append(a["mean"] / b["mean"])
            if ratios:
                gm.setdefault(fam, {})[tool] = geomean(ratios)
    result["geomean_vs_rg"] = gm
    print("\ngeometric-mean speed vs rg (>1 = faster than rg):")
    for fam, d in gm.items():
        print(f"   {fam:6} " + "  ".join(f"{t}={v:.2f}×" for t, v in d.items()))
    save_json(os.path.join(RESULTS, f"speed-{host_key()}.json"), result)
    save_json(os.path.join(RESULTS, "speed.json"), result)


# ───────────────────────────── SCIP oracle ─────────────────────────────

def pb_varint(b, i):
    x = shift = 0
    while True:
        c = b[i]
        i += 1
        x |= (c & 0x7F) << shift
        if c < 0x80:
            return x, i
        shift += 7


def pb_fields(b):
    """Yield (field_number, value) for one protobuf message; value is int or bytes."""
    i, n = 0, len(b)
    while i < n:
        key, i = pb_varint(b, i)
        f, wt = key >> 3, key & 7
        if wt == 0:
            v, i = pb_varint(b, i)
        elif wt == 2:
            ln, i = pb_varint(b, i)
            v = b[i:i + ln]
            i += ln
        elif wt == 1:
            v = b[i:i + 8]
            i += 8
        elif wt == 5:
            v = b[i:i + 4]
            i += 4
        else:
            raise ValueError(f"wire type {wt}")
        yield f, v


def pb_packed_i32(b):
    out, i = [], 0
    while i < len(b):
        v, i = pb_varint(b, i)
        out.append(v - (1 << 32) if v >= 1 << 31 else v)
    return out


DESC_RE = re.compile(r"(?:`((?:[^`]|``)+)`|([^`/#.:!()\[\]\s]+))(?:\(([^)]*)\))?([./#:!])$")


def scip_name(symbol):
    """(name, kind) from a SCIP symbol string, or None for locals/unnamed."""
    if symbol.startswith("local "):
        return None
    m = DESC_RE.search(symbol)
    if not m:
        if symbol.endswith("]"):
            return None  # type parameter
        return None
    name = m.group(1).replace("``", "`") if m.group(1) else m.group(2)
    suffix = m.group(4)
    if m.group(3) is not None and suffix == ".":
        kind = "method"
    else:
        kind = {"#": "type", ".": "term", "/": "namespace", ":": "meta", "!": "macro"}[suffix]
    return name, kind


def read_scip(path):
    """{name: {"kind": kind, "defs": {(path, line)}, "refs": {(path, line)}}} plus per-line occurrence map."""
    with open(path, "rb") as fh:
        data = fh.read()
    names = {}
    lines = {}  # (path, line) -> set of (name, is_def)
    docs = set()
    for f, v in pb_fields(data):
        if f != 2:
            continue
        rel, occs = None, []
        for df, dv in pb_fields(v):
            if df == 1:
                rel = dv.decode("utf8", "replace")
            elif df == 2:
                occs.append(dv)
        if rel is None:
            continue
        rel = rel.removeprefix("./")
        docs.add(rel)
        for o in occs:
            rng, sym, roles = None, None, 0
            for of, ov in pb_fields(o):
                if of == 1:
                    rng = pb_packed_i32(ov) if isinstance(ov, bytes) else [ov]
                elif of == 2:
                    sym = ov.decode("utf8", "replace")
                elif of == 3:
                    roles = ov
            if not rng or sym is None:
                continue
            nk = scip_name(sym)
            if not nk:
                continue
            name, kind = nk
            is_def = bool(roles & 1)
            loc = (rel, rng[0] + 1)
            e = names.setdefault(name, {"kind": kind, "defs": set(), "refs": set(), "symbols": set()})
            e["symbols"].add(sym)
            (e["defs"] if is_def else e["refs"]).add(loc)
            lines.setdefault(loc, set()).add((name, is_def))
    return names, lines, docs


def scip_index_for(name, cwd, scip_dir):
    """Run the language's SCIP indexer if index.scip is missing; return its path or None."""
    out = os.path.join(scip_dir, name, "index.scip")
    if os.path.exists(out):
        return out
    os.makedirs(os.path.dirname(out), exist_ok=True)
    lang = CORPORA[name]["lang"]
    if lang == "rust" and shutil.which("rust-analyzer"):
        cmd = ["rust-analyzer", "scip", ".", "--output", out]
    elif lang == "python" and shutil.which("npx"):
        cmd = ["npx", "-y", "@sourcegraph/scip-python", "index", "--project-name", name, "--output", out]
    elif lang in ("typescript", "javascript") and shutil.which("npx"):
        cmd = ["npx", "-y", "@sourcegraph/scip-typescript", "index", "--no-progress-bar", "--infer-tsconfig", "--output", out]
    elif lang == "kotlin" and shutil.which("scip-java"):
        cmd = ["scip-java", "index", "--output", out]
    else:
        print(f"{name}: no SCIP indexer for {lang} on this machine; skipped")
        return None
    print(f"{name}: running {' '.join(cmd)}")
    r = subprocess.run(cmd, cwd=cwd, stdin=DEVNULL, stdout=DEVNULL, stderr=subprocess.PIPE)
    if r.returncode != 0 or not os.path.exists(out):
        print(f"{name}: indexer failed: {r.stderr.decode('utf8', 'replace')[-400:]}")
        return None
    return out


def sample_names(names, per_bucket, seed):
    rnd = random.Random(seed)
    buckets = {"1": [], "2-5": [], "6+": []}
    for n, e in names.items():
        # `meta` descriptors are locals and object-literal properties, `namespace`
        # ones are files and modules whose SCIP definition is line 1 of the file:
        # neither is a definition an agent asks `def NAME` about.
        if len(n) < 3 or not e["defs"] or not re.match(r"^[A-Za-z_]\w*$", n) or e["kind"] in ("meta", "namespace"):
            continue
        d = len(e["defs"])
        buckets["1" if d == 1 else "2-5" if d <= 5 else "6+"].append(n)
    out = []
    for b, lst in buckets.items():
        lst.sort()
        rnd.shuffle(lst)
        # keep the kind mix: types, methods/functions, terms in rotation
        by_kind = {}
        for n in lst:
            by_kind.setdefault(names[n]["kind"], []).append(n)
        picked = []
        while len(picked) < per_bucket and any(by_kind.values()):
            for k in list(by_kind):
                if by_kind[k] and len(picked) < per_bucket:
                    picked.append(by_kind[k].pop())
        out += [(n, b) for n in picked]
    return out


def acc_at(shown, truth, k):
    return any(h in truth for h in shown[:k])


def oracle(args):
    greeg = os.path.abspath(args.greeg)
    scip_dir = args.scip or os.path.join(os.path.dirname(cache_dir()), "scip")
    all_results = load_json(os.path.join(RESULTS, "oracle.json"), {}) or {}
    for name in args.corpora:
        cwd = corpus_path(name)
        if not os.path.isdir(cwd):
            print(f"{name}: not fetched; skipped")
            continue
        idx = scip_index_for(name, cwd, scip_dir)
        if not idx:
            continue
        t0 = time.time()
        names, lines, docs = read_scip(idx)
        print(f"\n== {name}: SCIP {os.path.getsize(idx)/1e6:.0f} MB, {len(docs)} documents, {len(names)} names with occurrences, {sum(1 for e in names.values() if e['defs'])} defined in-repo ({time.time()-t0:.1f} s to decode)")
        run([greeg, "index", "--root", "."], cwd)
        samples = sample_names(names, args.per_bucket, args.seed)
        rows = []
        for i, (nm, bucket) in enumerate(samples):
            e = names[nm]
            truth_def, truth_ref = e["defs"], e["refs"]
            truth_all = truth_def | truth_ref
            row = {"name": nm, "bucket": bucket, "kind": e["kind"], "defs": len(truth_def), "refs": len(truth_ref)}
            # definitions: greeg def (JSON), rg -w and grep -w first lines
            gd = [(j["path"], j["line"]) for j in map(json.loads, (l for l in out_of([greeg, "def", nm, "--json", "--no-session"], cwd).splitlines() if l.startswith("{"))) if j.get("type") == "def"]
            rgd = [h for h in text_hits(out_of(["rg", "-n", "-w", "-e", nm, "."], cwd)) if h]
            grd = [h for h in text_hits(out_of(["grep", "-rnwI", "--exclude-dir=.git", "--exclude-dir=node_modules", "-e", nm, "."], cwd)) if h]
            for tool, shown in (("greeg", gd), ("rg", rgd), ("grep", grd)):
                row[f"{tool}_acc"] = [acc_at(shown, truth_def, k) for k in (1, 5, 10)]
            row["greeg_def_shown"] = len(gd)
            # references: greeg refs (JSON, unbudgeted) recall; classification vs roles
            refs_out = out_of([greeg, "refs", nm, "--json", "--budget", "0", "--no-session"], cwd)
            found = set()
            for l in refs_out.splitlines():
                if not l.startswith("{"):
                    continue
                j = json.loads(l)
                if j.get("type") == "ref":
                    found.add((j["data"]["path"], j["data"]["line"]))
                elif j.get("type") == "def":
                    found.add((j["path"], j["line"]))
            row["ref_recall"] = (len(truth_ref & found) / len(truth_ref)) if truth_ref else None
            row["def_recall"] = (len(truth_def & found) / len(truth_def)) if truth_def else None
            for precise in (False, True):
                cmd = [greeg, "--json", "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", "-w", "-e", nm, "."] + (["--precise"] if precise else [])
                hits = []
                for l in out_of(cmd, cwd).splitlines():
                    if l.startswith("{"):
                        j = json.loads(l)
                        if j.get("type") == "match":
                            hits.append(((j["data"]["path"]["text"].removeprefix("./"), j["data"]["line_number"]), j["data"]["kind"], (j["data"].get("symbol") or {}).get("kind")))
                tp = fp = fn = fp_impl = 0  # def classification
                code_hit_on_scip = code_hits = noncode_on_scip = noncode_hits = 0
                for loc, kind, symkind in hits:
                    if loc[0] not in docs:
                        continue  # only files SCIP covers say anything about roles
                    occ = lines.get(loc, set())
                    scip_def = (nm, True) in occ
                    scip_any = any(n == nm for n, _ in occ)
                    if kind == "def":
                        tp += scip_def
                        if not scip_def and symkind == "impl":
                            fp_impl += 1  # greeg counts `impl X` headers as definitions; SCIP marks them references
                        else:
                            fp += not scip_def
                    elif scip_def:
                        fn += 1
                    if kind in ("doc", "comment", "string"):
                        noncode_hits += 1
                        noncode_on_scip += scip_any
                    else:
                        code_hits += 1
                        code_hit_on_scip += scip_any
                key = "precise" if precise else "spans"
                row[f"cls_{key}"] = {"def_tp": tp, "def_fp": fp, "def_fp_impl": fp_impl, "def_fn": fn, "code_hits": code_hits, "code_on_scip": code_hit_on_scip, "noncode_hits": noncode_hits, "noncode_on_scip": noncode_on_scip}
            # context efficiency at a 500-line budget: greeg default answer vs rg vs grep
            for tool, cmd in (("greeg", [greeg, nm, "--no-session"]), ("greeg6k", [greeg, nm, "--no-session", "--budget", "6000"]), ("rg", ["rg", "-n", "-w", "-e", nm, "."]), ("grep", ["grep", "-rnwI", "--exclude-dir=.git", "--exclude-dir=node_modules", "-e", nm, "."])):
                text = out_of(cmd, cwd)
                emitted = text.splitlines()[:500]
                hs = (greeg_text_hits if tool.startswith("greeg") else text_hits)("\n".join(emitted))
                useful = [h for h in hs if h and h in truth_all]
                first_def_tokens = None
                for k, h in enumerate(hs):
                    if h and h in truth_def:
                        first_def_tokens = tokens("\n".join(emitted[:k + 1]))
                        break
                tk = tokens("\n".join(emitted))
                row[f"ctx_{tool}"] = {"lines": len(emitted), "useful": len(useful), "coverage": len(set(useful)) / max(1, len(truth_all)), "tokens": tk, "useful_per_ktok": len(set(useful)) / max(1, tk) * 1000, "tokens_to_def": first_def_tokens}
            rows.append(row)
            if (i + 1) % 15 == 0:
                print(f"   … {i+1}/{len(samples)}")
        summary = summarize_oracle(rows)
        misses = [{"name": r["name"], "kind": r["kind"], "bucket": r["bucket"], "scip_defs": sorted(names[r["name"]]["defs"])[:3], "greeg_shown": r["greeg_def_shown"]} for r in rows if not r["greeg_acc"][2]]
        all_results[name] = {"date": datetime.datetime.now().isoformat(timespec="seconds"), "scip": os.path.basename(os.path.dirname(idx)), "names": len(names), "sampled": len(rows), "summary": summary, "misses": misses, "rows": rows}
        print_oracle(name, summary)
        for m in misses:
            print(f"   miss@10 {m['name']} ({m['kind']}, ambiguity {m['bucket']}): SCIP {', '.join(f'{p}:{l}' for p, l in m['scip_defs'])}; greeg def showed {m['greeg_shown']}")
        save_json(os.path.join(RESULTS, "oracle.json"), all_results)


def summarize_oracle(rows):
    s = {}

    def mean(xs):
        xs = [x for x in xs if x is not None]
        return (sum(xs) / len(xs)) if xs else None

    for tool in ("greeg", "rg", "grep"):
        s[f"{tool}_acc"] = [mean([r[f"{tool}_acc"][i] for r in rows]) for i in range(3)]
    for tool in ("greeg", "greeg6k", "rg", "grep"):
        s[f"{tool}_ctx_useful_per_ktok"] = mean([r[f"ctx_{tool}"]["useful_per_ktok"] for r in rows])
        s[f"{tool}_ctx_useful_ratio"] = mean([r[f"ctx_{tool}"]["useful"] / r[f"ctx_{tool}"]["lines"] for r in rows if r[f"ctx_{tool}"]["lines"]])
        s[f"{tool}_ctx_coverage"] = mean([r[f"ctx_{tool}"]["coverage"] for r in rows])
        s[f"{tool}_tokens"] = mean([r[f"ctx_{tool}"]["tokens"] for r in rows])
        tdef = [r[f"ctx_{tool}"]["tokens_to_def"] for r in rows]
        s[f"{tool}_tokens_to_def"] = statistics.median([t for t in tdef if t is not None]) if any(t is not None for t in tdef) else None
        s[f"{tool}_def_reached"] = mean([t is not None for t in tdef])
    for b in ("1", "2-5", "6+"):
        br = [r for r in rows if r["bucket"] == b]
        if br:
            s[f"greeg_acc_bucket_{b}"] = [mean([r["greeg_acc"][i] for r in br]) for i in range(3)]
            s[f"n_bucket_{b}"] = len(br)
    s["ref_recall"] = mean([r["ref_recall"] for r in rows])
    s["def_recall"] = mean([r["def_recall"] for r in rows])
    for key in ("spans", "precise"):
        tp = sum(r[f"cls_{key}"]["def_tp"] for r in rows)
        fp = sum(r[f"cls_{key}"]["def_fp"] for r in rows)
        fpi = sum(r[f"cls_{key}"]["def_fp_impl"] for r in rows)
        fn = sum(r[f"cls_{key}"]["def_fn"] for r in rows)
        code = sum(r[f"cls_{key}"]["code_hits"] for r in rows)
        code_on = sum(r[f"cls_{key}"]["code_on_scip"] for r in rows)
        nc = sum(r[f"cls_{key}"]["noncode_hits"] for r in rows)
        nc_on = sum(r[f"cls_{key}"]["noncode_on_scip"] for r in rows)
        s[f"cls_{key}"] = {"def_precision": tp / (tp + fp) if tp + fp else None, "def_precision_with_impl": tp / (tp + fp + fpi) if tp + fp + fpi else None, "impl_headers": fpi, "def_recall": tp / (tp + fn) if tp + fn else None, "code_on_scip": code_on / code if code else None, "noncode_precision": (nc - nc_on) / nc if nc else None, "noncode_hits": nc, "code_hits": code}
    return s


def pct(x):
    return "–" if x is None else f"{100*x:.0f} %"


def print_oracle(name, s):
    print(f"\n{name}: definitions Acc@1/5/10  greeg {'/'.join(pct(x) for x in s['greeg_acc'])}   rg {'/'.join(pct(x) for x in s['rg_acc'])}   grep {'/'.join(pct(x) for x in s['grep_acc'])}")
    for b in ("1", "2-5", "6+"):
        if f"greeg_acc_bucket_{b}" in s:
            print(f"   ambiguity {b:4} (n={s[f'n_bucket_{b}']:2}): greeg Acc@1/5/10 {'/'.join(pct(x) for x in s[f'greeg_acc_bucket_{b}'])}")
    print(f"   reference recall (greeg refs vs SCIP) {pct(s['ref_recall'])}; definition recall {pct(s['def_recall'])}")
    for key in ("spans", "precise"):
        c = s[f"cls_{key}"]
        print(f"   classification [{key}]: def precision {pct(c['def_precision'])} ({pct(c['def_precision_with_impl'])} counting {c['impl_headers']} impl headers as errors) recall {pct(c['def_recall'])}; code hits on a SCIP occurrence {pct(c['code_on_scip'])}; noncode precision {pct(c['noncode_precision'])} ({c['noncode_hits']} noncode / {c['code_hits']} code hits)")
    for tool in ("greeg", "greeg6k", "rg", "grep"):
        print(f"   context@500 {tool:7}: useful lines {pct(s[f'{tool}_ctx_useful_ratio'])}  coverage {pct(s[f'{tool}_ctx_coverage'])}  tokens {s[f'{tool}_tokens'] and round(s[f'{tool}_tokens'])}  useful/ktok {s[f'{tool}_ctx_useful_per_ktok']:.1f}  tokens-to-first-def median {s[f'{tool}_tokens_to_def'] and round(s[f'{tool}_tokens_to_def'])} (reached {pct(s[f'{tool}_def_reached'])})")


# ───────────────────────────── gate ─────────────────────────────

def gate(args):
    key = host_key()
    tol = args.tolerance / 100.0
    if args.what == "speed":
        cur = load_json(os.path.join(RESULTS, f"speed-{key}.json"))
        base_path = os.path.join(BASELINES, f"speed-{key}.json")
        base = load_json(base_path)
        if not cur:
            sys.exit("no current speed results for this host; run bench/bench.py speed first")
        if args.save or not base:
            save_json(base_path, cur)
            print(f"baseline {'saved' if args.save else 'created'} for {key}; nothing to compare")
            return
        worst = []
        for c, e in cur["corpora"].items():
            be = base["corpora"].get(c)
            if not be:
                continue
            for fam, q in e["queries"].items():
                bq = be["queries"].get(fam)
                if not bq:
                    continue
                for tool in ("greeg", "greeg-full", "greeg-scan"):
                    a = q["tools"].get(tool, {}).get("warm")
                    b = bq["tools"].get(tool, {}).get("warm")
                    if a and b and b["mean"] > 0:
                        d = a["mean"] / b["mean"] - 1
                        worst.append((d, f"{c} {fam} {tool}: {fmt_ms(b['mean'])} → {fmt_ms(a['mean'])} ({d:+.0%})"))
            if e["index"].get("build_s") and be["index"].get("build_s"):
                d = e["index"]["build_s"] / be["index"]["build_s"] - 1
                worst.append((d, f"{c} index build: {fmt_ms(be['index']['build_s'])} → {fmt_ms(e['index']['build_s'])} ({d:+.0%})"))
        worst.sort(reverse=True)
        for d, line in worst[:8]:
            print(("SLOWER  " if d > tol else "ok      ") + line)
        bad = [w for w in worst if w[0] > tol]
        print(f"speed gate ({args.tolerance:.0f} %): {'FAIL' if bad else 'PASS'} ({len(worst)} comparisons, {len(bad)} regressions)")
        sys.exit(1 if bad else 0)
    # kernels: criterion estimates under target/criterion/<group>/<bench>/new/estimates.json
    crit = os.path.join(ROOT, "target", "criterion")
    cur = {}
    for dirpath, _, files in os.walk(crit):
        if "estimates.json" in files and os.path.basename(dirpath) == "new":
            est = load_json(os.path.join(dirpath, "estimates.json"))
            rel = os.path.relpath(os.path.dirname(dirpath), crit)
            if est and "mean" in est:
                cur[rel] = est["mean"]["point_estimate"]
    if not cur:
        sys.exit("no criterion results under target/criterion; run `cargo bench -p greeg-index --bench kernels` first")
    base_path = os.path.join(BASELINES, f"kernels-{key}.json")
    base = load_json(base_path)
    if args.save or not base:
        save_json(base_path, {"host": host_info(), "ns": cur})
        print(f"kernel baseline {'saved' if args.save else 'created'} for {key}; nothing to compare")
        return
    bad = []
    for k, v in sorted(cur.items()):
        b = base["ns"].get(k)
        if not b:
            print(f"new     {k}: {v/1e3:.1f} µs")
            continue
        d = v / b - 1
        print(f"{'SLOWER ' if d > tol else 'ok     '} {k}: {b/1e3:.1f} → {v/1e3:.1f} µs ({d:+.1%})")
        if d > tol:
            bad.append(k)
    print(f"kernel gate ({args.tolerance:.0f} %): {'FAIL' if bad else 'PASS'}")
    sys.exit(1 if bad else 0)


# ───────────────────────────── report ─────────────────────────────

def report(args):
    speed_res = load_json(os.path.join(RESULTS, "speed.json"))
    oracle_res = load_json(os.path.join(RESULTS, "oracle.json"), {})
    out = ["# Benchmarks", "", "Generated by `bench/bench.py report` from `bench/results/`. Protocol: PLAN.md M6, DESIGN.md §13.", ""]
    if speed_res:
        h = speed_res["host"]
        out += [f"## Speed", "", f"Host `{h['key']}` ({h['cpus']} CPUs), {speed_res['date']}. `{speed_res['greeg']}`, `{speed_res['rg']}`, `{speed_res['grep'][:40]}`. hyperfine `-N --warmup 3 --runs {speed_res['runs']}`, warm page cache, means. All tools print `path:line:text` for every match; `greeg` (default) is the budgeted answer an agent reads, `greeg-full` the unbudgeted indexed search, `greeg-scan` the same without an index. Match sets verified equal between rg and greeg for every row unless marked.", ""]
        out += ["### Index build", "", "| corpus | files | source | build | index | ratio | build RSS |", "|---|---:|---:|---:|---:|---:|---:|"]
        for c, e in speed_res["corpora"].items():
            ix = e["index"]
            if ix.get("build_s"):
                out.append(f"| {c} | {e['files']:,} | {e['bytes']/1e6:.0f} MB | {fmt_ms(ix['build_s'])} | {ix.get('size') or '–'} | {ix.get('ratio') or '–'}× | {ix.get('rss_mb') and round(ix['rss_mb'])} MB |")
        out += ["", "### Search latency (warm)", ""]
        tools = ["grep", "rg", "rg-j4", "greeg-scan", "greeg-full", "greeg"]
        out += ["| corpus | family | pattern | " + " | ".join(tools) + " | matches |", "|---|---|---|" + "---:|" * len(tools) + "---:|"]
        for c, e in speed_res["corpora"].items():
            for fam in FAMILIES:
                q = e["queries"].get(fam)
                if not q:
                    continue
                cells = []
                for t in tools:
                    r = q["tools"].get(t, {}).get("warm")
                    cells.append(fmt_ms(r["mean"]) if r else "–")
                m = q["matches"]
                out.append(f"| {c} | {fam} | `{q['pattern']}` | " + " | ".join(cells) + f" | {m['rg']:,}{'' if m['rg_eq_greeg'] else ' ⚠'} |")
        cold_rows = [(c, fam, q) for c, e in speed_res["corpora"].items() for fam, q in e["queries"].items() if fam in FAMILIES and any("cold" in q["tools"].get(t, {}) for t in tools)]
        if cold_rows:
            out += ["", "### Search latency (cold page cache)", "", "| corpus | family | " + " | ".join(tools) + " |", "|---|---|" + "---:|" * len(tools)]
            for c, fam, q in cold_rows:
                out.append(f"| {c} | {fam} | " + " | ".join(fmt_ms(q["tools"][t]["cold"]["mean"]) if q["tools"].get(t, {}).get("cold") else "–" for t in tools) + " |")
        out += ["", "### Verbs", "", "| corpus | verb | name | greeg |", "|---|---|---|---:|"]
        for c, e in speed_res["corpora"].items():
            for v in VERBS:
                q = e["queries"].get(v)
                if q and q["tools"]["greeg"]["warm"]:
                    out.append(f"| {c} | {v} | `{q['pattern']}` | {fmt_ms(q['tools']['greeg']['warm']['mean'])} |")
        out += ["", "### Geometric-mean speed relative to rg (> 1 = faster)", "", "| family | " + " | ".join(tools) + " |", "|---|" + "---:|" * len(tools)]
        for fam, d in speed_res.get("geomean_vs_rg", {}).items():
            out.append(f"| {fam} | " + " | ".join(f"{d[t]:.2f}×" if t in d else "–" for t in tools) + " |")
        out += ["", "### Peak RSS", "", "| corpus | rg (ident) | greeg (ident) |", "|---|---:|---:|"]
        for c, e in speed_res["corpora"].items():
            q = e["queries"].get("ident")
            if q:
                out.append(f"| {c} | {q['tools'].get('rg', {}).get('rss_mb') and round(q['tools']['rg']['rss_mb'])} MB | {q['tools'].get('greeg', {}).get('rss_mb') and round(q['tools']['greeg']['rss_mb'])} MB |")
        out.append("")
    if oracle_res:
        out += ["## Quality (SCIP oracle)", "", "Ground truth: SCIP occurrences from `rust-analyzer scip`, `scip-python`, `scip-typescript`. Names are sampled from symbols defined in the repository, stratified by ambiguity (number of definitions with that name) and kind. Acc@k: a true definition is among the first k locations the tool prints for the bare name (`greeg def NAME`, `rg -nw NAME`, `grep -rnw NAME`). Reference recall: SCIP reference occurrences found by `greeg refs NAME --budget 0`. Classification: `greeg -w NAME --json` hit kinds vs SCIP roles on the same line, from stored spans and with `--precise`. Context@500: the first 500 lines of each tool's output for the bare name; useful = lines that carry a SCIP occurrence of the name; tokens are o200k_base counts.", ""]
        out += ["| corpus | n | greeg Acc@1/5/10 | rg Acc@1/5/10 | grep Acc@1/5/10 | ref recall | def precision (spans / precise) | noncode precision |", "|---|---:|---|---|---|---:|---|---:|"]
        for c, r in oracle_res.items():
            s = r["summary"]
            out.append(f"| {c} | {r['sampled']} | {'/'.join(pct(x) for x in s['greeg_acc'])} | {'/'.join(pct(x) for x in s['rg_acc'])} | {'/'.join(pct(x) for x in s['grep_acc'])} | {pct(s['ref_recall'])} | {pct(s['cls_spans']['def_precision'])} / {pct(s['cls_precise']['def_precision'])} | {pct(s['cls_spans']['noncode_precision'])} |")
        out += ["", "| corpus | ambiguity 1 | ambiguity 2–5 | ambiguity 6+ |", "|---|---|---|---|"]
        for c, r in oracle_res.items():
            s = r["summary"]
            out.append(f"| {c} | " + " | ".join(f"{'/'.join(pct(x) for x in s[f'greeg_acc_bucket_{b}'])} (n={s[f'n_bucket_{b}']})" if f"greeg_acc_bucket_{b}" in s else "–" for b in ("1", "2-5", "6+")) + " |")
        out += ["", "| corpus | tool | useful lines | coverage | tokens/query | useful locations per 1k tokens | tokens to first def (median) | def reached |", "|---|---|---:|---:|---:|---:|---:|---:|"]
        for c, r in oracle_res.items():
            s = r["summary"]
            for t in ("greeg", "greeg6k", "rg", "grep"):
                out.append(f"| {c} | {t} | {pct(s[f'{t}_ctx_useful_ratio'])} | {pct(s[f'{t}_ctx_coverage'])} | {round(s[f'{t}_tokens'] or 0):,} | {s[f'{t}_ctx_useful_per_ktok']:.1f} | {round(s[f'{t}_tokens_to_def']) if s[f'{t}_tokens_to_def'] else '–'} | {pct(s[f'{t}_def_reached'])} |")
        out.append("")
    path = args.out or os.path.join(ROOT, "docs", "BENCH.md")
    with open(path, "w") as fh:
        fh.write("\n".join(out))
    print(f"wrote {os.path.relpath(path, ROOT)}")


# ───────────────────────────── main ─────────────────────────────

def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fetch")
    f.add_argument("names", nargs="*")
    f.add_argument("--all", action="store_true")
    s = sub.add_parser("speed")
    s.add_argument("--greeg", default=os.path.join(ROOT, "target", "release", "greeg"))
    s.add_argument("--corpora", help="comma-separated; default: the small tier")
    s.add_argument("--runs", type=int, default=10)
    s.add_argument("--cold", action="store_true", help="also measure with a purged page cache (needs passwordless sudo)")
    o = sub.add_parser("oracle")
    o.add_argument("corpora", nargs="+")
    o.add_argument("--greeg", default=os.path.join(ROOT, "target", "release", "greeg"))
    o.add_argument("--scip", help="directory holding <corpus>/index.scip (default: sibling `scip` of the corpus cache)")
    o.add_argument("--per-bucket", type=int, default=25)
    o.add_argument("--seed", type=int, default=42)
    g = sub.add_parser("gate")
    g.add_argument("what", choices=["speed", "kernels"])
    g.add_argument("--tolerance", type=float, default=10.0, help="percent slower than the baseline that fails")
    g.add_argument("--save", action="store_true", help="overwrite the baseline for this host")
    r = sub.add_parser("report")
    r.add_argument("--out")
    a = p.parse_args()
    if a.cmd == "fetch":
        names = a.names or ([n for n in CORPORA] if a.all else [n for n, s in CORPORA.items() if s.get("size") == "small"])
        fetch(names)
    elif a.cmd == "speed":
        speed(a)
    elif a.cmd == "oracle":
        oracle(a)
    elif a.cmd == "gate":
        gate(a)
    else:
        report(a)


if __name__ == "__main__":
    main()
