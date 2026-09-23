#!/usr/bin/env python3
"""Soak test: randomized queries and edits on corpora,
greeg vs rg, for a wall-clock duration.

    python3 bench/soak.py MINUTES GREEG CORPUS [CORPUS...]

Every iteration picks a corpus and a query from a family (identifiers from
the index's symbol names, words, phrases, regexes, with random -w/-i/-t/-g
flags), compares the (path, line) sets of `rg --json` and
`greeg --json --budget 0 --no-ladder --max-columns 0`, and runs one random verb
to catch crashes (exit code 2). Every 20 iterations it applies a burst of
edits (modify, create, delete, rename) in disposable snapshots. Source corpora
are read-only inputs; restores return to their initial snapshot contents.
Prints a summary and exits 1 on any mismatch or crash.
"""
import argparse, json, os, random, sys, time, math
from contextlib import ExitStack
from corpus import Corpus, executable, interrupted_cleanup

def run(args, cwd, timeout=120):
    if args[0] == "rg":
        args = [ripgrep, *args[1:]]
    return owned[cwd].run(args, timeout=timeout)

def rg_lines(pattern, flags, cwd):
    r = run(["rg", "--json", "-n"] + flags + ["-e", pattern, "."], cwd)
    out = set()
    for line in r.stdout.decode("utf8", "replace").splitlines():
        try:
            j = json.loads(line)
        except ValueError:
            continue
        if j.get("type") == "match":
            out.add((j["data"]["path"]["text"].removeprefix("./"), j["data"]["line_number"]))
    return out, r.returncode

def greeg_lines(pattern, flags, cwd):
    r = run([greeg, "--json", "--budget", "0", "--no-ladder", "--max-columns", "0", "--no-session"] + flags + ["-e", pattern, "."], cwd)
    out = set()
    for line in r.stdout.decode("utf8", "replace").splitlines():
        try:
            j = json.loads(line)
        except ValueError:
            continue
        if j.get("type") == "match":
            out.add((j["data"]["path"]["text"].removeprefix("./"), j["data"]["line_number"]))
    return out, r.returncode, r.stderr.decode("utf8", "replace")

def names_of(cwd):
    r = run([greeg, "map", "--json", "--budget", "0", "--no-session"], cwd)
    if r.returncode not in (0, 1):
        raise RuntimeError(r.stderr.decode(errors="replace"))
    names = []
    for line in r.stdout.decode("utf8", "replace").splitlines():
        try:
            j = json.loads(line)
        except ValueError:
            continue
        if j.get("type") == "file":
            names += [n for _, n in j["data"]["top"]]
    return sorted(set(names)) or ["main", "test", "config"]

def source_files(cwd, n=400):
    r = run(["rg", "--files", "-0", "-t", "py", "-t", "rust", "-t", "ts", "-t", "js", "-t", "kotlin"], cwd)
    if r.returncode not in (0, 1):
        raise RuntimeError(r.stderr.decode(errors="replace"))
    files = [os.fsdecode(f) for f in r.stdout.split(b"\0") if f]
    files.sort()
    random.shuffle(files)
    return owned[cwd].regular_files(files)[:n]

WORDS = ["self", "for", "node", "request", "error", "Result", "Option", "class", "def", "impl", "return", "async", "await", "import", "test", "TODO", "fixme", "http", "json", "id"]
REGEXES = [r"fn \w+_test", r"def test_\w+", r"class \w+\(", r"impl<[^>]+> \w+", r"\bawait\s+\w+\(", r"[A-Z]{3,}_[A-Z_]+", r"pub\s+fn\s+\w+", r"import\s+\{[^}]+\}", r"fun \w+\(", r"val \w+ =", r"\d{3,}", r"TODO|FIXME|XXX"]

def random_query(names):
    fam = random.choice(["ident", "ident", "ident", "word", "phrase", "regex"])
    flags = []
    if fam == "ident":
        pat = random.choice(names)
        if random.random() < 0.4:
            flags.append("-w")
        if random.random() < 0.2:
            flags.append("-i")
    elif fam == "word":
        pat = random.choice(WORDS)
        flags.append("-w") if random.random() < 0.7 else None
    elif fam == "phrase":
        pat = random.choice(["fn new", "def __init__", "class Meta", "import os", "use std", "async fn", "return None", "impl Default", "fun main", "const val"])
        flags.append("-F")
    else:
        pat = random.choice(REGEXES)
    if random.random() < 0.25:
        flags += ["-t", random.choice(["py", "rust", "ts", "js", "kotlin"])]
    if random.random() < 0.15:
        flags += ["-g", random.choice(["*.rs", "*.py", "src/**", "!*test*", "*.kt"])]
    return pat, flags

VERBS = ["def", "refs", "callers", "impls", "impact"]

edited = {}  # cwd -> list of (kind, path)

def edit_burst(cwd, files):
    ops = []
    for f in random.sample(files, min(12, len(files))):
        kind = random.choice(["modify", "modify", "modify", "delete", "rename", "create"])
        p = owned[cwd].path(f)
        try:
            if kind == "modify" and os.path.isfile(p):
                with open(p, "a") as fh:
                    fh.write("\n// soak edit ZZSOAK%d fn soak_marker_%d() {}\n" % (random.randint(0, 9999), random.randint(0, 9999)))
                ops.append(("modify", f))
            elif kind == "delete" and os.path.isfile(p):
                os.remove(p)
                ops.append(("delete", f))
            elif kind == "rename" and os.path.isfile(p):
                os.rename(p, owned[cwd].path(f + ".soak"))
                ops.append(("rename", f))
            elif kind == "create":
                d = os.path.dirname(p)
                np = owned[cwd].path(os.path.relpath(os.path.join(d, "soak_new_%d.rs" % random.randint(0, 99999)), cwd))
                with open(np, "w") as fh:
                    fh.write("pub fn soak_created_%d() { let request = 1; }\n" % random.randint(0, 9999))
                ops.append(("create", os.path.relpath(np, cwd)))
        except OSError:
            pass
    edited.setdefault(cwd, []).extend(ops)

def restore(cwd):
    owned[cwd].restore()
    edited[cwd] = []

def exercise(minutes, corpora):
    for cwd in corpora:
        owned[cwd].run([greeg, "index", "--root", ".", "--quiet", "--no-session"],
                       check=True, timeout=600)
    names = {c: names_of(c) for c in corpora}
    files = {c: source_files(c) for c in corpora}
    it = mismatches = crashes = 0
    worst_ms = 0.0
    deadline = time.monotonic() + minutes * 60
    while time.monotonic() < deadline:
        cwd = random.choice(corpora)
        pat, flags = random_query(names[cwd])
        t = time.perf_counter()
        g, gcode, gerr = greeg_lines(pat, flags, cwd)
        ms = (time.perf_counter() - t) * 1000
        worst_ms = max(worst_ms, ms)
        if gcode not in (0, 1):
            crashes += 1
            print("CRASH  %s  %r %r\n  %s" % (os.path.basename(cwd), pat, flags, gerr.strip()[:300]))
        r, rcode = rg_lines(pat, flags, cwd)
        if rcode not in (0, 1):
            pass  # rg rejected the pattern; skip comparison
        elif g != r or gcode != rcode:
            mismatches += 1
            only_r = sorted(r - g)[:3]
            only_g = sorted(g - r)[:3]
            print("MISMATCH  %s  %r %r  rg=%d greeg=%d  only_rg=%s only_greeg=%s" % (os.path.basename(cwd), pat, flags, len(r), len(g), only_r, only_g))
        if random.random() < 0.3:
            verb = random.choice(VERBS)
            v = run([greeg, verb, random.choice(names[cwd]), "--budget", "600", "--no-session"], cwd)
            if v.returncode not in (0, 1):
                crashes += 1
                print("CRASH  %s  %s %s\n  %s" % (os.path.basename(cwd), verb, pat, v.stderr.decode("utf8", "replace").strip()[:300]))
        it += 1
        if it % 20 == 0:
            if edited.get(cwd):
                restore(cwd)
            else:
                edit_burst(cwd, files[cwd])
            time.sleep(0.12)  # freshness TTL
        if it % 50 == 0:
            print("… %d iterations, %d mismatches, %d crashes, worst %.0f ms" % (it, mismatches, crashes, worst_ms), flush=True)
    print("SOAK %s: %d iterations in %.1f min, %d mismatches, %d crashes, worst query %.0f ms" % ("PASS" if mismatches == 0 and crashes == 0 else "FAIL", it, minutes, mismatches, crashes, worst_ms))
    return 0 if mismatches == 0 and crashes == 0 else 1


def main():
    global owned, greeg, ripgrep
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("minutes", type=float)
    parser.add_argument("greeg")
    parser.add_argument("corpora", nargs="+")
    parser.add_argument("--seed", type=int, default=int(time.time()))
    args = parser.parse_args()
    if not math.isfinite(args.minutes) or args.minutes < 0:
        parser.error("minutes must be finite and nonnegative")
    greeg = executable(args.greeg)
    ripgrep = executable("rg")
    sources = [Corpus.validate_source(source) for source in args.corpora]
    random.seed(args.seed)
    print(f"Random seed: {args.seed}", flush=True)
    with interrupted_cleanup(), ExitStack() as stack:
        owned = {}
        for source in sources:
            corpus = stack.enter_context(Corpus(source))
            owned[str(corpus.root)] = corpus
            print(f"Disposable corpus: {corpus.root} (source: {corpus.source})", flush=True)
        return exercise(args.minutes, list(owned))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
