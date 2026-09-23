#!/usr/bin/env python3
"""Edit-burst correctness: mutate a corpus, compare greeg (index) with rg after each step.
Usage: edits.py CORPUS_DIR GREEG_BIN   (mutates only a disposable snapshot)"""
import json, os, random, sys, time, shutil
from corpus import Corpus, executable, interrupted_cleanup
def run(args, **kw):
    if args[0] == "rg":
        args = [ripgrep, *args[1:]]
    if args[0] == greeg:
        args = [greeg, "--no-session", *args[1:]]
    return corpus.run(args, **kw)
def pairs(out):
    s=set()
    for line in out.splitlines():
        if not line.startswith('{'): continue
        j=json.loads(line)
        if j.get('type')=='match': d=j['data']; s.add((d['path']['text'].removeprefix('./'), d['line_number']))
    return s
def compare(q, label):
    time.sleep(0.12)  # past the 100 ms freshness TTL
    oracle = run(["rg","--json","-n",*q,"."])
    rg = pairs(oracle.stdout.decode(errors="replace"))
    t=time.perf_counter()
    p = run([greeg,"--json","--budget","0","--no-ladder","--max-columns","0","--stats",*q,"."])
    ms=(time.perf_counter()-t)*1e3
    gg = pairs(p.stdout.decode(errors="replace"))
    src = "index" if "greeg: index" in p.stderr.decode() else "scan"
    fresh = [l for l in p.stderr.decode().splitlines() if "fresh" in l]
    ok = rg == gg and oracle.returncode in (0, 1) and p.returncode == oracle.returncode
    print(f"  {label:28} {' '.join(q):22} rg={len(rg):5} greeg={len(gg):5} {'OK ' if ok else 'DIFF'} via {src} {ms:6.1f} ms  {fresh[0][7:60] if fresh else ''}")
    if not ok:
        for x in list(rg-gg)[:3]: print("     missing", x)
        for x in list(gg-rg)[:3]: print("     extra", x)
    return ok
def rows(out):
    for line in out.decode(errors="replace").splitlines():
        if line.startswith('{'):
            try: yield json.loads(line)
            except ValueError: pass
def defs(out):
    for j in rows(out):
        if j.get("type") == "def": yield j
def graph_check():
    """Pick an imported file with symbols from `map`, find a file that imports it
    (reach 1.0 from `def --from`), edit both, and check the reach survives the deltas."""
    # the edit burst above spawned a rebuild; `map` needs its phase 2
    for _ in range(300):
        st = run([greeg,"index","--status"]).stdout.decode(errors="replace")
        if '"phase2": true' in st: break
        time.sleep(0.1)
    time.sleep(0.12)
    p = run([greeg,"--json","--fresh","stat","map","."])
    ranked = [j["data"] for j in rows(p.stdout) if j.get("type")=="file"]
    ranked = [f for f in ranked if f.get("imported_by",0) > 0 and f.get("top")]
    ranked.sort(key=lambda f: -f["imported_by"])
    def reach_of(name, origin, target):
        d = run([greeg,"--json","--fresh","none","def",name,"--from",origin])
        for e in defs(d.stdout):
            if e.get("path") == target: return e.get("reach")
        return None
    origin = target = name = None
    for f in ranked[:6]:
        # importers mention the module name: `foo` for foo.py, the directory for mod.rs / index.ts
        base = os.path.basename(f["path"]); stem = base.split(".")[0]
        if stem in ("mod", "index", "lib", "main", "__init__"): stem = os.path.basename(os.path.dirname(f["path"]))
        q = run([greeg,"--fresh","none","-l","-w",stem,"."])
        cands = [l.strip() for l in q.stdout.decode(errors="replace").splitlines() if l.strip() and l.strip() != f["path"]]
        for cand in cands[:60]:
            for _, nm in f["top"][:2]:
                if reach_of(nm, cand, f["path"]) == 1.0:
                    origin, target, name = cand, f["path"], nm; break
            if origin: break
        if origin: break
    if not origin:
        print("  graph check skipped: no (importer, imported) pair with reach 1.0 found"); return True
    ok = True
    for label, edit in (("edited importer", origin), ("edited both", target)):
        with open(corpus.path(edit),"a") as fh: fh.write("\n# ZZEDITMARK graph\n")
        time.sleep(0.12)
        t=time.perf_counter()
        d = run([greeg,"--json","--stats","--fresh","stat","def",name,"--from",origin])
        ms=(time.perf_counter()-t)*1e3
        reach = None; src = "?"
        for j in rows(d.stdout):
            if j.get("type") == "def" and j.get("path") == target: reach = j.get("reach")
            if j.get("type") == "footer": src = j["data"].get("source", "?")
        good = reach == 1.0
        ok &= good
        print(f"  {label:28} def {name} --from {origin[-44:]:44} reach={reach} {'OK ' if good else 'DIFF'} via {src} {ms:6.1f} ms")
    return ok
def exercise():
    listing = run(["rg", "--files", "-0", "-t", "py", "-t", "rust", "-t", "kotlin", "-t", "ts", "-t", "js", "."])
    if listing.returncode not in (0, 1):
        raise RuntimeError(listing.stderr.decode(errors="replace"))
    files = [os.fsdecode(f) for f in listing.stdout.split(b"\0") if f]
    files = corpus.regular_files([f.removeprefix("./") for f in files])
    files.sort()
    random.shuffle(files)
    ok = True
    queries = [["ZZEDITMARK"], ["-w","self"], ["fn "]] if any(f.endswith(".rs") for f in files) else [["ZZEDITMARK"], ["-w","self"], ["def "]]
    print("== baseline")
    run([greeg,"index","--root",".","--quiet"], check=True)
    for q in queries: ok &= compare(q, "baseline")
    print(f"== {min(200, len(files))} files modified")
    for f in files[:200]:
        with open(corpus.path(f),"a") as fh: fh.write("\n# ZZEDITMARK edit\n")
    for q in queries: ok &= compare(q, "after 200 edits")
    print("== 1 new file, 1 new dir with 3 files, 1 deleted, 1 dir renamed")
    d = (os.path.dirname(files[0]) or ".") if files else "."
    corpus.path(os.path.join(d,"zz_new_file.py")).write_text("ZZEDITMARK = 1\n")
    os.makedirs(corpus.path("zz_new_dir/sub"), exist_ok=True)
    for i in range(3):
        corpus.path(f"zz_new_dir/sub/f{i}.rs").write_text(f"fn zz{i}() {{ let ZZEDITMARK = {i}; }}\n")
    if files:
        corpus.path(files[min(200, len(files) - 1)]).unlink()
    # rename a directory that has files
    dirs = sorted({os.path.dirname(f) for f in files[201:400] if "/" in f and os.path.dirname(f)})
    rn = None
    for cand in dirs:
        if os.path.isdir(corpus.path(cand)) and cand.count("/")>=1:
            rn=cand; break
    if rn:
        shutil.move(corpus.path(rn), corpus.path(rn+"_renamed"))
    for q in queries: ok &= compare(q, "after adds/deletes/rename")
    print("== graph after edits: an edited importer keeps reach 1.0 to what it imports")
    if files:
        ok &= graph_check()
    print("== restore initial snapshot")
    corpus.restore()
    for q in queries: ok &= compare(q, "after restore")
    print("EDITS", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main():
    global corpus, greeg, ripgrep
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    greeg = executable(sys.argv[2])
    ripgrep = executable("rg")
    random.seed(7)
    with interrupted_cleanup(), Corpus(sys.argv[1]) as corpus:
        print(f"Disposable corpus: {corpus.root}", flush=True)
        return exercise()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
