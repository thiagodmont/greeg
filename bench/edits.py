#!/usr/bin/env python3
"""Edit-burst correctness: mutate a corpus, compare greeg (index) with rg after each step.
Usage: edits.py CORPUS_DIR GREEG_BIN   (corpus must be a git checkout; it is restored at the end)"""
import json, os, random, subprocess, sys, time, shutil
cwd, greeg = sys.argv[1], sys.argv[2]
random.seed(7)
def run(args, **kw):
    return subprocess.run(args, cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL, **kw)
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
print("== restore (git checkout: branch-switch-like burst)")
run(["git","checkout","-q","--","."]); run(["git","clean","-qfd"])
for q in queries: ok &= compare(q, "after restore")
print("EDITS", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
