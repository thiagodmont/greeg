#!/usr/bin/env python3
"""SCIP headroom check (phase 7, experiment 2): what would perfect bindings change?

    bench/scip_headroom.py GREEG CORPUS... [--n 100] [--seed 7] [--output R.json]

Names are drawn from the corpus's SCIP index, as the oracle (`bench.py oracle`)
draws them but without strata by ambiguity: probability ∝ log(1 + references),
so ambiguous names come up as often as the corpus has them. For each name it
measures two answers that SCIP bindings could change:

- refs: the share of the lines `greeg refs NAME --budget 0` lists that SCIP binds
  to another symbol of that name than its most-referenced in-repo definition.
  The answer changes materially at 10 % or more.
- def from a use: for one reference of the name, bound to an in-repo
  definition, whether `greeg def NAME --from FILE` ranks that definition first.

Rule agreed before the check: if perfect bindings would fix fewer than 5 % of
the sampled answers, the SCIP sidecar (W11) is rejected without a prototype.
The index is built in a temporary GREEG_INDEX_DIR.
"""
import argparse, json, math, os, random, re, subprocess, sys, tempfile, time

import bench as B
import json_records

MATERIAL = 0.10


def read_symbols(path):
    """{symbol: {name, kind, defs, refs}}, (path, line) -> symbols there, documents."""
    with open(path, "rb") as fh:
        data = fh.read()
    syms, at, docs = {}, {}, set()
    for f, v in B.pb_fields(data):
        if f != 2:
            continue
        rel, occs = None, []
        for df, dv in B.pb_fields(v):
            if df == 1:
                rel = dv.decode("utf8", "replace").removeprefix("./")
            elif df == 2:
                occs.append(dv)
        if rel is None:
            continue
        docs.add(rel)
        for o in occs:
            rng, sym, roles = None, None, 0
            for of, ov in B.pb_fields(o):
                if of == 1:
                    rng = B.pb_packed_i32(ov) if isinstance(ov, bytes) else [ov]
                elif of == 2:
                    sym = ov.decode("utf8", "replace")
                elif of == 3:
                    roles = ov
            if not rng or sym is None:
                continue
            nk = B.scip_name(sym)
            if not nk:
                continue
            loc = (rel, rng[0] + 1)
            e = syms.setdefault(sym, {"name": nk[0], "kind": nk[1], "defs": set(), "refs": set()})
            (e["defs"] if roles & 1 else e["refs"]).add(loc)
            at.setdefault(loc, set()).add(sym)
    return syms, at, docs


def population(syms):
    """Names the oracle would sample: identifiers of 3+ characters with an
    in-repo definition that is not a local or a file, and 3+ references."""
    by_name = {}
    for s, e in syms.items():
        by_name.setdefault(e["name"], []).append(s)
    out = {}
    for n, ss in by_name.items():
        local = [s for s in ss if syms[s]["defs"] and syms[s]["kind"] not in ("meta", "namespace")]
        refs = sum(len(syms[s]["refs"]) for s in ss)
        if len(n) >= 3 and re.match(r"^[A-Za-z_]\w*$", n) and local and refs >= B.MIN_REFS:
            out[n] = {"symbols": ss, "local": local, "refs": refs}
    return out


def records(greeg, args, cwd, env):
    out = subprocess.run([greeg, *args, "--json=greeg", "--no-session"], cwd=cwd, env=env,
                         stdin=subprocess.DEVNULL, capture_output=True, text=True).stdout
    return [json.loads(l) for l in out.splitlines() if l.startswith("{")]


def bucket(k):
    return "1" if k == 1 else "2-5" if k <= 5 else "6+"


def check(greeg, corpus, scip, n, seed):
    cwd = B.corpus_path(corpus)
    t0 = time.time()
    syms, at, docs = read_symbols(scip)
    pop = population(syms)
    print(f"== {corpus}: {len(syms)} symbols, {len(pop)} names in the population ({time.time() - t0:.0f} s)", flush=True)
    rnd = random.Random(seed)
    names = B.wsample(list(pop), n, lambda nm: math.log1p(pop[nm]["refs"]), rnd)
    index_dir = tempfile.mkdtemp(prefix="greeg-headroom-")
    env = {**os.environ, "GREEG_INDEX_DIR": index_dir, "GREEG_STATS": "0", "GREEG_BUDGET": "2000"}
    subprocess.run([greeg, "index", "--quiet"], cwd=cwd, env=env, check=True, capture_output=True)
    rows = []
    for nm in names:
        p = pop[nm]
        target = max(p["local"], key=lambda s: (len(syms[s]["refs"]), s))
        mine = set(p["symbols"])
        listed = {(json_records.path_of(r["data"]["path"]), r["data"]["line"])
                  for r in records(greeg, ["refs", nm, "--budget", "0"], cwd, env) if r["type"] in ("ref", "def")}
        bound = {"target": 0, "other": 0, "unbound": 0, "outside": 0}
        for loc in listed:
            if loc[0] not in docs:
                bound["outside"] += 1
                continue
            here = at.get(loc, set()) & mine
            bound["target" if target in here else "other" if here else "unbound"] += 1
        judged = bound["target"] + bound["other"]
        other_share = bound["other"] / judged if judged else 0.0
        # one use of an in-repo definition, and whether def ranks that definition first from its file
        uses = sorted((loc, s) for s in p["local"] for loc in syms[s]["refs"])
        use = rnd.choice(uses) if uses else None
        def_hit = None
        if use:
            (path, _), s = use
            defs = [(json_records.path_of(r["data"]["path"]), r["data"]["line"])
                    for r in records(greeg, ["def", nm, "--from", path], cwd, env) if r["type"] == "def"]
            def_hit = bool(defs) and defs[0] in syms[s]["defs"]
        rows.append({"name": nm, "bucket": bucket(len(p["local"])), "definitions": len(p["local"]),
                     "refs": p["refs"], "listed": bound, "other_share": round(other_share, 3),
                     "refs_change": other_share >= MATERIAL, "use": use and f"{use[0][0]}:{use[0][1]}",
                     "def_from_use_first": def_hit})
    return rows


def summary(rows):
    n = len(rows)
    refs_change = sum(r["refs_change"] for r in rows)
    judged = [r for r in rows if r["def_from_use_first"] is not None]
    def_miss = sum(not r["def_from_use_first"] for r in judged)
    either = sum(r["refs_change"] or r["def_from_use_first"] is False for r in rows)
    by = {}
    for b in ("1", "2-5", "6+"):
        rs = [r for r in rows if r["bucket"] == b]
        by[b] = {"n": len(rs), "refs_change": sum(r["refs_change"] for r in rs),
                 "def_miss": sum(r["def_from_use_first"] is False for r in rs)}
    return {"n": n, "refs_change": refs_change, "refs_change_share": round(refs_change / n, 3) if n else None,
            "mean_other_share": round(sum(r["other_share"] for r in rows) / n, 3) if n else None,
            "def_judged": len(judged), "def_miss": def_miss,
            "either": either, "either_share": round(either / n, 3) if n else None, "by_bucket": by}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("greeg")
    ap.add_argument("corpora", nargs="+")
    ap.add_argument("--n", type=int, default=100)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--scip", default=os.path.join(os.path.dirname(B.cache_dir()), "scip"))
    ap.add_argument("--output")
    a = ap.parse_args()
    greeg = os.path.abspath(a.greeg)
    out = {"greeg": B.version_of([greeg, "--version"]), "seed": a.seed, "n": a.n, "material": MATERIAL, "corpora": {}}
    for corpus in a.corpora:
        scip = os.path.join(a.scip, corpus, "index.scip")
        if not os.path.exists(scip):
            sys.exit(f"{corpus}: no {scip}")
        rows = check(greeg, corpus, scip, a.n, a.seed)
        s = summary(rows)
        out["corpora"][corpus] = {"revision": B.out_of(["git", "rev-parse", "--short", "HEAD"], B.corpus_path(corpus)).strip(),
                                  "summary": s, "rows": rows}
        print(json.dumps(s), flush=True)
    if a.output:
        with open(a.output, "w") as fh:
            json.dump(out, fh, indent=1)


if __name__ == "__main__":
    main()
