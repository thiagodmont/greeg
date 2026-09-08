#!/usr/bin/env python3
"""Parity check against ripgrep.

    bench/parity.py CORPORA_DIR GREEG [--corpora a,b] [--no-sets] [--no-matrix]

Two parts. *Sets*: for each fetched corpus, the (path, line) sets of
`greeg --json --budget 0 --no-ladder --max-columns 0` and `rg --json` must be
equal for a table of queries. Corpora that are not fetched are skipped with a
message (CI fetches only the small tier). *Matrix*: behavioural cases run on
the first fetched corpus plus a scratch directory: absolute path arguments,
`-U`, `-l` and `-c` output shape, stdin, a `.gitignore` edit (temporary,
restored), `-C 2` twice in one session, `-x`, `-F`, `-S`, a UTF-8 BOM file, a
glob-looking positional, exit codes, and JSON match-record counts. Exit 1 on
any failure.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time

SET_CASES = {
    "tokio": [["fn poll_read"], ["-w", "Waker"], ["-i", "jOinHandle"], ["spawn_blocking", "-t", "rust"], ["-F", "Pin<&mut Self>"], [r"\bpoll_\w+\("]],
    "django": [["get_queryset"], ["-w", "request"], ["def get_queryset"], ["-g", "*.py", "csrf_token"], ["HttpResponseRedirect", "-t", "py"]],
    "ktor": [["respond"], ["fun respond"], ["-w", "ApplicationCall"], ["ContentNegotiation", "-g", "!**/test/**"]],
    "kotlinx.coroutines": [["CoroutineScope"], ["-w", "Job"], ["-F", "suspend fun"]],
    "TypeScript": [["createSourceFile"], ["checkExpression"], ["ParseFlags"]],
    "TypeScript-5.9": [["createSourceFile"], ["checkExpression"], ["ParseFlags"]],
    "rust": [["mir_borrowck"], ["-w", "HirId"], ["LocalDefId", "-t", "rust"]],
}
# per corpus: a name with many hits and context around them, a source subdirectory, a glob
MATRIX_PARAMS = {
    "tokio": {"name": "Waker", "subdir": "tokio/src/sync", "glob": "tokio/src/**"},
    "django": {"name": "HttpResponse", "subdir": "django/http", "glob": "django/http/**"},
    "ktor": {"name": "ApplicationCall", "subdir": "ktor-server/ktor-server-core", "glob": "ktor-server/**"},
    "kotlinx.coroutines": {"name": "Job", "subdir": "kotlinx-coroutines-core/common", "glob": "kotlinx-coroutines-core/**"},
    "TypeScript": {"name": "SyntaxKind", "subdir": "src/compiler", "glob": "src/compiler/**"},
    "TypeScript-5.9": {"name": "SyntaxKind", "subdir": "src/compiler", "glob": "src/compiler/**"},
    "rust": {"name": "HirId", "subdir": "compiler/rustc_hir/src", "glob": "compiler/rustc_hir/**"},
}
GREEG_RAW = ["--json", "--budget", "0", "--no-ladder", "--max-columns", "0"]


NO_STATS = {**os.environ, "GREEG_STATS": "0"}  # parity runs are not usage: keep them out of `greeg stats`


def sh(cmd, cwd, stdin=None):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, input=stdin, stdin=None if stdin is not None else subprocess.DEVNULL, env=NO_STATS)


def records(out, kind="match"):
    recs = []
    for line in out.splitlines():
        if not line.startswith("{"):
            continue
        try:
            j = json.loads(line)
        except ValueError:
            continue
        if j.get("type") == kind:
            recs.append(j["data"])
    return recs


def pairs(out):
    return {(d["path"]["text"].removeprefix("./"), d["line_number"]) for d in records(out)}


def rg_pairs(args, cwd):
    return pairs(sh(["rg", "--json", "-n", *args], cwd).stdout.decode(errors="replace"))


def greeg_pairs(greeg, args, cwd, session=False):
    return pairs(sh([greeg, *GREEG_RAW, *([] if session else ["--no-session"]), *args], cwd).stdout.decode(errors="replace"))


def diff(a, b):
    miss, extra = a - b, b - a
    if not miss and not extra:
        return None
    return f"rg={len(a)} greeg={len(b)} missing={len(miss)} extra={len(extra)}" + "".join(f" missing {x}" for x in sorted(miss)[:3]) + "".join(f" extra {x}" for x in sorted(extra)[:3])


def run_sets(corpora_dir, greeg, names):
    ok, ran = True, 0
    print(f"{'corpus':18} {'query':34} {'rg':>6} {'greeg':>6} {'missing':>7} {'extra':>5}  status")
    for c in names:
        qs = SET_CASES.get(c)
        if not qs:
            continue
        cwd = os.path.join(corpora_dir, c)
        if not os.path.isdir(cwd):
            print(f"{c:18} not fetched (bench/bench.py fetch {c}); skipped")
            continue
        for q in qs:
            a = rg_pairs([*q, "."], cwd)
            b = greeg_pairs(greeg, [*q, "."], cwd)
            miss, extra = a - b, b - a
            st = "OK" if not miss and not extra else "DIFF"
            if st == "DIFF":
                ok = False
                st += "".join(f" missing {x}" for x in sorted(miss)[:3]) + "".join(f" extra {x}" for x in sorted(extra)[:3])
            print(f"{c:18} {' '.join(q):34} {len(a):6} {len(b):6} {len(miss):7} {len(extra):5}  {st}")
            ran += 1
    return ok, ran


# ───────────────────────────── matrix ─────────────────────────────

def case_abs_path(greeg, cwd, prm, scratch):
    """Absolute path arguments: rg prints absolute paths; the sets must match."""
    target = os.path.join(cwd, prm["subdir"])
    a = rg_pairs(["-w", prm["name"], target], cwd)
    b = greeg_pairs(greeg, ["-w", prm["name"], target], cwd)
    if not a:
        return False, f"rg found nothing under {target} (bad case parameters)"
    return (diff(a, b) is None), diff(a, b) or f"{len(a)} lines"


def case_multiline(greeg, cwd, prm, scratch):
    """-U: a pattern spanning two lines; (path, first line) sets must match rg -U."""
    q = ["-U", r"\{\n\s*\}"]
    a = rg_pairs([*q, "."], cwd)
    b = greeg_pairs(greeg, [*q, "."], cwd)
    if not a:
        return False, "rg -U found nothing (bad case pattern)"
    return diff(a, b) is None, diff(a, b) or f"{len(a)} multiline matches"


def case_files_with_matches(greeg, cwd, prm, scratch):
    """-l with stdout piped: only paths, one per line, same set as rg -l."""
    a = {l.removeprefix("./") for l in sh(["rg", "-l", "-w", prm["name"], "."], cwd).stdout.decode(errors="replace").splitlines() if l.strip()}
    raw = sh([greeg, "--no-session", "-l", "-w", prm["name"], "."], cwd).stdout.decode(errors="replace")
    lines = [l for l in raw.splitlines() if l.strip()]
    not_paths = [l for l in lines if not os.path.exists(os.path.join(cwd, l.strip()))]
    b = {l.strip().removeprefix("./") for l in lines}
    if not_paths:
        return False, f"{len(not_paths)} of {len(lines)} lines are not bare paths, e.g. {not_paths[0]!r}"
    return diff(a, b) is None, diff(a, b) or f"{len(a)} paths"


def case_count(greeg, cwd, prm, scratch):
    """-c: `path:count` lines, same set as rg -c."""
    a = {l.removeprefix("./") for l in sh(["rg", "-c", "-w", prm["name"], "."], cwd).stdout.decode(errors="replace").splitlines() if l.strip()}
    raw = [l for l in sh([greeg, "--no-session", "-c", "-w", prm["name"], "."], cwd).stdout.decode(errors="replace").splitlines() if l.strip()]
    bad = [l for l in raw if l.count(":") < 1 or not l.rsplit(":", 1)[1].strip().isdigit()]
    if bad:
        return False, f"{len(bad)} lines are not path:count, e.g. {bad[0]!r}"
    b = {l.strip().removeprefix("./") for l in raw}
    return diff(a, b) is None, diff(a, b) or f"{len(a)} files"


def case_stdin(greeg, cwd, prm, scratch):
    """printf 'a\\nb\\n' | greeg a  must print what rg prints (the matching stdin line)."""
    data = b"a\nb\n"
    a = sh(["rg", "a"], scratch, stdin=data).stdout
    b = sh([greeg, "--no-session", "a"], scratch, stdin=data).stdout
    return a == b, f"rg={a!r} greeg={b[:60]!r}"


def case_gitignore_edit(greeg, cwd, prm, scratch):
    """A new dir is found; after adding it to .gitignore (temporary edit) it is not; then restored."""
    d = os.path.join(cwd, "zz_parity_dir")
    gi = os.path.join(cwd, ".gitignore")
    orig = open(gi, "rb").read() if os.path.exists(gi) else None
    try:
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "a.rs"), "w") as fh:
            fh.write("fn zz_parity_mark() {}\n")
        time.sleep(0.15)
        a1, b1 = rg_pairs(["zz_parity_mark", "."], cwd), greeg_pairs(greeg, ["zz_parity_mark", "."], cwd)
        with open(gi, "ab") as fh:
            fh.write(b"\n/zz_parity_dir/\n")
        time.sleep(0.15)
        a2, b2 = rg_pairs(["zz_parity_mark", "."], cwd), greeg_pairs(greeg, ["zz_parity_mark", "."], cwd)
    finally:
        if orig is None:
            if os.path.exists(gi):
                os.remove(gi)
        else:
            with open(gi, "wb") as fh:
                fh.write(orig)
        shutil.rmtree(d, ignore_errors=True)
    if not a1 or a2:
        return False, f"rg itself did not behave (before={len(a1)} after={len(a2)})"
    d1, d2 = diff(a1, b1), diff(a2, b2)
    return d1 is None and d2 is None, f"before edit: {d1 or 'OK'}; after edit: {d2 or 'OK'}"


def case_context_twice(greeg, cwd, prm, scratch):
    """-C 2 twice in one session: context records must appear both times (session dedup must not eat them)."""
    session = f"parity-{os.getpid()}"
    q = ["-C", "2", "-w", prm["name"], "."]
    rg_ctx = len(records(sh(["rg", "--json", "-n", *q], cwd).stdout.decode(errors="replace"), "context"))
    counts = []
    for _ in range(2):
        out = sh([greeg, *GREEG_RAW, "--session", session, *q], cwd).stdout.decode(errors="replace")
        counts.append(len(records(out, "context")))
        time.sleep(0.05)
    ok = rg_ctx > 0 and counts[0] == rg_ctx and counts[1] == rg_ctx
    return ok, f"rg context records {rg_ctx}; greeg run 1: {counts[0]}, run 2: {counts[1]}"


def case_line_regexp(greeg, cwd, prm, scratch):
    """-x: whole-line matches."""
    q = ["-x", r"\s*\}"]
    a, b = rg_pairs([*q, "."], cwd), greeg_pairs(greeg, [*q, "."], cwd)
    return diff(a, b) is None and bool(a), diff(a, b) or f"{len(a)} lines"


def case_fixed(greeg, cwd, prm, scratch):
    """-F: a literal with regex metacharacters."""
    q = ["-F", "(&self)"]
    a, b = rg_pairs([*q, "."], cwd), greeg_pairs(greeg, [*q, "."], cwd)
    return diff(a, b) is None and bool(a), diff(a, b) or f"{len(a)} lines"


def case_smart_case(greeg, cwd, prm, scratch):
    """-S: insensitive for a lowercase pattern, sensitive when the pattern has uppercase."""
    out = []
    ok = True
    for pat in (prm["name"].lower(), prm["name"]):
        a, b = rg_pairs(["-S", "-w", pat, "."], cwd), greeg_pairs(greeg, ["-S", "-w", pat, "."], cwd)
        d = diff(a, b)
        ok &= d is None
        out.append(f"{pat}: {d or f'{len(a)} lines'}")
    return ok, "; ".join(out)


def case_bom(greeg, cwd, prm, scratch):
    """A UTF-8 BOM file: the BOM must not hide the first line's match."""
    with open(os.path.join(scratch, "bom.rs"), "wb") as fh:
        fh.write(b"\xef\xbb\xbfzz_bom_marker first\nzz_bom_marker second\n")
    a, b = rg_pairs(["zz_bom_marker", "."], scratch), greeg_pairs(greeg, ["zz_bom_marker", "."], scratch)
    return diff(a, b) is None and len(a) == 2, diff(a, b) or f"{len(a)} lines"


def case_glob_positional(greeg, cwd, prm, scratch):
    """A glob-looking positional (`src/**`) selects the same files as -g 'src/**'."""
    g = prm["glob"]
    a = rg_pairs(["-w", prm["name"], "-g", g, "."], cwd)
    b = greeg_pairs(greeg, ["-w", prm["name"], g], cwd)
    c = greeg_pairs(greeg, ["-w", prm["name"], "-g", g, "."], cwd)
    d1, d2 = diff(a, b), diff(a, c)
    return d1 is None and d2 is None and bool(a), f"positional: {d1 or 'OK'}; -g: {d2 or 'OK'} ({len(a)} lines)"


def case_exit_codes(greeg, cwd, prm, scratch):
    """0 with hits, 1 without, 2 on error (bad regex, missing path), like rg."""
    checks = [("hits", ["-w", prm["name"], "."]), ("none", ["zzqq_no_such_token_zzqq", "."]), ("bad regex", ["(", "."]), ("missing path", [prm["name"], "/nonexistent/zz_parity"])]
    ok, out = True, []
    for label, q in checks:
        r = sh(["rg", *q], cwd).returncode
        g = sh([greeg, "--no-session", *q], cwd).returncode
        ok &= r == g
        out.append(f"{label}: rg {r} greeg {g}")
    return ok, "; ".join(out)


def case_json_count(greeg, cwd, prm, scratch):
    """The number of JSON match records equals rg's (one per matching line)."""
    q = ["-w", prm["name"], "."]
    a = len(records(sh(["rg", "--json", "-n", *q], cwd).stdout.decode(errors="replace")))
    b = len(records(sh([greeg, *GREEG_RAW, "--no-session", *q], cwd).stdout.decode(errors="replace")))
    return a == b and a > 0, f"rg {a} greeg {b}"


MATRIX = [("abs-path", case_abs_path), ("multiline -U", case_multiline), ("-l | cat", case_files_with_matches), ("-c", case_count), ("stdin", case_stdin), (".gitignore edit", case_gitignore_edit), ("-C 2 twice", case_context_twice), ("-x", case_line_regexp), ("-F", case_fixed), ("-S", case_smart_case), ("UTF-8 BOM", case_bom), ("glob positional", case_glob_positional), ("exit codes", case_exit_codes), ("json count", case_json_count)]


def run_matrix(corpora_dir, greeg, names):
    corpus = next((c for c in names if c in MATRIX_PARAMS and os.path.isdir(os.path.join(corpora_dir, c))), None)
    if not corpus:
        print("matrix: no fetched corpus with parameters; skipped")
        return True, 0
    cwd = os.path.join(corpora_dir, corpus)
    prm = MATRIX_PARAMS[corpus]
    scratch = tempfile.mkdtemp(prefix="greeg-parity-", dir=os.environ.get("GREEG_PARITY_SCRATCH"))
    subprocess.run([greeg, "index", "--root", "."], cwd=cwd, capture_output=True, env=NO_STATS)
    print(f"\nmatrix on {corpus} (scratch {scratch})")
    ok = True
    failed = []
    try:
        for label, fn in MATRIX:
            try:
                good, detail = fn(greeg, cwd, prm, scratch)
            except Exception as e:  # noqa: BLE001 - a crash is a failure, not an abort
                good, detail = False, f"exception {type(e).__name__}: {e}"
            ok &= good
            if not good:
                failed.append(label)
            print(f"  {'OK  ' if good else 'FAIL'} {label:18} {detail}")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    if failed:
        print(f"matrix failures: {', '.join(failed)}")
    return ok, len(MATRIX)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("corpora_dir")
    p.add_argument("greeg")
    p.add_argument("--corpora", help="comma-separated corpus directory names (default: every known corpus that is fetched)")
    p.add_argument("--no-sets", action="store_true")
    p.add_argument("--no-matrix", action="store_true")
    a = p.parse_args()
    greeg = os.path.abspath(a.greeg)
    names = a.corpora.split(",") if a.corpora else list(SET_CASES)
    ok, ran = True, 0
    if not a.no_sets:
        o, n = run_sets(a.corpora_dir, greeg, names)
        ok, ran = ok and o, ran + n
    if not a.no_matrix:
        o, n = run_matrix(a.corpora_dir, greeg, names)
        ok, ran = ok and o, ran + n
    if ran == 0:
        print("PARITY FAIL: no case ran (no corpus fetched under " + a.corpora_dir + ")")
        sys.exit(1)
    print("PARITY", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
