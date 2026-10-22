# SCIP headroom check (phase 7, experiment 2)

This asks, before any product code: how many answers would perfect name
bindings change? Bindings tie each use of a name to the definition it means,
as SCIP records them. The check decides whether a SCIP sidecar (W11) is worth
a prototype. The rule was agreed before it ran: **if perfect bindings would fix
fewer than 5 % of the sampled answers, W11 is rejected.**

```sh
python3 bench/scip_headroom.py GREEG tokio django TypeScript-5.9 --n 100 --output R.json
```

## Method
- **Sample:** names are drawn from each corpus's SCIP index with the oracle's
  filter: identifiers of 3+ characters, with an in-repo definition and 3+
  references. Probability is ∝ log(1 + references), and there are **no
  strata by ambiguity**, so ambiguous names come up as often as the corpus
  has them. Seed 7, 100 names per corpus.
- **refs:** take the lines `greeg refs NAME --budget 0` lists in files SCIP
  covers, and that SCIP binds to a symbol of that name. Count the share bound
  to a symbol other than the name's most-referenced in-repo definition. The
  answer changes materially at 10 % or more.
- **def from a use:** take one reference of the name, bound to an in-repo
  definition. Does `greeg def NAME --from FILE` rank that definition first?
- **Ground truth:** the oracle's indexes. scip-typescript 0.4.0, scip-python
  0.6.6 and rust-analyzer 1.94.0, at the pinned corpus revisions. greeg at
  c719108.

## Result (2026-10-22)

| corpus | refs change materially | def from a use misses | either | names with 1 / 2–5 / 6+ definitions |
|---|---:|---:|---:|---|
| tokio | 54 % | 25 % | 54 % | 55 / 29 / 16 |
| django | 37 % | 12 % | 37 % | 64 / 21 / 15 |
| TypeScript-5.9 | 57 % | 37 % (of 98) | 66 % | 26 / 52 / 22 |

- **Headroom: yes.** Far above the 5 % rule on all three corpora, so W11
  is not rejected by it.
- **Nearly all of it is in ambiguous names.** With one definition, `def`
  never misses, and `refs` changes only when same-named external symbols
  are listed: tokio 9 of 55 (std's `TcpStream`, mio's `Interest`), django 1
  of 64, TypeScript 0 of 26.
- **The other definitions are real, distinct ones:**
  - methods of other types (`reset_at` on `Interval` and on `DelayQueue`,
    `new`, `get_pin_mut`);
  - a field and a method (`driver`);
  - free functions in other modules (`io::copy`, `fs::copy`).
- **Some are families an agent may want whole:** overrides of one method
  (django's `process_response`, `__deepcopy__`), or an interface member and
  its implementations. A per-definition answer is not always the one wanted,
  so the counts are an upper bound on the value.
- **Agents have not shown this as a failure.** In the ambiguous-definition
  run (`bench/agent/README.md`), no agent answered with the wrong definition
  in 24 runs: they read the receiver's type or the hierarchy. The headroom is
  in greeg's answers, which agents then correct by reading.

**Decision (2026-10-22): the SCIP sidecar prototype is shelved.**
- It would only help users who supply a SCIP index matching their tree.
- The measured gap is in greeg's answers, which agents already correct by
  reading.

Revisit it, or the cheaper per-definition `refs` and `callers` from greeg's
own import graph, when an agent run shows wrong-definition failures.

`scip-headroom-2026-10-22.json` holds every row: the name, its ambiguity, the
listed lines by binding, and the use and verdict for `def`.
