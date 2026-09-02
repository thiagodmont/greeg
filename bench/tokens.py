#!/usr/bin/env python3
"""Calibrate greeg's token estimate against o200k_base. Usage: tokens.py CORPORA_DIR GREEG_BIN"""
import re, subprocess, sys, os, statistics, tiktoken
corpora, greeg = sys.argv[1], sys.argv[2]
enc = tiktoken.get_encoding("o200k_base")
cases = {
    "tokio": [["fn poll_read"], ["-w", "Waker"], ["spawn_blocking"], ["JoinHandle", "--mode", "outline"], ["Waker", "--mode", "files"]],
    "django": [["get_queryset"], ["-w", "request"], ["HttpResponseRedirect"], ["csrf_token", "--budget", "800"], ["ModelAdmin", "--mode", "block"]],
    "ktor": [["respond"], ["fun respond"], ["ApplicationCall", "--budget", "4000"], ["ContentNegotiation", "--mode", "outline"]],
    "TypeScript": [["createSourceFile"], ["checkExpression"], ["-w", "node"]],
    "rust": [["mir_borrowck"], ["-w", "HirId"], ["TyCtxt", "--budget", "1000"]],
}
ratios = []
print(f"{'corpus':11} {'query':32} {'est':>6} {'o200k':>6} {'ratio':>6} {'bytes/tok':>9}")
for c, qs in cases.items():
    cwd = os.path.join(corpora, c)
    for q in qs:
        out = subprocess.run([greeg, *q, "."], cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL).stdout.decode(errors="replace")
        m = re.search(r"~(\d+) tokens", out)
        if not m:
            continue
        est = int(m.group(1))
        actual = len(enc.encode(out))
        ratios.append(est / actual)
        print(f"{c:11} {' '.join(q):32} {est:6} {actual:6} {est/actual:6.2f} {len(out.encode())/actual:9.2f}")
print(f"est/actual: median {statistics.median(ratios):.2f}  mean {statistics.mean(ratios):.2f}  min {min(ratios):.2f}  max {max(ratios):.2f}")
