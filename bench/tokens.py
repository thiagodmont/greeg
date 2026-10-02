#!/usr/bin/env python3
"""Fit greeg's token estimate (crates/greeg-query/src/tokens.rs) against o200k_base.

  tokens.py collect GREEG CORPORA_DIR OUT.json   run search and verb queries on disposable
                                                 copies, store stdout and its o200k count
  tokens.py fit OUT.json                         fit the weights, report estimate/o200k

collect needs tiktoken; fit needs numpy and scipy. `features` mirrors tokens.rs: keep them equal.
"""
import json
import math
import os
import subprocess
import sys

from corpus import Corpus, executable

QUERIES = {
    "tokio": (["100", "300", "1000", "2000"], [
        ["fn poll_read"], ["-w", "Waker"], ["spawn_blocking"], ["JoinHandle", "--mode", "outline"],
        ["Waker", "--mode", "files"], ["poll_read", "--mode", "block"], ["AsyncRead", "--budget", "500"],
        ["-i", "semaphore"], ["impl Drop for"], ["Notify", "--mode", "outline"],
        ["def", "spawn"], ["refs", "spawn_blocking"], ["callers", "spawn_blocking"], ["impls", "AsyncRead"],
        ["impact", "spawn_blocking"], ["outline", "tokio/src/runtime/blocking/pool.rs"],
        ["outline", "tokio/src/sync/mod.rs"], ["show", "tokio/src/runtime/blocking/pool.rs:150"],
        ["map", "tokio/src"], ["def", "spawn", "--mode", "block"]]),
    "django": (["100", "300", "1000", "2000"], [
        ["get_queryset"], ["-w", "request"], ["HttpResponseRedirect"], ["csrf_token", "--budget", "800"],
        ["ModelAdmin", "--mode", "block"], ["Model", "--mode", "outline"],
        ["def", "get_queryset"], ["refs", "QuerySet"], ["callers", "get_queryset"], ["impls", "Model"],
        ["impact", "render"], ["outline", "django/db/models/query.py"], ["show", "django/db/models/query.py:300"],
        ["map", "django/db"], ["outline", "django/contrib/admin/options.py"], ["refs", "get_object_or_404"],
        ["impls", "Field"], ["show", "django/contrib/admin/options.py:120"]]),
    "ktor": (["100", "300", "1000", "2000"], [
        ["respond"], ["fun respond"], ["ApplicationCall", "--budget", "4000"], ["ContentNegotiation", "--mode", "outline"],
        ["def", "respond"], ["refs", "ApplicationCall"], ["callers", "respond"], ["impls", "Closeable"],
        ["impact", "respond"], ["map", "ktor-server"], ["def", "install"], ["refs", "HttpClient"]]),
    "TypeScript-5.9": (["100", "300", "1000", "2000"], [
        ["createSourceFile"], ["checkExpression"], ["-w", "node"],
        ["def", "createSourceFile"], ["refs", "SyntaxKind"], ["callers", "checkExpression"],
        ["outline", "src/compiler/utilities.ts"], ["map", "src/compiler"], ["impls", "Node"],
        ["show", "src/compiler/parser.ts:1500"], ["outline", "src/compiler/types.ts"]]),
    "rust": (["100", "300", "1000", "2000"], [
        ["mir_borrowck"], ["-w", "HirId"], ["TyCtxt", "--budget", "1000"],
        ["def", "mir_borrowck"], ["refs", "HirId"], ["callers", "mir_borrowck"], ["impls", "Visitor"],
        ["outline", "compiler/rustc_middle/src/ty/context.rs"], ["map", "compiler/rustc_borrowck/src"],
        ["impls", "Iterator"], ["show", "compiler/rustc_middle/src/ty/context.rs:900"], ["impact", "mir_borrowck"]]),
    "kotlinx.coroutines": (["150", "600", "2000"], [
        ["launch"], ["-w", "Job"], ["suspend fun"], ["CoroutineScope", "--mode", "outline"],
        ["withContext", "--mode", "block"], ["Dispatchers", "--mode", "files"],
        ["def", "launch"], ["refs", "CoroutineScope"], ["callers", "withContext"], ["impls", "Job"],
        ["impact", "withContext"], ["outline", "kotlinx-coroutines-core/common/src/Job.kt"],
        ["show", "kotlinx-coroutines-core/common/src/Builders.common.kt:40"], ["map", "kotlinx-coroutines-core"]]),
    "ui": (["150", "600", "2000"], [
        ["className"], ["-w", "Button"], ["useState"], ["cn(", "--mode", "outline"], ["export function", "--mode", "files"],
        ["def", "Button"], ["refs", "cn"], ["callers", "cn"], ["impact", "Button"], ["outline", "apps/v4/mdx-components.tsx"],
        ["show", "apps/v4/mdx-components.tsx:30"], ["map", "apps/v4"], ["def", "Card"]]),
}
VERBS = {"def", "refs", "callers", "impls", "impact", "outline", "show", "map"}
FORMATS = [[], ["--json=greeg"]]


def collect(greeg, corpora, out):
    import tiktoken
    enc = tiktoken.get_encoding("o200k_base")
    rows = []
    for name, (budgets, queries) in QUERIES.items():
        with Corpus(os.path.join(corpora, name)) as c:
            subprocess.run([greeg, "index", "--quiet"], cwd=c.root, env=c.env, check=True)
            for q in queries:
                layout = q[0] if q[0] in VERBS else "search" + ("-" + q[q.index("--mode") + 1] if "--mode" in q else "")
                for fmt in FORMATS:
                    for b in [None] if "--budget" in q else budgets:
                        argv = [greeg, *q, *fmt, "--no-session", "--fresh", "stat"] + (["--budget", b] if b else [])
                        p = subprocess.run(argv, cwd=c.root, env=c.env, capture_output=True, stdin=subprocess.DEVNULL)
                        text = p.stdout.decode("utf-8", "replace")
                        if text:
                            rows.append({"corpus": name, "layout": layout, "fmt": "json" if fmt else "text",
                                         "query": " ".join(q + fmt) + (f" --budget {b}" if b else ""),
                                         "text": text, "o200k": len(enc.encode(text, disallowed_special=()))})
        print(name, len(rows), file=sys.stderr, flush=True)
    with open(out, "w") as f:
        json.dump(rows, f)


def is_upper(c): return 65 <= c <= 90
def is_lower(c): return 97 <= c <= 122
def is_letter(c): return is_upper(c) or is_lower(c)
def is_digit(c): return 48 <= c <= 57
def is_space(c): return c in (32, 9, 10, 13, 11, 12)
def is_punct(c): return not (is_letter(c) or is_digit(c) or is_space(c))


def word_end(b, i):
    while i < len(b) and is_upper(b[i]):
        i += 1
    while i < len(b) and is_lower(b[i]):
        i += 1
    if i < len(b) and b[i] == 39:
        for suffix in (b"s", b"t", b"m", b"d", b"re", b"ve", b"ll"):
            if b[i + 1:i + 1 + len(suffix)].lower() == suffix:
                return i + 1 + len(suffix)
    return i


NAMES = ["word", "word_len", "caps_len", "lead", "space_or_digits", "punct", "punct_len", "non_ascii"]


def features(b):
    """Per-text sums of o200k's pre-tokenizer pieces, by kind and length."""
    f = [0.0] * len(NAMES)
    n, i = len(b), 0
    while i < n:
        c = b[i]
        leads = c < 0x80 and not is_letter(c) and not is_digit(c) and c not in (10, 13) and i + 1 < n and is_letter(b[i + 1])
        if is_letter(c) or leads:
            start = i + 1 if leads else i
            end = word_end(b, start)
            word = b[start:end]
            f[0] += 1
            if len(word) > 1 and all(is_upper(w) or w == 39 for w in word):
                f[2] += len(word) - 2
            else:
                f[1] += max(0, len(word) - 6)
            if leads and c != 32:
                f[3] += 1
            i = end
        elif is_digit(c):
            start = i
            while i < n and i - start < 3 and is_digit(b[i]):
                i += 1
            f[4] += 1
        elif is_punct(c) or (c == 32 and i + 1 < n and is_punct(b[i + 1])):
            i += c == 32
            ascii_, non_ascii = 0, 0
            while i < n and is_punct(b[i]):
                ascii_ += b[i] < 0x80
                non_ascii += b[i] >= 0xC0
                i += 1
            while i < n and b[i] in (10, 13, 47):
                ascii_ += b[i] == 47
                i += 1
            if ascii_:
                f[5] += 1
                f[6] += max(0, ascii_ - 3)
            f[7] += non_ascii
        else:
            start = end = i
            while end < n and is_space(b[end]):
                end += 1
            breaks = [k for k in range(start, end) if b[k] in (10, 13)]
            i = breaks[-1] + 1 if breaks else end - 1 if end < n and end - start >= 2 else end
            f[4] += 1
    return f


def fit(path):
    import numpy as np
    from scipy.optimize import nnls
    rows = json.load(open(path))
    X = np.array([features(r["text"].encode()) for r in rows])
    y = np.array([r["o200k"] for r in rows], dtype=float)
    # relative error matters, so weight each output by 1/sqrt(size)
    sw = 1 / np.sqrt(y)
    w, _ = nnls(X * sw[:, None], y * sw)
    print("weights:", ", ".join(f"{n} {v:.2f}" for n, v in zip(NAMES, w)))
    ratio = np.ceil(X @ np.round(w, 2)) / y
    print(f"estimate/o200k over {len(rows)} outputs: p5 {np.percentile(ratio, 5):.3f} median {np.median(ratio):.3f} "
          f"p95 {np.percentile(ratio, 95):.3f}, min {ratio.min():.3f} max {ratio.max():.3f}, "
          f"within 8 %: {np.mean(abs(ratio - 1) <= 0.08):.0%}")
    groups = {}
    for r, x in zip(rows, ratio):
        groups.setdefault((r["fmt"], r["layout"]), []).append(x)
    for (fmt, layout), v in sorted(groups.items()):
        print(f"  {fmt:4} {layout:15} n={len(v):3} min {min(v):.3f} max {max(v):.3f}")
    print("leave one corpus out:")
    corpus = np.array([r["corpus"] for r in rows])
    for held in sorted(set(corpus)):
        train = corpus != held
        wh, _ = nnls((X * sw[:, None])[train], (y * sw)[train])
        r = np.ceil(X[~train] @ np.round(wh, 2)) / y[~train]
        print(f"  {held:18} min {r.min():.3f} median {np.median(r):.3f} max {r.max():.3f}")


if __name__ == "__main__":
    if len(sys.argv) == 5 and sys.argv[1] == "collect":
        collect(executable(sys.argv[2]), sys.argv[3], sys.argv[4])
    elif len(sys.argv) == 3 and sys.argv[1] == "fit":
        fit(sys.argv[2])
    else:
        sys.exit(__doc__)
