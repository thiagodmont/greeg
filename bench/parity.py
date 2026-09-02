#!/usr/bin/env python3
"""Parity check: (path, line) sets from `greeg --json --budget 0` must equal `rg --json`."""
import json, subprocess, sys, os, time
corpora = sys.argv[1]
greeg = sys.argv[2]
cases = {
    "tokio": [["fn poll_read"], ["-w", "Waker"], ["-i", "jOinHandle"], ["spawn_blocking", "-t", "rust"], ["-F", "Pin<&mut Self>"], [r"\bpoll_\w+\("]],
    "django": [["get_queryset"], ["-w", "request"], ["def get_queryset"], ["-g", "*.py", "csrf_token"], ["HttpResponseRedirect", "-t", "py"]],
    "ktor": [["respond"], ["fun respond"], ["-w", "ApplicationCall"], ["ContentNegotiation", "-g", "!**/test/**"]],
    "TypeScript": [["createSourceFile"], ["checkExpression"], ["ParseFlags"]],
    "rust": [["mir_borrowck"], ["-w", "HirId"], ["LocalDefId", "-t", "rust"]],
}
def pairs(out):
    s = set()
    for line in out.splitlines():
        if not line.startswith('{'):
            continue
        j = json.loads(line)
        if j.get("type") != "match":
            continue
        d = j["data"]
        s.add((d["path"]["text"].removeprefix("./"), d["line_number"]))
    return s
ok = True
print(f"{'corpus':11} {'query':34} {'rg':>6} {'greeg':>6} {'missing':>7} {'extra':>5}  status")
for c, qs in cases.items():
    cwd = os.path.join(corpora, c)
    for q in qs:
        rg = subprocess.run(["rg", "--json", "-n", *q, "."], cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL).stdout.decode(errors="replace")
        gg = subprocess.run([greeg, "--json", "--budget", "0", "--no-ladder", "--max-columns", "0", *q, "."], cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL).stdout.decode(errors="replace")
        a, b = pairs(rg), pairs(gg)
        miss, extra = a - b, b - a
        st = "OK" if not miss and not extra else "DIFF"
        if st == "DIFF":
            ok = False
            for x in list(miss)[:3]: st += f" missing {x}"
            for x in list(extra)[:3]: st += f" extra {x}"
        print(f"{c:11} {' '.join(q):34} {len(a):6} {len(b):6} {len(miss):7} {len(extra):5}  {st}")
print("PARITY", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
