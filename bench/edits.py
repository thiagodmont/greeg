#!/usr/bin/env python3
"""Edit-burst correctness: mutate a corpus, compare greeg (index) with rg after each step.
Usage: edits.py CORPUS_DIR GREEG_BIN   (corpus must be a git checkout; it is restored at the end)"""
import json, os, random, subprocess, sys, time, shutil
cwd, greeg = sys.argv[1], sys.argv[2]
random.seed(7)
NO_STATS = {**os.environ, "GREEG_STATS": "0"}  # edit bursts are not usage: keep them out of `greeg stats`
def run(args, **kw):
    return subprocess.run(args, cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL, env=NO_STATS, **kw)
def pairs(out):
    s=set()
    for line in out.splitlines():
        if not line.startswith('{'): continue
        j=json.loads(line)
        if j.get('type')=='match': d=j['data']; s.add((d['path']['text'].removeprefix('./'), d['line_number']))
    return s
def compare(q, label):
    time.sleep(0.12)  # past the 100 ms freshness TTL
    rg = pairs(run(["rg","--json","-n",*q,"."]).stdout.decode(errors="replace"))
    t=time.perf_counter()
    p = run([greeg,"--json","--budget","0","--no-ladder","--max-columns","0","--stats",*q,"."])
    ms=(time.perf_counter()-t)*1e3
    gg = pairs(p.stdout.decode(errors="replace"))
    src = "index" if "greeg: index" in p.stderr.decode() else "scan"
    fresh = [l for l in p.stderr.decode().splitlines() if "fresh" in l]
    ok = rg==gg
    print(f"  {label:28} {' '.join(q):22} rg={len(rg):5} greeg={len(gg):5} {'OK ' if ok else 'DIFF'} via {src} {ms:6.1f} ms  {fresh[0][7:60] if fresh else ''}")
    if not ok:
        for x in list(rg-gg)[:3]: print("     missing", x)
        for x in list(gg-rg)[:3]: print("     extra", x)
    return ok
files = [l for l in run(["rg","--files","-t","py","-t","rust","-t","kotlin","-t","ts","-t","js","."]).stdout.decode().splitlines()]
files = [f.removeprefix("./") for f in files]
random.shuffle(files)
ok = True
queries = [["ZZEDITMARK"], ["-w","self"], ["fn "]] if any(f.endswith(".rs") for f in files) else [["ZZEDITMARK"], ["-w","self"], ["def "]]
print("== baseline"); run([greeg,"index","--root",".","--quiet"]); 
for q in queries: ok &= compare(q, "baseline")
print("== 200 files modified")
for f in files[:200]:
    with open(os.path.join(cwd,f),"a") as fh: fh.write("\n# ZZEDITMARK edit\n")
for q in queries: ok &= compare(q, "after 200 edits")
print("== 1 new file, 1 new dir with 3 files, 1 deleted, 1 dir renamed")
d = os.path.dirname(files[0]) or "."
open(os.path.join(cwd,d,"zz_new_file.py"),"w").write("ZZEDITMARK = 1\n")
os.makedirs(os.path.join(cwd,"zz_new_dir/sub"), exist_ok=True)
for i in range(3): open(os.path.join(cwd,f"zz_new_dir/sub/f{i}.rs"),"w").write(f"fn zz{i}() {{ let ZZEDITMARK = {i}; }}\n")
os.remove(os.path.join(cwd,files[200]))
# rename a directory that has files
dirs = sorted({os.path.dirname(f) for f in files[201:400] if "/" in f and os.path.dirname(f)})
rn = None
for cand in dirs:
    if os.path.isdir(os.path.join(cwd,cand)) and cand.count("/")>=1:
        rn=cand; break
if rn:
    shutil.move(os.path.join(cwd,rn), os.path.join(cwd,rn+"_renamed"))
for q in queries: ok &= compare(q, "after adds/deletes/rename")
print("== graph after edits: an edited importer keeps reach 1.0 to what it imports (DESIGN.md §4.3)")
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
        with open(os.path.join(cwd,edit),"a") as fh: fh.write("\n# ZZEDITMARK graph\n")
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
ok &= graph_check()
print("== restore (git checkout: branch-switch-like burst)")
run(["git","checkout","-q","--","."]); run(["git","clean","-qfd"])
for q in queries: ok &= compare(q, "after restore")
print("EDITS", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
