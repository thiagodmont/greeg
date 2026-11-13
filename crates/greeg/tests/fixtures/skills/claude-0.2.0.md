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
    greeg map DIR               important files by import PageRank
    greeg impact NAME           WILL / MAY BREAK / REVIEW if NAME changes

Use `-e PATTERN` when the pattern is a verb name (`greeg -e def src`). When the pattern
or a path starts with `-`, put `--` before it: `greeg -- -x src`, `greeg foo -- -weird`.
`--budget 0` gives unlimited, rg-ordered output. `greeg doctor` shows index state.

## Hook notes

The installed PreToolUse hook (`greeg hook run`) rewrites plain `rg`/`grep` Bash
calls into `greeg`. PreToolUse hooks run in parallel and the last `updatedInput`
wins, so another Bash rewriter (for example RTK) can override or be overridden by
this one; position in `settings.json` does not order them. Permission allow rules
such as `Bash(rg:*)` no longer match the rewritten command: pair them with
`Bash(greeg:*)`.
