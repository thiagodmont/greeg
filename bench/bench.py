#!/usr/bin/env python3
"""greeg benchmark suite.

    bench/bench.py fetch  [NAME...]                    shallow-clone pinned corpora into the cache
    bench/bench.py speed  [--corpora a,b] [--cold]     hyperfine protocol vs grep / rg, index build, RSS
    bench/bench.py oracle CORPUS...                    SCIP-based quality protocol (definitions, refs, context)
    bench/bench.py gate   speed|kernels [--tolerance] [--require-baseline | --record-only | --save]
                                                       compare with the saved baseline for this host
    bench/bench.py report                              render the benchmark report from bench/results

Corpora and per-corpus queries live in bench/corpora.toml. Results are JSON
under bench/results/ (one file per protocol and host), and `report` renders
them. External tools: git, hyperfine, rg, grep; for the oracle rust-analyzer
(`rust-analyzer scip`), `npx @sourcegraph/scip-typescript`,
`npx @sourcegraph/scip-python`, and scip-java for Kotlin when installed.
"""
import argparse, datetime, json, math, os, platform, random, re, shlex, shutil, statistics, subprocess, sys, tempfile, time, tomllib

from reporting import latency_summary, paired_table, report_heading, result_link
from report_catalog import ReportDataError, ReportDatasets

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
    env = {**(os.environ if env is None else env), "GREEG_STATS": "0"}  # bench runs are not usage: keep them out of `greeg stats`
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
    TOKENIZER = "o200k_base"

    def tokens(s):
        return len(_ENC.encode(s, disallowed_special=()))
except Exception:  # noqa: BLE001
    TOKENIZER = "bytes/3.7 fallback"  # tiktoken missing: recorded in every oracle result

    def tokens(s):
        return int(len(s.encode("utf8", "replace")) / 3.7)


HIT_RE = re.compile(r"^(\.?/?[^\s:][^:]*?):(\d+)(?=[:\s])")  # path may contain spaces; hit lines in grouped layouts start with whitespace


def text_hits(text):
    """(path, line) for each output line that names a location, in order; None entries for other lines."""
    out = []
    for line in text.splitlines():
        m = HIT_RE.match(line)
        out.append((m.group(1).removeprefix("./"), int(m.group(2))) if m else None)
    return out


GREEG_FILE_RE = re.compile(r"^([^\s]+(?:/[^\s]+|\.\w+))(?:  \[[\w,]+\])?$")  # `path` or `path  [test]` (output contract v2)
GREEG_LINE_RE = re.compile(r"^\s+(\d+)\s")  # `<line> [kind] <text>`: hits (kind omitted for ident and in the definitions block) and context lines


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
            # servers that refuse fetch-by-SHA: fall back to a full fetch of the default branch,
            # deepening an earlier depth-1 clone or the pinned commit stays unreachable
            deep = ["--unshallow"] if os.path.exists(os.path.join(dst, ".git", "shallow")) else []
            subprocess.run(["git", "fetch", "-q", *deep, "origin"], cwd=dst, check=True)
        subprocess.run(["git", "checkout", "-q", "--detach", "FETCH_HEAD" if r.returncode == 0 else sha], cwd=dst, check=True)
        print(f"{name}: {out_of(['git', 'rev-parse', '--short', 'HEAD'], dst).strip()}")



# ───────────────────────────── speed ─────────────────────────────

# Protocol 2 (2026-09-02): every timing run is preceded by `sleep 0.15` so the
# greeg rows pay the index freshness check (the 100 ms TTL no longer hides it),
# the freshness cost is measured separately, the table reports medians with
# min–max, and speedups are headlined against `rg -j4` (the best-configured rg on
# macOS, where rg's default thread count spends most of its time in the kernel).
PROTOCOL = 2
PREPARE_SLEEP = "sleep 0.15"
PURGE_CMD = "sudo -n purge" if MAC else "sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'"

FAMILIES = ["ident", "word", "phrase", "regex"]
VERBS = ["def", "refs", "callers"]
FAMILY_FLAGS = {"ident": [], "word": ["-w"], "phrase": ["-F"], "regex": []}
# Printed shape of each report column.
TOOL_SHAPE = {
    "grep": "`path:line:text` for every match (no .gitignore support: its match set can exceed rg's)",
    "rg": "`path:line:text` for every match, default thread count",
    "rg-j4": "`path:line:text` for every match, four threads",
    "greeg-scan": "every match, grouped by file with a kind tag, no index (`--no-index --budget 0`)",
    "greeg-full": "every match, grouped by file with a kind tag, from the index (`--budget 0`)",
    "greeg": "the budgeted digest an agent reads (facets, definitions, top hits, ~2k tokens; `--budget 2000`, the default)",
}
HUGE_FILE = "1000000000000"  # --max-filesize for the single-file corpus


def tool_cmd(tool, greeg, family, pat, corpus_kind, target="."):
    fl = FAMILY_FLAGS[family]
    huge = ["--max-filesize", HUGE_FILE] if corpus_kind == "file" else []
    if tool == "grep":
        return ["grep", "-rnI", "--exclude-dir=.git", "--exclude-dir=node_modules", *fl, "-e", pat, target]
    if tool == "grep-E":
        return ["grep", "-rnIE", "--exclude-dir=.git", "--exclude-dir=node_modules", *fl, "-e", pat, target]
    if tool == "rg":
        return ["rg", "-n", *fl, "-e", pat, target]
    if tool == "rg-j4":
        return ["rg", "-j4", "-n", *fl, "-e", pat, target]
    if tool == "greeg-scan":
        return [greeg, "--no-index", "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", *huge, *fl, "-e", pat, target]
    if tool == "greeg-full":
        return [greeg, "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", *fl, "-e", pat, target]
    if tool == "greeg":
        return [greeg, "--no-session", *fl, "-e", pat, target]
    if tool == "greeg-nofresh":
        return [greeg, "--no-session", "--fresh", "none", *fl, "-e", pat, target]
    raise KeyError(tool)


TOOLS = ["grep", "rg", "rg-j4", "greeg-scan", "greeg-full", "greeg"]
FILE_TOOLS = ["grep", "rg", "rg-j4", "greeg-scan"]  # a single file is scanned, never indexed
RSS_TOOLS = ["rg", "greeg-scan", "greeg-full", "greeg"]


def hyperfine(cmds, cwd, runs, warmup, prepare=None, timeout=1800):
    """hyperfine -N (no shell) per command; `prepare` runs before every timing run (also without a
    shell, so it must be a plain executable invocation such as `sleep 0.15` or `sudo -n purge`)."""
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
        out.append({"mean": res["mean"], "median": res["median"], "stddev": res.get("stddev") or 0.0, "min": res["min"], "max": res["max"], "user": res["user"], "system": res["system"], "n": len(res.get("times") or [])})
    while len(out) < len(cmds):
        out.append(None)
    return out


_RSS_WARNED = [False]


def max_rss_mb(cmd, cwd):
    if MAC:
        r = run(["/usr/bin/time", "-l", *cmd], cwd)
        m = re.search(r"(\d+)\s+maximum resident set size", r.stderr.decode("utf8", "replace"))
        return int(m.group(1)) / 1e6 if m else None
    if os.path.exists("/usr/bin/time"):
        r = run(["/usr/bin/time", "-v", *cmd], cwd)
        m = re.search(r"Maximum resident set size \(kbytes\): (\d+)", r.stderr.decode("utf8", "replace"))
        return int(m.group(1)) / 1e3 if m else None
    if not _RSS_WARNED[0]:
        print("RSS skipped: /usr/bin/time is not installed (apt-get install time)")
        _RSS_WARNED[0] = True
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


def manifest_rss_mb(greeg, cwd):
    """Peak RSS of the build, as the build itself recorded it in the manifest
    (`getrusage` high-water mark). Costs nothing: no extra build run."""
    try:
        m = json.loads(out_of([greeg, "index", "--status"], cwd).split("dir: ")[0])
    except Exception:
        return None
    return (m.get("peak_rss") or 0) / 1e6 or None


FRESH_RE = re.compile(r"^greeg: fresh (\w+) ([\d.]+) ms", re.M)


def fresh_cost(greeg, cmd_tail, cwd, samples):
    """Freshness-check cost of the index as greeg itself reports it (`--stats`: `greeg: fresh <mode> <ms> ms`),
    sampled `samples` times with the protocol's 0.15 s gap so every sample pays the check."""
    modes, ms = {}, []
    for _ in range(samples):
        time.sleep(0.15)
        r = run([greeg, "--no-session", "--stats", *cmd_tail], cwd)
        m = FRESH_RE.search(r.stderr.decode("utf8", "replace"))
        if m:
            modes[m.group(1)] = modes.get(m.group(1), 0) + 1
            ms.append(float(m.group(2)) / 1000.0)
    if not ms:
        return None
    return {"mode": max(modes, key=modes.get), "median": statistics.median(ms), "min": min(ms), "max": max(ms), "n": len(ms)}


def cold_available(want):
    """True when the page-cache purge command works without a password (never prompts: sudo -n)."""
    if not want:
        return False
    if shutil.which("sudo") is None:
        print("cold runs skipped: sudo is not installed")
        return False
    r = run(shlex.split(PURGE_CMD), timeout=300)
    if r.returncode != 0:
        print(f"cold runs skipped: `{PURGE_CMD}` failed ({r.stderr.decode('utf8', 'replace').strip()[:120] or 'passwordless sudo is not available'})")
        return False
    return True


def median_of(r):
    return r["median"] if r else None


def speed(args):
    greeg = os.path.abspath(args.greeg)
    names = args.corpora.split(",") if args.corpora else [n for n, s in CORPORA.items() if s.get("size") == "small"]
    cold = cold_available(args.cold)
    gver = version_of([greeg, "--version"])
    result = {"protocol": PROTOCOL, "prepare": PREPARE_SLEEP, "host": host_info(), "date": datetime.datetime.now().isoformat(timespec="seconds"), "greeg": gver, "rg": version_of(["rg", "--version"]), "grep": version_of(["grep", "--version"]), "hyperfine": version_of(["hyperfine", "--version"]), "runs": args.runs, "corpora": {}}
    # CPU wake-up penalty of the protocol: every timed command starts after `sleep 0.15`, which
    # on Apple Silicon adds a few ms of idle-state and frequency ramp to every process. Measured
    # once per run on `--version` hot and after the sleep, so the small rows can be read either way.
    wake = {}
    for label, cmd in (("greeg", [greeg, "--version"]), ("rg", ["rg", "--version"])):
        hot = hyperfine([cmd], os.getcwd(), runs=20, warmup=3)[0]
        slept = hyperfine([cmd], os.getcwd(), runs=20, warmup=3, prepare=PREPARE_SLEEP)[0]
        wake[label] = {"hot_ms": hot["median"] * 1e3 if hot else None, "prepared_ms": slept["median"] * 1e3 if slept else None}
    result["wakeup"] = wake
    fmt_ms = lambda x: f"{x:.1f} ms" if x is not None else "n/a"
    print("wake-up penalty: " + "  ".join(f"{k} --version {fmt_ms(v['hot_ms'])} hot → {fmt_ms(v['prepared_ms'])} after the sleep" for k, v in wake.items()))
    prev = load_json(os.path.join(RESULTS, f"speed-{host_key()}.json"))
    if prev and args.corpora and not args.no_splice:
        # a subset run refreshes only its corpora; rows kept from an earlier run are marked when
        # they were measured with another binary or protocol, so the table never mixes them silently
        for c, e in prev.get("corpora", {}).items():
            if c in names:
                continue
            stale = []
            e_ver, e_date, e_proto = e.get("greeg", prev.get("greeg")), e.get("date", prev.get("date")), e.get("protocol", prev.get("protocol", 1))
            if e_ver != gver:
                stale.append(f"measured with `{e_ver}` on {e_date}; the other rows use `{gver}`")
            if e_proto != PROTOCOL:
                stale.append(f"measured with protocol {e_proto} (no sleep between runs: the TTL hid the freshness check); current protocol is {PROTOCOL}")
            e["stale"] = "; ".join(stale) or None
            result["corpora"][c] = e
            print(f"{c}: kept from {e_date}" + (f"  STALE: {e['stale']}" if e["stale"] else ""))
    for name in names:
        spec = CORPORA[name]
        path = corpus_path(name)
        is_file = spec.get("kind") == "file"
        if is_file:
            if not os.path.isfile(path):
                print(f"{name}: not fetched (bench/bench.py fetch {name}); skipped")
                continue
            cwd, target = os.path.dirname(path), os.path.basename(path)
            nfiles, nbytes = 1, os.path.getsize(path)
        else:
            if not os.path.isdir(path):
                print(f"{name}: not fetched (bench/bench.py fetch {name}); skipped")
                continue
            cwd, target = path, "."
            nfiles, nbytes = corpus_stats(cwd)
        print(f"\n== {name}: {nfiles} files, {nbytes/1e6:.0f} MB" + ("  (single file: scan-only rows, no index)" if is_file else ""))
        entry = {"files": nfiles, "bytes": nbytes, "sha": spec.get("sha"), "kind": spec.get("kind", "repo"), "greeg": gver, "date": result["date"], "protocol": PROTOCOL, "stale": None, "index": {}, "queries": {}}
        if not is_file:
            idx = tempfile.mkdtemp(prefix="greeg-bench-idx-")
            build = hyperfine([[greeg, "index", "--index-dir", idx, "--root", "."]], cwd, runs=max(2, args.runs // 3), warmup=0, prepare=f"rm -rf {idx}")[0]
            shutil.rmtree(idx, ignore_errors=True)
            run([greeg, "index", "--root", "."], cwd)  # the default index used by the queries
            # the build records its own getrusage high-water mark, so this is
            # the peak of the run that produced the index the queries use, and
            # costs no extra build
            rss = manifest_rss_mb(greeg, cwd)
            info = doctor_index(greeg, cwd)
            info.update({"build_s": build["median"] if build else None, "build_mean_s": build["mean"] if build else None, "build_min_s": build["min"] if build else None, "build_max_s": build["max"] if build else None, "build_stddev": build["stddev"] if build else None, "rss_mb": rss})
            entry["index"] = info
            print(f"   index build {fmt_ms(info['build_s'])} median ({fmt_ms(info.get('build_min_s'))}–{fmt_ms(info.get('build_max_s'))})  size {info['size']} ({info['ratio']}× of source)  RSS {rss and round(rss)} MB")
        for fam in FAMILIES:
            pat = spec["queries"].get(fam)
            if not pat:
                continue
            tools = list(FILE_TOOLS if is_file else TOOLS)
            timed = tools + ([] if is_file else ["greeg-nofresh"])
            if fam == "regex":
                timed = [("grep-E" if t == "grep" else t) for t in timed]
            cmds = [tool_cmd(t, greeg, fam, pat, spec.get("kind"), target) for t in timed]
            # match-set verification. rg --json is the reference. `greeg-full`: the (path, line) set parsed
            # from the text the *timed* command prints. `greeg` (budgeted digest) cannot be verified from
            # its own output: it is checked as `--json --budget 0` with the same query, which is what
            # `rg_eq_greeg` means. grep is compared by text (no gitignore: its set may exceed rg's).
            if is_file:
                rgj = json_hits(out_of(["rg", "--json", "-n", *FAMILY_FLAGS[fam], "-e", pat, target], cwd))
                ggj = json_hits(out_of([greeg, "--json", "--no-index", "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", "--max-filesize", HUGE_FILE, *FAMILY_FLAGS[fam], "-e", pat, target], cwd))
                full_txt = None
            else:
                rgj = json_hits(out_of(["rg", "--json", "-n", *FAMILY_FLAGS[fam], "-e", pat, "."], cwd))
                ggj = json_hits(out_of([greeg, "--json", "--no-session", "--budget", "0", "--no-ladder", "--max-columns", "0", *FAMILY_FLAGS[fam], "-e", pat, "."], cwd))
                full_txt = {h for h in greeg_text_hits(out_of(tool_cmd("greeg-full", greeg, fam, pat, None), cwd)) if h}
            grp = {h for h in text_hits(out_of(cmds[0], cwd)) if h}
            verified = {"rg": len(rgj), "greeg": len(ggj), "grep": len(grp), "rg_eq_greeg": rgj == ggj, "grep_eq_rg": grp == rgj, "greeg_verified_as": "greeg --json --budget 0 (same query, unbudgeted JSON); the timed budgeted digest is not verifiable from its own output"}
            if full_txt is not None:
                verified.update({"greeg_full_text": len(full_txt), "rg_eq_greeg_full_text": full_txt == rgj})
            states = [("warm", PREPARE_SLEEP)] + ([("cold", PURGE_CMD)] if cold else [])
            fam_entry = {"pattern": pat, "matches": verified, "tools": {}}
            for state, prep in states:
                res = hyperfine(cmds, cwd, runs=args.runs if state == "warm" else max(3, args.runs // 3), warmup=3 if state == "warm" else 0, prepare=prep)
                for t, r in zip(timed, res):
                    key = t.replace("grep-E", "grep")
                    fam_entry["tools"].setdefault(key, {})[state] = r
            if not is_file:
                fam_entry["fresh"] = fresh_cost(greeg, [*FAMILY_FLAGS[fam], "-e", pat, "."], cwd, samples=max(3, args.runs))
            for t in tools:
                if t in RSS_TOOLS:
                    fam_entry["tools"][t]["rss_mb"] = max_rss_mb(tool_cmd(t, greeg, fam, pat, spec.get("kind"), target), cwd)
            entry["queries"][fam] = fam_entry
            line = "  ".join(f"{t}={fmt_ms(median_of(fam_entry['tools'][t].get('warm')))}" for t in tools)
            fr = fam_entry.get("fresh")
            print(f"   {fam:6} {pat!r:34} {line}" + (f"  fresh={fmt_ms(fr['median'])} ({fr['mode']})" if fr else "") + f"  matches rg={verified['rg']} greeg={verified['greeg']} grep={verified['grep']}" + ("" if verified["rg_eq_greeg"] else "  MISMATCH rg/greeg(json)") + ("" if verified.get("rg_eq_greeg_full_text", True) else "  MISMATCH rg/greeg-full(text)"))
        if not is_file:
            for verb in VERBS:
                nm = spec["queries"].get(verb)
                if not nm:
                    continue
                cmd = [greeg, verb, nm, "--no-session"]
                r = hyperfine([cmd], cwd, runs=args.runs, warmup=3, prepare=PREPARE_SLEEP)[0]
                entry["queries"][verb] = {"pattern": nm, "tools": {"greeg": {"warm": r}}}
                print(f"   {verb:6} {nm!r:34} greeg={fmt_ms(median_of(r))}")
        result["corpora"][name] = entry
    result["geomean"] = geomeans(result["corpora"])
    result["geomean_vs_rg"] = result["geomean"].get("rg", {})
    for base in ("rg-j4", "rg"):
        print(f"\ngeometric-mean speed vs {base} (medians, > 1 = faster than {base}):")
        for fam, d in result["geomean"].get(base, {}).items():
            print(f"   {fam:6} " + "  ".join(f"{t}={v:.2f}×" for t, v in d.items()))
    save_json(os.path.join(RESULTS, f"speed-{host_key()}.json"), result)
    save_json(os.path.join(RESULTS, "speed.json"), result)


def geomeans(corpora):
    """{base: {family: {tool: geometric mean of base_median / tool_median over corpora}}}, warm runs, current rows only."""
    gm = {}
    for base in ("rg-j4", "rg"):
        for fam in FAMILIES:
            for tool in TOOLS + ["greeg-nofresh"]:
                ratios = []
                for e in corpora.values():
                    if e.get("stale"):
                        continue
                    q = e["queries"].get(fam)
                    if not q or tool not in q["tools"] or base not in q["tools"]:
                        continue
                    a, b = q["tools"][base].get("warm"), q["tools"][tool].get("warm")
                    if a and b:
                        ratios.append(a["median"] / b["median"])
                if ratios:
                    gm.setdefault(base, {}).setdefault(fam, {})[tool] = geomean(ratios)
    return gm


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
    """{name: {"kind": kind, "defs": {(path, line)}, "refs": {(path, line)}}}, per-line occurrence map, documents, indexer ToolInfo."""
    with open(path, "rb") as fh:
        data = fh.read()
    names = {}
    lines = {}  # (path, line) -> set of (name, is_def)
    docs = set()
    meta = {}
    for f, v in pb_fields(data):
        if f == 1 and isinstance(v, bytes):
            for mf, mv in pb_fields(v):
                if mf == 2 and isinstance(mv, bytes):
                    for tf, tv in pb_fields(mv):
                        if tf == 1:
                            meta["tool"] = tv.decode("utf8", "replace")
                        elif tf == 2:
                            meta["version"] = tv.decode("utf8", "replace")
            continue
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
    return names, lines, docs, meta


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



def wsample(items, k, weight, rnd):
    """Weighted sampling without replacement (deterministic for a given seed; items are sorted first)."""
    items = sorted(items)
    weights = [weight(n) for n in items]
    picked = []
    while items and len(picked) < k:
        i = rnd.choices(range(len(items)), weights)[0]
        picked.append(items.pop(i))
        weights.pop(i)
    return picked


MIN_REFS = 3
ZERO_REF_BUCKET = "0ref"


def sample_names(names, per_bucket, seed, zero_ref=10):
    """Names stratified by ambiguity (1 / 2–5 / 6+ definitions); within a bucket sampled without
    replacement with probability ∝ log(1 + reference count), so the sample leans toward names an
    agent actually asks about. Names need ≥ MIN_REFS references; a separate small bucket of
    zero-reference names is drawn uniformly and reported on its own."""
    rnd = random.Random(seed)
    buckets = {"1": [], "2-5": [], "6+": []}
    zero = []
    for n, e in names.items():
        # `meta` descriptors are locals and object-literal properties, `namespace`
        # ones are files and modules whose SCIP definition is line 1 of the file:
        # neither is a definition an agent asks `def NAME` about.
        if len(n) < 3 or not e["defs"] or not re.match(r"^[A-Za-z_]\w*$", n) or e["kind"] in ("meta", "namespace"):
            continue
        d, r = len(e["defs"]), len(e["refs"])
        if r == 0:
            zero.append(n)
        elif r >= MIN_REFS:
            buckets["1" if d == 1 else "2-5" if d <= 5 else "6+"].append(n)
    out = []
    for b, lst in buckets.items():
        out += [(n, b) for n in wsample(lst, per_bucket, lambda n: math.log1p(len(names[n]["refs"])), rnd)]
    zero.sort()
    out += [(n, ZERO_REF_BUCKET) for n in rnd.sample(zero, min(zero_ref, len(zero)))]
    return out


def acc_at(shown, truth, k):
    return any(h in truth for h in shown[:k])


# what an agent types when it wants the definition with rg: a per-language definition regex
DEF_REGEX = {
    "rust": r"(fn|struct|enum|trait|type|mod|const|static|union|impl(<[^>]*>)?( \w+ for)?) {n}\b",
    "python": r"(def|class) {n}\b",
    "typescript": r"(function|class|interface|type|enum|const|let|var|namespace) {n}\b",
    "javascript": r"(function|class|const|let|var) {n}\b",
    "kotlin": r"(fun|class|interface|object|val|var|typealias|enum class|data class) {n}\b",
}


def rg_def_cmd(lang, nm):
    return ["rg", "-n", "--sort", "path", "-e", DEF_REGEX.get(lang, DEF_REGEX["typescript"]).format(n=re.escape(nm)), "."]


# greeg text lines that summarise rather than locate: neutral in the context metric
NEUTRAL_RE = re.compile(r"^(?:by (?:kind|area|lang|flag)\b|areas  |langs  |definitions \(|top hits\b|imported by \d|next:|\s*\+\d[\d,]* (?:more|test|vendored|generated|demoted|mock)\b|[\d,]+ of [\d,]+ hits\b|[\d,]+/[\d,]+ hits\b|[\d,]+ hits · [\d,]+ files\b|no hits\b|\S.*  [\d,]+ (?:matches|hits) · |see also\b|hint:|matched \w|defined at\b|related  |\w+ \(\d+\)$|\s*\.\.\.$|\S.*  \d+ of \d+ definitions\b|(?:WILL|MAY) BREAK\b|REVIEW\b|refs \S+  |callers \S+  |impact \S+  |def \S+  )")


def greeg_line_classes(text):
    """[(location or None, neutral)] per emitted line of greeg's text output. Neutral lines are the
    header, facet lines, section titles, file headers, `+N more`, the footer and blank lines: they
    aggregate rather than locate, so they leave the denominator of the useful-line ratio. Context
    lines (no kind tag) stay in the denominator: they cost tokens without naming a true location."""
    out = []
    for line, loc in zip(text.splitlines(), greeg_text_hits(text)):
        neutral = loc is None and (not line.strip() or bool(NEUTRAL_RE.match(line)) or bool(GREEG_FILE_RE.match(line)))
        out.append((loc, neutral))
    return out


def plain_line_classes(text):
    return [(loc, loc is None and (not line.strip() or line == "--")) for line, loc in zip(text.splitlines(), text_hits(text))]


def context_metrics(emitted, classes, truth_all, truth_def, docs):
    """Context@500 for one tool: distinct true locations, coverage, tokens, tokens until the first
    and until every true definition has been printed (None when not reached within the window).

    Two useful-line ratios (protocol 3): `useful_ratio` over every line that names a location, and
    `useful_ratio_covered` over the lines in files the SCIP index actually holds. The oracle knows
    nothing about the other files — `lib.dom.d.ts`, `tests/baselines/**`, fixture `.js` — so it
    scores a line there as not-useful by default, though it cannot adjudicate it; the classification
    metric already skips them for exactly that reason."""
    seen, useful_lines = set(), 0
    first_def_tokens = all_defs_tokens = None
    covered_defs = set()
    for k, (loc, _) in enumerate(classes):
        if loc and loc in truth_all:
            useful_lines += 1
            seen.add(loc)
        if loc and loc in truth_def:
            covered_defs.add(loc)
            if first_def_tokens is None:
                first_def_tokens = tokens("\n".join(emitted[:k + 1]))
            if all_defs_tokens is None and covered_defs >= truth_def:
                all_defs_tokens = tokens("\n".join(emitted[:k + 1]))
    tk = tokens("\n".join(emitted))
    neutral = sum(1 for _, n in classes if n)
    denom = max(1, len(classes) - neutral)
    adjudicable = max(1, sum(1 for loc, n in classes if not n and loc and loc[0] in docs))
    return {"lines": len(classes), "neutral": neutral, "useful": len(seen), "useful_lines": useful_lines, "useful_ratio": len(seen) / denom, "lines_covered": adjudicable, "useful_ratio_covered": len(seen) / adjudicable, "coverage": len(seen) / max(1, len(truth_all)), "tokens": tk, "useful_per_ktok": len(seen) / max(1, tk) * 1000, "tokens_to_def": first_def_tokens, "tokens_to_all_defs": all_defs_tokens, "defs_covered": len(covered_defs) / max(1, len(truth_def))}


def scip_tool_version(name, lang):
    if lang == "rust":
        return version_of(["rust-analyzer", "--version"])
    if lang == "kotlin":
        return version_of(["scip-java", "--version"])
    return None  # scip-typescript / scip-python: taken from the index's metadata (ToolInfo)


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
        lang = CORPORA[name]["lang"]
        t0 = time.time()
        names, lines, docs, meta = read_scip(idx)
        print(f"\n== {name}: SCIP {os.path.getsize(idx)/1e6:.0f} MB by {meta.get('tool') or '?'} {meta.get('version') or ''}, {len(docs)} documents, {len(names)} names with occurrences, {sum(1 for e in names.values() if e['defs'])} defined in-repo ({time.time()-t0:.1f} s to decode); tokenizer {TOKENIZER}")
        run([greeg, "index", "--root", "."], cwd)
        samples = sample_names(names, args.per_bucket, args.seed, args.zero_ref)
        rows = []
        for i, (nm, bucket) in enumerate(samples):
            e = names[nm]
            truth_def, truth_ref = e["defs"], e["refs"]
            truth_all = truth_def | truth_ref
            row = {"name": nm, "bucket": bucket, "kind": e["kind"], "defs": len(truth_def), "refs": len(truth_ref)}
            # definitions: greeg def (JSON); rg -nw and grep -rnw first lines (thread order, what an
            # agent sees); rg with the language's definition regex, --sort path (deterministic)
            gd = [(j["path"], j["line"]) for j in map(json.loads, (l for l in out_of([greeg, "def", nm, "--json", "--no-session"], cwd).splitlines() if l.startswith("{"))) if j.get("type") == "def"]
            rgd = [h for h in text_hits(out_of(["rg", "-n", "-w", "-e", nm, "."], cwd)) if h]
            rgdef = [h for h in text_hits(out_of(rg_def_cmd(lang, nm), cwd)) if h]
            grd = [h for h in text_hits(out_of(["grep", "-rnwI", "--exclude-dir=.git", "--exclude-dir=node_modules", "-e", nm, "."], cwd)) if h]
            for tool, shown in (("greeg", gd), ("rg", rgd), ("rgdef", rgdef), ("grep", grd)):
                row[f"{tool}_acc"] = [acc_at(shown, truth_def, k) for k in (1, 5, 10)]
            row["greeg_def_shown"] = len(gd)
            row["rgdef_shown"] = len(rgdef)
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
                classes = (greeg_line_classes if tool.startswith("greeg") else plain_line_classes)("\n".join(emitted))
                row[f"ctx_{tool}"] = context_metrics(emitted, classes, truth_all, truth_def, docs)
            rows.append(row)
            if (i + 1) % 15 == 0:
                print(f"   … {i+1}/{len(samples)}")
        main_rows = [r for r in rows if r["bucket"] != ZERO_REF_BUCKET]
        zero_rows = [r for r in rows if r["bucket"] == ZERO_REF_BUCKET]
        summary = summarize_oracle(main_rows, args.seed)
        zero_summary = summarize_oracle(zero_rows, args.seed, light=True) if zero_rows else None
        misses = [{"name": r["name"], "kind": r["kind"], "bucket": r["bucket"], "scip_defs": sorted(names[r["name"]]["defs"])[:3], "greeg_shown": r["greeg_def_shown"]} for r in rows if not r["greeg_acc"][2]]
        all_results[name] = {"date": datetime.datetime.now().isoformat(timespec="seconds"), "scip": os.path.basename(os.path.dirname(idx)), "scip_tool": meta, "scip_tool_cli": scip_tool_version(name, lang), "tokenizer": TOKENIZER, "greeg": version_of([greeg, "--version"]), "sampling": f"stratified by ambiguity 1 / 2-5 / 6+, {args.per_bucket} per bucket, weighted by log(1 + references), references >= {MIN_REFS}; plus {len(zero_rows)} zero-reference names reported separately", "seed": args.seed, "rg_def_regex": DEF_REGEX.get(lang, DEF_REGEX["typescript"]), "names": len(names), "sampled": len(main_rows), "sampled_zero_ref": len(zero_rows), "summary": summary, "summary_zero_ref": zero_summary, "misses": misses, "rows": rows}
        print_oracle(name, summary, zero_summary)
        for m in misses:
            print(f"   miss@10 {m['name']} ({m['kind']}, ambiguity {m['bucket']}): SCIP {', '.join(f'{p}:{l}' for p, l in m['scip_defs'])}; greeg def showed {m['greeg_shown']}")
        save_json(os.path.join(RESULTS, "oracle.json"), all_results)


def mean(xs):
    xs = [x for x in xs if x is not None]
    return (sum(xs) / len(xs)) if xs else None


def median(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def bootstrap_ci(xs, stat=mean, n=2000, seed=0):
    """95 % percentile bootstrap interval of `stat` over xs (None entries dropped)."""
    xs = [x for x in xs if x is not None]
    if len(xs) < 2:
        return None
    rnd = random.Random(seed)
    k = len(xs)
    vals = sorted(stat([xs[rnd.randrange(k)] for _ in range(k)]) for _ in range(n))
    return [vals[int(0.025 * n)], vals[min(n - 1, int(0.975 * n))]]


ACC_TOOLS = ("greeg", "rg", "rgdef", "grep")
CTX_TOOLS = ("greeg", "greeg6k", "rg", "grep")


def summarize_oracle(rows, seed=0, light=False):
    s = {"n": len(rows)}
    for tool in ACC_TOOLS:
        if rows and f"{tool}_acc" in rows[0]:
            s[f"{tool}_acc"] = [mean([r[f"{tool}_acc"][i] for r in rows]) for i in range(3)]
            s[f"{tool}_acc1_ci"] = bootstrap_ci([float(r[f"{tool}_acc"][0]) for r in rows], seed=seed)
    if light:
        return s
    for tool in CTX_TOOLS:
        c = [r[f"ctx_{tool}"] for r in rows]
        s[f"{tool}_ctx_useful_per_ktok"] = mean([x["useful_per_ktok"] for x in c])
        s[f"{tool}_ctx_useful_ratio"] = mean([x["useful_ratio"] for x in c])
        s[f"{tool}_ctx_useful_ratio_covered"] = mean([x["useful_ratio_covered"] for x in c]) if "useful_ratio_covered" in c[0] else None
        s[f"{tool}_ctx_useful_ratio_covered_ci"] = bootstrap_ci([x["useful_ratio_covered"] for x in c], seed=seed) if "useful_ratio_covered" in c[0] else None
        s[f"{tool}_ctx_neutral_ratio"] = mean([x["neutral"] / x["lines"] for x in c if x["lines"]])
        s[f"{tool}_ctx_coverage"] = mean([x["coverage"] for x in c])
        s[f"{tool}_tokens"] = mean([x["tokens"] for x in c])
        s[f"{tool}_tokens_median"] = median([x["tokens"] for x in c])
        s[f"{tool}_tokens_median_ci"] = bootstrap_ci([x["tokens"] for x in c], stat=statistics.median, seed=seed)
        s[f"{tool}_tokens_to_def"] = median([x["tokens_to_def"] for x in c])
        s[f"{tool}_def_reached"] = mean([x["tokens_to_def"] is not None for x in c])
        s[f"{tool}_tokens_to_all_defs"] = median([x["tokens_to_all_defs"] for x in c])
        s[f"{tool}_all_defs_reached"] = mean([x["tokens_to_all_defs"] is not None for x in c])
        s[f"{tool}_defs_covered"] = mean([x["defs_covered"] for x in c])
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


def ci_pp(ci):
    return "" if not ci else f" [{100*ci[0]:.0f}–{100*ci[1]:.0f}]"


def ci_n(ci):
    return "" if not ci else f" [{round(ci[0]):,}–{round(ci[1]):,}]"


def accs(s, tool):
    return "/".join(pct(x) for x in s[f"{tool}_acc"]) + ci_pp(s.get(f"{tool}_acc1_ci")) if f"{tool}_acc" in s else "–"


def print_oracle(name, s, zero=None):
    print(f"\n{name}: definitions Acc@1/5/10 [95 % CI of Acc@1], n={s['n']}  greeg {accs(s, 'greeg')}   rg -nw {accs(s, 'rg')}   rg-def {accs(s, 'rgdef')}   grep {accs(s, 'grep')}")
    for b in ("1", "2-5", "6+"):
        if f"greeg_acc_bucket_{b}" in s:
            print(f"   ambiguity {b:4} (n={s[f'n_bucket_{b}']:2}): greeg Acc@1/5/10 {'/'.join(pct(x) for x in s[f'greeg_acc_bucket_{b}'])}")
    if zero:
        print(f"   zero-reference names (n={zero['n']}): greeg {accs(zero, 'greeg')}  rg -nw {accs(zero, 'rg')}  rg-def {accs(zero, 'rgdef')}")
    print(f"   reference recall (greeg refs vs SCIP) {pct(s['ref_recall'])}; definition recall {pct(s['def_recall'])}")
    for key in ("spans", "precise"):
        c = s[f"cls_{key}"]
        print(f"   classification [{key}]: def precision {pct(c['def_precision'])} ({pct(c['def_precision_with_impl'])} counting {c['impl_headers']} impl headers as errors) recall {pct(c['def_recall'])}; code hits on a SCIP occurrence {pct(c['code_on_scip'])}; noncode precision {pct(c['noncode_precision'])} ({c['noncode_hits']} noncode / {c['code_hits']} code hits)")
    for tool in CTX_TOOLS:
        print(f"   context@500 {tool:7}: useful lines {pct(s[f'{tool}_ctx_useful_ratio'])} / {pct(s.get(f'{tool}_ctx_useful_ratio_covered'))} of SCIP-covered{ci_pp(s.get(f'{tool}_ctx_useful_ratio_covered_ci'))} (neutral {pct(s.get(f'{tool}_ctx_neutral_ratio'))})  coverage {pct(s[f'{tool}_ctx_coverage'])}  tokens mean {round(s[f'{tool}_tokens'] or 0):,} median {round(s.get(f'{tool}_tokens_median') or 0):,}{ci_n(s.get(f'{tool}_tokens_median_ci'))}  distinct true/ktok {s[f'{tool}_ctx_useful_per_ktok']:.1f}  tokens→first def {s[f'{tool}_tokens_to_def'] and round(s[f'{tool}_tokens_to_def'])} (reached {pct(s[f'{tool}_def_reached'])})  tokens→all defs {s.get(f'{tool}_tokens_to_all_defs') and round(s[f'{tool}_tokens_to_all_defs'])} (reached {pct(s.get(f'{tool}_all_defs_reached'))})")


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
        if args.record_only:
            print(f"speed results recorded for {key} (protocol {cur.get('protocol', 1)}, {cur.get('greeg')}); no baseline comparison on this host")
            for c, e in cur["corpora"].items():
                for fam, q in e["queries"].items():
                    r = q["tools"].get("greeg", {}).get("warm")
                    if r:
                        print(f"   {c:20} {fam:8} greeg {fmt_ms(r['median'])} median  rg-j4 {fmt_ms(median_of(q['tools'].get('rg-j4', {}).get('warm')))}" + ("  (stale row)" if e.get("stale") else ""))
            return
        if args.save:
            save_json(base_path, cur)
            print(f"baseline saved for {key}")
            return
        if not base:
            if args.require_baseline:
                sys.exit(f"GATE FAIL: no speed baseline for host {key} under bench/baselines/ and --require-baseline was passed (record one on the reference machine with `bench/bench.py gate speed --save`)")
            save_json(base_path, cur)
            print(f"baseline created for {key}; nothing to compare (pass --require-baseline to make this an error)")
            return
        if base.get("protocol", 1) != cur.get("protocol", 1):
            sys.exit(f"GATE FAIL: baseline for {key} was recorded with protocol {base.get('protocol', 1)} and the current results with protocol {cur.get('protocol', 1)}; the numbers are not comparable. Re-record the baseline with `bench/bench.py gate speed --save`.")
        worst = []
        for c, e in cur["corpora"].items():
            be = base["corpora"].get(c)
            if not be or e.get("stale"):
                continue
            for fam, q in e["queries"].items():
                bq = be["queries"].get(fam)
                if not bq:
                    continue
                for tool in ("greeg", "greeg-full", "greeg-scan"):
                    a = q["tools"].get(tool, {}).get("warm")
                    b = bq["tools"].get(tool, {}).get("warm")
                    if a and b and b["median"] > 0:
                        d = a["median"] / b["median"] - 1
                        worst.append((d, f"{c} {fam} {tool}: {fmt_ms(b['median'])} → {fmt_ms(a['median'])} median ({d:+.0%})"))
            if e["index"].get("build_s") and be["index"].get("build_s"):
                d = e["index"]["build_s"] / be["index"]["build_s"] - 1
                worst.append((d, f"{c} index build: {fmt_ms(be['index']['build_s'])} → {fmt_ms(e['index']['build_s'])} ({d:+.0%})"))
            if e["index"].get("rss_mb") and be["index"].get("rss_mb"):
                d = e["index"]["rss_mb"] / be["index"]["rss_mb"] - 1
                worst.append((d, f"{c} index build peak RSS: {be['index']['rss_mb']:.0f} → {e['index']['rss_mb']:.0f} MB ({d:+.0%})"))
        worst.sort(reverse=True)
        for d, line in worst[:8]:
            print(("SLOWER  " if d > tol else "ok      ") + line)
        bad = [w for w in worst if w[0] > tol]
        print(f"speed gate ({args.tolerance:.0f} %, medians): {'FAIL' if bad else 'PASS'} ({len(worst)} comparisons, {len(bad)} regressions)")
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
    if args.record_only:
        save_json(os.path.join(RESULTS, f"kernels-{key}.json"), {"host": host_info(), "ns": cur})
        for k, v in sorted(cur.items()):
            print(f"   {k}: {v/1e3:.1f} µs")
        print(f"kernel results recorded for {key}; no baseline comparison on this host")
        return
    if args.save:
        save_json(base_path, {"host": host_info(), "ns": cur})
        print(f"kernel baseline saved for {key}")
        return
    if not base:
        if args.require_baseline:
            sys.exit(f"GATE FAIL: no kernel baseline for host {key} under bench/baselines/ and --require-baseline was passed")
        save_json(base_path, {"host": host_info(), "ns": cur})
        print(f"kernel baseline created for {key}; nothing to compare (pass --require-baseline to make this an error)")
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

def cell(r):
    return "–" if not r else f"{fmt_ms(r['median'])} ({fmt_ms(r['min'])}–{fmt_ms(r['max'])})"


def mb(x):
    return "n/a" if x is None else f"{round(x)} MB"


def matching_report(output_dir, datasets=None):
    datasets = datasets or ReportDatasets(RESULTS)
    matrix_entry = datasets.section("matching")[0]
    recheck_entry = datasets.section("matching_recheck")[0]
    matrix_name, recheck_name = matrix_entry.filename, recheck_entry.filename
    matrix = datasets.get(matrix_entry)
    if not matrix or not matrix.get("results"):
        return []
    recheck = datasets.get(recheck_entry)
    binaries = matrix["binaries"]
    machine = [r for r in matrix["results"] if r["candidate"]["rg_stdout_and_status_equal"] is not None]
    if not machine:
        return []
    token_note = (f"Tokens count stdout plus stderr with `{matrix['tokenizer']}`."
                  if matrix.get("tokenizer") else "Token counts were not measured (n/a).")
    passed = lambda label: sum(r[label]["rg_stdout_and_status_equal"] for r in machine)
    out = report_heading(matrix_entry) + [
        f"Focused comparison of `{binaries['baseline']['version']}` against `{binaries['candidate']['version']}`. "
        "The candidate contains the exact/discovery policy change. These measurements are separate from the older full-corpus tables below.", "",
        f"{matrix['platform']}, {matrix['cpu_count']} logical CPUs; "
        f"{matrix['corpus']['files']} generated Rust files ({matrix['corpus']['bytes']:,} bytes). "
        f"{matrix['runs']} randomized paired runs per case after {matrix['warmups']} warmups; "
        "release binaries, one reader thread, warm caches, `--fresh stat`, sessions/statistics disabled. "
        f"Times include process startup. {token_note}", "",
        f"**Correctness:** stdout and exit-status parity with {matrix['ripgrep']} improved from "
        f"{passed('baseline')}/{len(machine)} to {passed('candidate')}/{len(machine)} machine-output cases. "
        "The improvement removes unwanted suggestions on exact misses; it is not a general ripgrep-compatibility score.", "",
        "### Exact misses", "",
    ]
    out += paired_table([r for r in machine if not r["case"].startswith("hit_")], "backend", "Backend",
                        tokenizer=matrix.get("tokenizer"), token_key="tokens_both_streams")
    hits = [r for r in machine if r["case"].startswith("hit_")]
    unchanged = all(r["stdout_and_status_unchanged"] for r in hits)
    if hits:
        summary = latency_summary(hits)
        timing = (f"Median latency changes ranged from {summary['median_min']:+.1f}% to {summary['median_max']:+.1f}%."
                  if summary else "Latency comparison unavailable.")
        out += ["", f"Successful machine-query stdout and status unchanged: **{'yes' if unchanged else 'no'}**. "
                + timing, ""]
    if recheck and recheck.get("results"):
        original = next((r for r in matrix["results"] if r["backend"] == "scan" and r["case"] == "ranked_hit"), None)
        trigger = ""
        original_summary = latency_summary([original]) if original else None
        if original_summary:
            trigger = f"The first run's scan-ranked-hit p95 changed {original_summary['p95_max']:+.1f}%, triggering a {recheck['runs']}-pair recheck. "
        summary = latency_summary(recheck["results"])
        investigation = (f"Recheck cases exceeding the 10% median / 20% p95 investigation thresholds: **{summary['flagged']}**."
                         if summary else "Recheck latency comparison unavailable.")
        out += report_heading(recheck_entry, level=3)
        out += [trigger + "Both runs are retained. " + investigation, ""]
        out += paired_table(recheck["results"], "backend", "Backend")
        out += ["", f"[Ranked recheck and raw samples]({result_link(datasets.root, output_dir, recheck_name)})."]
    out += ["", f"[Original matrix and raw samples]({result_link(datasets.root, output_dir, matrix_name)}) include binary/corpus digests. "
            "Reproduce with `python3 bench/matching.py BASELINE CANDIDATE --runs 31 --tokens --output matrix.json`; "
            "for the recheck use `python3 bench/matching.py BASELINE CANDIDATE --runs 151 --cases ranked_hit ranked_discovery --tokens --output recheck.json`.", "",
            "**Limits:** this is a warm synthetic-corpus comparison, not a new full-corpus result or whole-agent-task token estimate. "
            "Cold cache, peak RSS, concurrency, and agent-task savings were not measured here. "
            "The older corpus results below retain their original versions and dates.", ""]
    return out


def matching_review_report(output_dir, datasets=None):
    datasets = datasets or ReportDatasets(RESULTS)
    out = []
    for entry in datasets.section("matching_review"):
        filename, title = entry.filename, entry.title
        data = datasets.get(entry)
        if not data or not data.get("results"):
            continue
        rows = data["results"]
        contracts = ("rg_stdout_and_status_equal", "json_exact_contract", "definition_contract")
        def passed(label):
            checks = [r[label][k] for r in rows for k in contracts if r[label].get(k) is not None]
            return f"{sum(checks)}/{len(checks)}" if checks else "not measured"
        summary = latency_summary(rows)
        timing = (f"Cases above the 10% median / 20% p95 investigation thresholds: **{summary['flagged']}**."
                  if summary else "Latency comparison unavailable.")
        link = result_link(datasets.root, output_dir, filename)
        out += report_heading(entry, title=title) + [
                f"`{data['binaries']['baseline']['version']}` → `{data['binaries']['candidate']['version']}`; "
                f"{data['runs']} randomized pairs per case, {data['warmups']} warmups on the same {data['corpus']['files']}-file warm synthetic corpus. "
                f"Contract checks passed: **{passed('baseline')} → {passed('candidate')}**. "
                + timing, ""]
        if entry.show_rows:
            out += paired_table(rows, "backend", "Backend", tokenizer=data.get("tokenizer"),
                                token_key="tokens_both_streams")
            out.append("")
        token_flag = " --tokens" if data.get("tokenizer") else ""
        out += [f"[Raw samples, environment, and binary/corpus digests]({link}). "
                f"Reproduce: `python3 bench/matching.py BASELINE CANDIDATE --cases {' '.join(data['cases'])} --runs {data['runs']}{token_flag} --output {filename}`.", ""]
    if out:
        out += ["JSON contracts compare match paths, lines, offsets, submatches, status, exact rung, and total hit counts with ripgrep. "
                "Repeat-output checks remove only elapsed fields; byte/token measurements retain them and use the first raw sample, so small JSON size differences reflect timing values. "
                "Definition checks compare paths and status on this controlled fixture, not general symbol-resolution accuracy. "
                "The initial review run used a full name-table case-fold scan; the final recheck uses prefix ranges. "
                "Both runs remain available. The corrected indexed case-insensitive hit now returns definitions instead of an empty answer, so its increased output is expected. "
                "Non-ASCII definition names absent from the symbol/module index use a scan fallback; its latency on large repositories is not measured here.", ""]
    return out


def hook_report(output_dir, datasets=None):
    datasets = datasets or ReportDatasets(RESULTS)
    out = []
    for entry in datasets.section("hook"):
        filename, title = entry.filename, entry.title
        data = datasets.get(entry)
        if not data or not data.get("results"):
            continue
        rows = data["results"]
        summary = latency_summary(rows)
        timing = (f"Hook process median latency changes ranged from {summary['median_min']:+.1f}% to {summary['median_max']:+.1f}%. "
                  f"Cases above the 10% median / 20% p95 investigation thresholds: **{summary['flagged']}**."
                  if summary else "Latency comparison unavailable.")
        searches = [r for r in data["search_contracts"] if r["binary"] == "candidate"]
        parity = (f"Candidate file/count match-row and exit-status checks: "
                  f"{sum(r['stdout_and_status_equal'] for r in searches)}/{len(searches)} against {data['ripgrep']}. "
                  "Explicit source paths and expected hit/miss assertions exercise both scan and full-index searches, "
                  "including files over 4 MiB and explicit size limits. The oracle ignores row ordering and does not require identical stderr."
                  if data.get("protocol", 1) >= 2 else
                  "Protocol 1's search checks read empty stdin and do not establish corpus parity. "
                  "Only its hook-process measurements remain valid; use protocol 2 below for search validation.")
        link = result_link(datasets.root, output_dir, filename)
        out += report_heading(entry, title=title) + [
                f"`{data['binaries']['baseline']['version']}` → `{data['binaries']['candidate']['version']}`; "
                f"{data['runs']} randomized pairs per case after {data['warmups']} warmups. "
                f"The candidate passed {sum(r['candidate']['contract'] for r in rows)}/{len(rows)} hook eligibility/explicit-policy checks. "
                + parity, "", timing, ""]
        out += paired_table(rows, "agent", "Host protocol", tokenizer=data.get("tokenizer"),
                            token_key="reply_tokens", token_title="Reply tokens")
        token_flag = " --tokens" if data.get("tokenizer") else ""
        out += ["", f"[Raw samples, reply sizes, and binary/corpus digests]({link}). "
                f"Reproduce: `python3 bench/hooks.py BASELINE CANDIDATE --runs {data['runs']}{token_flag} --output {filename}`.", ""]
    if out:
        out += ["The initial run triggered a longer paired recheck; both are retained. "
                "Reply tokens count only hook JSON with the recorded tokenizer, not search results or total agent usage. "
                "Explicit matching and file-size flags add reply cost; declined commands emit no reply and continue with the original tool. "
                "Tests use synthetic fixtures and the recorded response shapes, not live host approvals. "
                "No new cold-cache, RSS, native-search performance, or whole-task token claim is made.", ""]
    return out


def hook_config_report(output_dir, datasets=None):
    datasets = datasets or ReportDatasets(RESULTS)
    out = []
    for entry in datasets.section("hook_config"):
        out += hook_config_dataset_report(output_dir, entry.filename, entry.title, datasets)
        if not entry.recheck_of:
            continue
        original = next(e for e in datasets.entries if e.id == entry.recheck_of)
        initial, recheck = datasets.get(original), datasets.get(entry)
        if not initial or not initial["results"] or not recheck or not recheck["results"]:
            continue
        same_binaries = all(initial["binaries"][label].get("sha256") and
                            initial["binaries"][label]["sha256"] == recheck["binaries"][label].get("sha256")
                            for label in ("baseline", "candidate"))
        out += [(entry.comparison_intro + " " if entry.comparison_intro else "")
                + "Both the initial run and latency recheck are retained above. "
                + ("Both runs use the same binary digests. " if same_binaries else "The binary digests differ or are unavailable; these runs are not a controlled recheck. ")
                + entry.comparison_limits, ""]
    return out


def hook_config_dataset_report(output_dir, filename, title, datasets=None):
    datasets = datasets or ReportDatasets(RESULTS)
    entry = next((e for e in datasets.section("hook_config") if e.filename == filename), None)
    if entry is None:
        raise ReportDataError(f"{filename}: not in the hook configuration catalog")
    data = datasets.get(entry)
    if not data or not data.get("results"):
        return []
    link = result_link(datasets.root, output_dir, filename)
    rows = data["results"]
    passed = {label: sum(row[label]["contract"] for row in rows) for label in ("baseline", "candidate")}
    extended = data.get("protocol", 1) >= 2
    coverage = ("Protocol 2 also checks byte-identical installed no-ops, matcher-less cleanup, "
                "custom matcher preservation, and retained TOML comments/trust data. " if extended else "")
    skills = data.get("suite") == "skills"
    if skills:
        coverage = ("Checks cover skill creation, managed no-ops, legacy adoption, edited/custom preservation, "
                    "managed/legacy removal, absent skills and dry runs, alongside configuration outcomes. "
                    "Preservation and no-op cases require identical content, inode, mode, mtime and ctime. ")
    else:
        coverage = ("Checks cover mixed handlers, prefix lookalikes, non-command handlers, wrong matchers, invalid UTF-8, "
                    "missing-file uninstall and initial installation. " + coverage)
    out = report_heading(entry, title=title) + [
           f"`{data['binaries']['baseline']['version']}` → `{data['binaries']['candidate']['version']}`; "
           f"{data['runs']} randomized paired runs per case after {data['warmups']} warmups. "
           "Each invocation uses a reset disposable home and an isolated configuration/cache. "
           "Timing includes process startup and installation/removal, excluding fixture reset and validation.", "",
           f"**{'Skill and configuration' if skills else 'Configuration'} contracts:** {passed['baseline']}/{len(rows)} → {passed['candidate']}/{len(rows)}. "
           + coverage + "Baseline failures are not equivalent successful work; "
           "their timing differences are not speedup claims.", ""]
    out += paired_table(rows, "agent", "Host")
    out += ["", f"[Raw samples, output bytes/statuses, fixture/harness hashes and binary digests]({link}). "
            f"Reproduce: `python3 bench/hook_config.py BASELINE CANDIDATE{' --suite skills' if skills else ''} --runs {data['runs']} --output {filename}`.", "",
            f"These checks validate {'skill lifecycle and configuration editing' if skills else 'configuration editing'}, not live host approval behavior. "
            f"No {'agent-task token' if skills else 'token'}, native-search latency, atomic-write or concurrent-edit safety claim is made. "
            "Positional hook IDs may shift on removal; stored trust records are retained unchanged.", ""]
    if extended:
        failures = sum(row[label].get("failed_invocations", 0) for row in rows for label in ("baseline", "candidate"))
        out += [f"Invocation failures: {failures}. Timeouts/launch failures retain their elapsed time and partial output sizes, "
                "fail the contract, and suppress the affected timing ratios. Version probes remain preflight checks.", ""]
    summary = latency_summary(rows)
    if entry.analysis == "thresholds" and summary:
        out += [f"Cases above the +10% median / +20% p95 investigation thresholds: **{summary['flagged']}/{len(rows)}**. "
                f"Maximum median/p95 increases: {summary['median_max']:.1f}%/{summary['p95_max']:.1f}%.", ""]
    if entry.analysis == "publication" and summary:
        changed = {"mixed_uninstall", "wrong_matcher_install", "matcherless_uninstall", "custom_matcher_uninstall", "empty_install"}
        deltas = [r["candidate"]["median_ms"] - r["baseline"]["median_ms"] for r in rows if r["case"] in changed]
        quiet = [r["candidate"]["median_ms"] - r["baseline"]["median_ms"] for r in rows if r["case"] not in changed]
        if not deltas or not quiet:
            return out
        out += [f"**Latency investigation:** {summary['flagged']}/{len(rows)} cases exceed the +10% median or +20% p95 thresholds. "
                f"Changed configurations add {min(deltas):.2f}–{max(deltas):.2f} ms at the median; "
                f"no-op/error cases change by {min(quiet):+.2f}–{max(quiet):+.2f} ms. "
                "The changed path now locks, rereads, preserves metadata, syncs a temporary file and syncs the directory; "
                "the baseline writes in place without these guarantees. This is an accepted installation/removal cost "
                "for reliability, not a speed improvement. These operations do not run during hook rewrites or searches; "
                "the separate rewrite regression measures the recurring hook path. Atomicity, conflicts and interruption "
                "are covered by deterministic Rust tests, not inferred from these timing fixtures.", ""]
    return out


def corpus_report(output_dir):
    original = "disposable-corpora-2026-09-23-darwin-arm64.json"
    filename = "disposable-corpora-review-2026-09-23-darwin-arm64.json"
    data = load_json(os.path.join(RESULTS, filename))
    if not data:
        filename = original
        data = load_json(os.path.join(RESULTS, filename))
    if not data:
        return []
    link = result_link(RESULTS, output_dir, filename)
    setup = data["snapshot_setup"]
    corpus = data["corpus_before"]
    out = ["## Disposable edit and soak corpora (2026-09-23)", "",
           f"Source snapshot: {corpus['files']:,} files, {corpus['bytes']:,} bytes; "
           f"source content/mode/mtime digest unchanged after both workloads: **{data['source_unchanged']}**. "
           f"Test binary: `{data['binary']['version']}`.", ""]
    for command in data["commands"]:
        summary = next((line for line in reversed(command["stdout"].splitlines())
                        if line.startswith(("EDITS ", "SOAK "))), "No workload summary")
        out.append(f"- {summary} (exit {command['returncode']}).")
    out += ["", f"Warm setup ({setup['runs']} runs) copied the snapshot and working tree in "
            f"**{setup['median_ms']:.1f} ms median / {setup['p95_ms']:.1f} ms p95**. "
            "This excludes index creation and cleanup; allow two corpus copies plus a private index. "
            "Copying and initialization precede the soak timer, while restores count toward it.", "",
            "These are harness-safety checks, not paired native-search performance or token measurements. "
            "Isolated configuration, fresh indexes and snapshot restores change the workload conditions; "
            "do not compare the query timings with historical in-place runs. A fixed seed does not "
            "make process scheduling or duration-limited iteration counts deterministic.", "",
            f"[Commands, raw output, setup samples and binary/corpus/harness digests]({link}).", ""]
    if filename != original:
        original_link = result_link(RESULTS, output_dir, original)
        out += [f"[Initial measurements]({original_link}) are preserved verbatim. "
                "Their edit harness abbreviated freshness diagnostics to 53 characters. "
                "The recheck preserves complete emitted diagnostic lines (including the CLI’s explicit ellipsis for long plans) "
                "and exercises collision-safe fixtures. "
                "Setup samples are retained from the initial run; the corpus-copy implementation is unchanged.", ""]
    return out


def report(args):
    path = args.out or os.path.join(ROOT, "references", "BENCH.md")
    speed_res = load_json(os.path.join(RESULTS, "speed.json"))
    oracle_res = load_json(os.path.join(RESULTS, "oracle.json"), {}) or {}
    out = ["# Benchmarks", "", "Generated by `bench/bench.py report` from `bench/results/`. Rows measured under an older protocol are marked ⚠ and left out of the means. Protocol 2 (2026-09-02) fixed the comparison so both sides do the same work. Protocol 3 (2026-09-04) added the SCIP-covered useful-line ratio next to the strict one, and changed no earlier number.", ""]
    output_dir = os.path.dirname(os.path.realpath(path))
    datasets = ReportDatasets(RESULTS)
    for entry in datasets.entries:
        datasets.get(entry)
    out += matching_report(output_dir, datasets)
    out += matching_review_report(output_dir, datasets)
    out += hook_report(output_dir, datasets)
    out += corpus_report(output_dir)
    out += hook_config_report(output_dir, datasets)
    if speed_res:
        h = speed_res["host"]
        corpora = speed_res["corpora"]
        old = {c for c, e in corpora.items() if e.get("protocol", speed_res.get("protocol", 1)) != PROTOCOL}
        stale = {c: e["stale"] for c, e in corpora.items() if e.get("stale")}
        marks = {c: (" ⚠" if c in old or c in stale else "") for c in corpora}
        if old or stale:
            out += ["> **Protocol changed on 2026-09-02** (sleep between runs so every greeg run pays the freshness check, medians, `fresh` column, speedups vs `rg -j4`). Rows marked ⚠ were measured with the old protocol or another binary and are stale: rerun pending (`bench/bench.py speed`). Their greeg columns are flattered by the 100 ms TTL (the freshness check was skipped, no `fresh` column) and they are excluded from the geometric means.", ""]
        out += ["## Speed", "", f"Host `{h['key']}` ({h['cpus']} CPUs), {speed_res['date']}. `{speed_res['greeg']}`, `{speed_res['rg']}`, `{speed_res['grep'][:40]}`, {speed_res.get('hyperfine', 'hyperfine')}. hyperfine `-N --warmup 3 --runs {speed_res['runs']}` with `--prepare '{speed_res.get('prepare', PREPARE_SLEEP)}'` before every timing run, warm page cache. Cells are **medians** with min–max in parentheses.", ""]
        wk = speed_res.get("wakeup")
        if wk:
            wake_ms = lambda x: f"{x:.1f} ms" if x is not None else "n/a"
            out += ["The `sleep` before every run buys a CPU wake-up too (idle state, frequency ramp) that the hot latency never pays. On this host, " + ", ".join(f"`{k} --version` {wake_ms(v['hot_ms'])} hot → {wake_ms(v['prepared_ms'])} after the sleep" for k, v in wk.items()) + ". That adds a few milliseconds to every cell. It barely moves the ratios on the large trees, but it accounts for 30–50 % of the small-tree greeg cells, so subtract it if you want to read those as hot latencies.", ""]
        out += ["What each column prints:", ""]
        out += [f"* `{t}`: {TOOL_SHAPE[t]}" for t in TOOLS]
        out += ["* `fresh`: the index freshness check every `greeg`/`greeg-full` run pays under this protocol (median of `--stats` samples, mode in parentheses: `fsevents` or `stat` walk); `--fresh none` skips it (JSON column `greeg-nofresh`).", "", "Match sets: `rg --json` is the reference. `greeg-full` gets verified from the text the timed command prints. The budgeted `greeg` digest can't be verified from its own output, so it's checked as `--json --budget 0` with the same query. Rows marked ⚠ in the matches column failed one of those checks. grep has no `.gitignore` support, so its count can run past rg's.", ""]
        out += ["### Index build", "", "| corpus | files | source | build (median) | index | ratio | build RSS |", "|---|---:|---:|---:|---:|---:|---:|"]
        for c, e in corpora.items():
            ix = e["index"]
            if ix.get("build_s"):
                out.append(f"| {c}{marks[c]} | {e['files']:,} | {e['bytes']/1e6:.0f} MB | {fmt_ms(ix['build_s'])} | {ix.get('size') or '–'} | {ix.get('ratio') or '–'}× | {mb(ix.get('rss_mb'))} |")
        out += ["", "### Search latency (warm)", ""]
        out += ["| corpus | family | pattern | " + " | ".join(TOOLS) + " | fresh | matches |", "|---|---|---|" + "---:|" * (len(TOOLS) + 1) + "---:|"]
        for c, e in corpora.items():
            for fam in FAMILIES:
                q = e["queries"].get(fam)
                if not q:
                    continue
                cells = [cell(q["tools"].get(t, {}).get("warm")) for t in TOOLS]
                fr = q.get("fresh")
                m = q["matches"]
                ok = m.get("rg_eq_greeg", True) and m.get("rg_eq_greeg_full_text", True)
                out.append(f"| {c}{marks[c]} | {fam} | `{q['pattern']}` | " + " | ".join(cells) + f" | {(fmt_ms(fr['median']) + ' (' + fr['mode'] + ')') if fr else '–'} | {m['rg']:,}{'' if ok else ' ⚠'} |")
        file_rows = [c for c, e in corpora.items() if e.get("kind") == "file"]
        if file_rows:
            out += ["", f"`{', '.join(file_rows)}` is a single file, not a repository: only the scan tools are timed (greeg does not index it), and `greeg-full`/`greeg`/`fresh` are `–`."]
        cold_rows = [(c, fam, q) for c, e in corpora.items() for fam, q in e["queries"].items() if fam in FAMILIES and any("cold" in q["tools"].get(t, {}) for t in TOOLS)]
        if cold_rows:
            out += ["", "### Search latency (cold page cache)", "", f"Prepare: `{PURGE_CMD}` before every run.", "", "| corpus | family | " + " | ".join(TOOLS) + " |", "|---|---|" + "---:|" * len(TOOLS)]
            for c, fam, q in cold_rows:
                out.append(f"| {c}{marks[c]} | {fam} | " + " | ".join(cell(q["tools"].get(t, {}).get("cold")) for t in TOOLS) + " |")
        out += ["", "### Verbs", "", "| corpus | verb | name | greeg (median) |", "|---|---|---|---:|"]
        for c, e in corpora.items():
            for v in VERBS:
                q = e["queries"].get(v)
                if q and q["tools"]["greeg"]["warm"]:
                    out.append(f"| {c}{marks[c]} | {v} | `{q['pattern']}` | {cell(q['tools']['greeg']['warm'])} |")
        gm = speed_res.get("geomean") or {"rg": speed_res.get("geomean_vs_rg", {})}
        current = [c for c in corpora if c not in old and c not in stale]
        out += ["", "### Geometric-mean speed relative to `rg -j4` (headline; > 1 = faster)", "", f"Medians, over the {len(current)} current-protocol corp{'us' if len(current) == 1 else 'ora'} ({', '.join(current) or 'none'}). `rg -j4` is the best-configured rg on macOS, where rg's default thread count spends most of each run inside the kernel. On Linux the two rg columns sit close together.", ""]
        out += ["| family | " + " | ".join(TOOLS) + " | greeg-nofresh |", "|---|" + "---:|" * (len(TOOLS) + 1)]
        for fam, d in gm.get("rg-j4", {}).items():
            out.append(f"| {fam} | " + " | ".join(f"{d[t]:.2f}×" if t in d else "–" for t in TOOLS + ["greeg-nofresh"]) + " |")
        if not gm.get("rg-j4"):
            out.append("| – | " + " | ".join("–" for _ in TOOLS + ["greeg-nofresh"]) + " |")
        out += ["", "### Geometric-mean speed relative to default `rg` (secondary)", "", "| family | " + " | ".join(TOOLS) + " |", "|---|" + "---:|" * len(TOOLS)]
        for fam, d in gm.get("rg", {}).items():
            out.append(f"| {fam} | " + " | ".join(f"{d[t]:.2f}×" if t in d else "–" for t in TOOLS) + " |")
        out += ["", "### Peak RSS (ident query)", "", "| corpus | rg | greeg-scan | greeg-full | greeg |", "|---|---:|---:|---:|---:|"]
        for c, e in corpora.items():
            q = e["queries"].get("ident")
            if q:
                out.append(f"| {c}{marks[c]} | " + " | ".join(mb(q["tools"].get(t, {}).get("rss_mb")) for t in ("rg", "greeg-scan", "greeg-full", "greeg")) + " |")
        if stale:
            out += ["", "Stale rows:", ""] + [f"* {c}: {why}" for c, why in stale.items()]
        out.append("")
    if oracle_res:
        toks = {r.get("tokenizer", "unknown (results predate the tokenizer record)") for r in oracle_res.values()}
        out += ["## Quality (SCIP oracle)", "", "Ground truth: SCIP occurrences from `rust-analyzer scip`, `scip-python`, `scip-typescript`. The indexer name and version are recorded per corpus below.", "",
                f"Names are sampled from symbols defined in the repository, stratified by ambiguity (how many definitions share that name). Within a bucket, names are drawn with probability ∝ log(1 + reference count) and need at least {MIN_REFS} references, so the sample leans toward the names an agent actually asks about. Zero-reference names get their own bucket, reported separately.", "",
                "Acc@k means a true definition is among the first k locations the tool prints. The columns: `greeg def NAME`; `rg -nw NAME` (thread order, first lines); `rg-def`, which is `rg -n --sort path` with the language's definition regex for NAME (what an agent types when it wants the definition); and `grep -rnw NAME`. Brackets are 95 % bootstrap intervals of Acc@1 (n ≈ 75 → ±10 pp).", "",
                "Reference recall: SCIP reference occurrences found by `greeg refs NAME --budget 0`. Classification: `greeg -w NAME --json` hit kinds against SCIP roles on the same line, from stored spans and with `--precise`.", "",
                f"Context@500 reads the first 500 lines each tool prints for the bare name. Useful = distinct lines carrying a SCIP occurrence of the name, over the lines that name a location. Summary lines (header, facets, file headers, `+N more`, footer) are neutral and leave the denominator. Tokens are {' / '.join(sorted(toks))} counts.", "",
                "**useful lines, SCIP-covered** (protocol 3, reported beside the strict column) narrows that denominator to lines in files the SCIP index actually holds. The oracle can't adjudicate a line in `lib.dom.d.ts`, `tests/baselines/**` or a fixture `.js`, and scoring those as not-useful charges every tool for reading files the ground truth skipped. The classification metric already drops them for the same reason.", "",
                "Treat it as the stricter measure of ranking. Un-adjudicable lines still cost tokens, and the token columns keep counting them.", ""]
        runs = "; ".join(f"`{c}` {r.get('date', '?')[:10]} with {r.get('greeg', '?')}" for c, r in oracle_res.items())
        out += [f"Runs: {runs}. A corpus is only re-measured when it's re-run, so rows carrying different dates can straddle a change to the shaper. Compare the answer shape across corpora within one date, never across two.", ""]
        out += ["| corpus | indexer | n | greeg Acc@1/5/10 | rg -nw Acc@1/5/10 | rg-def Acc@1/5/10 | grep Acc@1/5/10 | ref recall | def precision (spans / precise) | noncode precision | SCIP density |", "|---|---|---:|---|---|---|---|---:|---|---:|---:|"]
        for c, r in oracle_res.items():
            s = r["summary"]
            ti = r.get("scip_tool") or {}
            indexer = f"{ti.get('tool') or r.get('scip', '?')} {ti.get('version') or ''}".strip() if ti else (r.get("scip_tool_cli") or r.get("scip", "?"))
            out.append(f"| {c} | {indexer} | {r['sampled']} | {accs(s, 'greeg')} | {accs(s, 'rg')} | {accs(s, 'rgdef')} | {accs(s, 'grep')} | {pct(s['ref_recall'])} | {pct(s['cls_spans']['def_precision'])} / {pct(s['cls_precise']['def_precision'])} | {pct(s['cls_spans']['noncode_precision'])} | {pct(s['cls_spans']['code_on_scip'])} |")
        out += ["", "**SCIP density** is the share of greeg's code hits on the sampled names that the indexer marks as an occurrence at all. It's a measure of what the ground truth can see, and it swings far more per indexer than per tool (scip-typescript ≈ 86 %, rust-analyzer ≈ 55 %, scip-python ≈ 43 %). So the SCIP-covered useful-line ratio below is a per-indexer number. Compare it within a corpus, never across.", ""]
        out += ["", "| corpus | ambiguity 1 | ambiguity 2–5 | ambiguity 6+ | zero-reference names: greeg / rg -nw / rg-def Acc@1/5/10 |", "|---|---|---|---|---|"]
        for c, r in oracle_res.items():
            s = r["summary"]
            z = r.get("summary_zero_ref")
            out.append(f"| {c} | " + " | ".join(f"{'/'.join(pct(x) for x in s[f'greeg_acc_bucket_{b}'])} (n={s[f'n_bucket_{b}']})" if f"greeg_acc_bucket_{b}" in s else "–" for b in ("1", "2-5", "6+")) + f" | {(accs(z, 'greeg') + ' / ' + accs(z, 'rg') + ' / ' + accs(z, 'rgdef') + f' (n={z['n']})') if z else '–'} |")
        out += ["", "| corpus | tool | useful lines | useful lines, SCIP-covered [95 % CI] | neutral lines | coverage | tokens/query mean | tokens/query median [95 % CI] | distinct true locations per 1k tokens | tokens to first def (median) | def reached | tokens until all defs covered (median) | all defs reached |", "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
        for c, r in oracle_res.items():
            s = r["summary"]
            for t in CTX_TOOLS:
                if f"{t}_tokens" not in s:
                    continue
                out.append(f"| {c} | {t} | {pct(s[f'{t}_ctx_useful_ratio'])} | {pct(s.get(f'{t}_ctx_useful_ratio_covered'))}{ci_pp(s.get(f'{t}_ctx_useful_ratio_covered_ci'))} | {pct(s.get(f'{t}_ctx_neutral_ratio'))} | {pct(s[f'{t}_ctx_coverage'])} | {round(s[f'{t}_tokens'] or 0):,} | {round(s[f'{t}_tokens_median']) if s.get(f'{t}_tokens_median') else '–'}{ci_n(s.get(f'{t}_tokens_median_ci'))} | {s[f'{t}_ctx_useful_per_ktok']:.1f} | {round(s[f'{t}_tokens_to_def']) if s[f'{t}_tokens_to_def'] else '–'} | {pct(s[f'{t}_def_reached'])} | {round(s[f'{t}_tokens_to_all_defs']) if s.get(f'{t}_tokens_to_all_defs') else '–'} | {pct(s.get(f'{t}_all_defs_reached'))} |")
        older = [c for c, r in oracle_res.items() if "tokenizer" not in r]
        if older:
            out += ["", f"⚠ {', '.join(older)}: measured before protocol 2 (uniform sampling over definitions including zero-reference names, useful-line ratio with layout lines in the denominator and duplicate locations counted, no rg-def column, no intervals); rerun pending (`bench/bench.py oracle {' '.join(older)}`)."]
        out.append("")
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
    s.add_argument("--cold", action="store_true", help="also measure with a purged page cache (needs passwordless sudo; skipped with a printed reason otherwise)")
    s.add_argument("--no-splice", action="store_true", help="with --corpora: drop rows of other corpora from the results instead of keeping (and marking) them")
    o = sub.add_parser("oracle")
    o.add_argument("corpora", nargs="+")
    o.add_argument("--greeg", default=os.path.join(ROOT, "target", "release", "greeg"))
    o.add_argument("--scip", help="directory holding <corpus>/index.scip (default: sibling `scip` of the corpus cache)")
    o.add_argument("--per-bucket", type=int, default=25)
    o.add_argument("--zero-ref", type=int, default=10, help="size of the separately reported zero-reference bucket")
    o.add_argument("--seed", type=int, default=42)
    g = sub.add_parser("gate")
    g.add_argument("what", choices=["speed", "kernels"])
    g.add_argument("--tolerance", type=float, default=10.0, help="percent slower than the baseline that fails")
    g.add_argument("--save", action="store_true", help="overwrite the baseline for this host")
    g.add_argument("--require-baseline", action="store_true", help="fail when no baseline exists for this host (reference machine)")
    g.add_argument("--record-only", action="store_true", help="print the current numbers and exit 0 without comparing or creating a baseline (hosted runners)")
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
        try:
            report(a)
        except ReportDataError as error:
            sys.exit(f"report error: {error}")


if __name__ == "__main__":
    main()
