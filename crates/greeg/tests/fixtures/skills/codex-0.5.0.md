---
name: greeg
description: Code search for agents. Use `greeg` instead of rg/grep: ranked, syntax-aware, budgeted results, plus symbol verbs (def, refs, callers, impls, outline, map, impact).
---

# greeg

`greeg PATTERN [PATHS]` accepts ripgrep flags (`-i -w -F -t rs -g '*.py' -A/-B/-C --json`)
and returns hits classified as def/import/call/type/member/ident/doc/comment/string with
the enclosing symbol, tests and vendored code demoted, within a token budget
(`--budget N`, default 2000). Broad queries return facets first; the footer says what
was cut and suggests the next query. Exit 1 means no hits (after the escalation
ladder: word boundary, case, split tokens, fuzzy names).

`-l` prints only paths and `-c` prints `path:count`, one per line on stdout, so both
are pipe-safe: `greeg -l foo | xargs sed -i ...`, `greeg -c foo | sort -t: -k2 -n`.

Symbol questions are cheaper than searches:

    greeg def NAME              where it is defined (signature, doc, reachability)
    greeg refs NAME             references grouped by kind
    greeg callers NAME --depth 2
    greeg impls NAME            implementations / subclasses
    greeg outline FILE          definitions of a file as a tree
    greeg show FILE:LINE        the definition enclosing a line, whole and dedented
    greeg map DIR               important files by import PageRank
    greeg impact NAME           WILL / MAY BREAK / REVIEW if NAME changes

To read a definition's body, `greeg show FILE:LINE` prints the whole definition around a
hit and `greeg def NAME --mode block` prints every definition of NAME with its body, both
dedented and bounded by the definition's span (`--mode block` on a search does the same
for its definition hits). Prefer them to guessing a `sed -n 'A,Bp'` range; `-A/-B/-C N`
give fixed context around hits. Answers are rows only: `N hits · M files` means complete.

Use `-e PATTERN` when the pattern is a verb name (`greeg -e def src`). When the pattern
or a path starts with `-`, put `--` before it: `greeg -- -x src`, `greeg foo -- -weird`.
`--budget 0` gives unlimited, rg-ordered output. `greeg doctor` shows index state.

## Hook notes

The installed PreToolUse hook (`greeg hook run --agent codex`) rewrites plain
`rg`/`grep` shell calls into `greeg`. PreToolUse hooks run in parallel and the
rewrite from the hook that finishes last wins, so another shell rewriter (for
example RTK) can override or be overridden by this one; position in `config.toml`
does not order them.
