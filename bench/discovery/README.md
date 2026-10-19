# Discovery evaluation

Questions asked without a known identifier ("where does the runtime wake a
parked worker?"). This is phase 7's first experiment: can an offline
lexical-plus-graph ranker find the right file where grepping the question's
words does not?

## Dataset (`dataset.toml`, frozen 2026-10-19)

The dataset was frozen before any ranker work. Changing a query means
re-freezing it and saying so.

- **Written queries:** 90 in all, 30 each on tokio, TypeScript-5.9 and ktor,
  at the pinned revisions.
  - **Targets:** `bench/discovery.py sample` draws one definition per file,
    10–60 lines long, from non-test source. The draw is seeded and
    reproducible.
  - **Questions:** a model that saw only the code, and nothing of any
    ranker, wrote each question in plain words, with no identifiers. A few
    ordinary words that names are built from remain (`write`, `open`,
    `loop`), as do domain names such as `io_uring` and `CommonJS`.
  - **Answer:** the target's file.
  - **One exception:** the draw keeps out test files, but a test function
    inside a source file can still be drawn. One is: tokio-09, a test in
    `h2_histogram.rs`.
- **SWE-bench rows:** 120 django issues from SWE-bench Verified, drawn with
  a seed.
  - **Query:** the issue text.
  - **Answer:** any file the gold patch changes.
  - Each row runs on django checked out at its base commit.
  - `fetch` caches the issue texts, and `run` checks each one against the
    SHA-256 digest frozen with its row.
- **Split:** a seeded third of each set is `dev`; the rest is `heldout`,
  scored once, after tuning.
  - Held-out answers are stored only as SHA-256 digests of their paths: the
    scorer compares digests, so tuning never reads them.
  - `run` refuses held-out rows unless it is given `--final`.

## Protocol

`GREEG` below is the path to a greeg binary, and `rg` must be on `PATH`.
Each corpus must be clean at its pinned revision.

```sh
python3 bench/discovery.py fetch                 # SWE-bench rows → ~/.cache/greeg-bench/discovery
python3 bench/discovery.py run rg GREEG --split dev --output rg-dev.json
python3 bench/discovery.py run greeg-search GREEG --split dev --output gs-dev.json
python3 bench/discovery.py score gs-dev.json --vs rg-dev.json
```

- **Metrics:** Acc@k means a gold file is among the first k files; MRR is
  the mean reciprocal rank of the first gold file within the top 20.
  Brackets are 95 % bootstrap intervals.
### Gate, agreed before tuning

- **Baseline:** whichever of `rg` and `greeg-search` has the higher held-out
  Acc@5 on the pooled set (written and SWE-bench rows together, n = 140).
- **Pass on accuracy:** the candidate's pooled held-out Acc@5 is at least 10
  points above that baseline, and the 95 % bootstrap interval of the paired
  per-query difference (`score --vs`) excludes 0. Each set is also reported
  on its own; a set where the candidate is worse is a finding to explain,
  not a pass.
- **Resource envelope:**
  - index size at most 15 % larger;
  - full build CPU at most 10 % higher;
  - delta apply at most 10 % slower;
  - warm query p95 at most 50 ms on django and TypeScript-5.9;
  - no model and no network.
- **Then:** only after both pass does a paired agent run follow.

## Baselines, dev split (2026-10-19, greeg 0.12.1)

Both baselines take the question's words: those of 3+ characters, minus
stop words.

- **rg:** ranks files by how many distinct words they hold, then by hits.
- **greeg-search:** greeg's ranked search over `w1|w2|…` (`-i -w`), files in
  answer order.

| set | method | n | Acc@1 | Acc@5 | Acc@10 | MRR |
|---|---|---:|---:|---:|---:|---:|
| written | rg | 30 | 0.07 | 0.40 [0.23–0.57] | 0.47 | 0.21 |
| written | greeg-search | 30 | 0.00 | 0.10 [0.00–0.23] | 0.17 | 0.05 |
| SWE-bench django | rg | 40 | 0.03 | 0.10 [0.03–0.20] | 0.17 | 0.08 |
| SWE-bench django | greeg-search | 40 | 0.03 | 0.05 [0.00–0.12] | 0.10 | 0.05 |

Per corpus, rg's written Acc@5 is 0.70 on tokio, 0.30 on ktor and 0.20 on
TypeScript. greeg's ranked search was built for identifiers: with a dozen
plain words in an alternation, it ranks definitions of common words first.
Raw results: `results/*-dev-2026-10-19.json`.
