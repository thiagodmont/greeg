#!/usr/bin/env python3
"""Check an answer that lists places: those a diff task's patch makes stale,
or the context a pack task asks for.

    check_sites.py TASK [ANSWER]     exit 0 when the answer passes
    check_sites.py TASK [ANSWER] --score   print recall, traps and extras as JSON

A citation is a line of the answer in the asked form, `name at path:line`
(bulleted or in backticks; a path suffix such as `checks.py:963` is enough),
within two lines of the verified one. A site may list alternatives,
`path:line|path:line`, such as a call and the function that makes it. Prose
that mentions a place, such as what was left out and why, cites nothing. The
answer passes when it cites every one of the task's `sites`, none of its
`traps` (a use of another definition with the same name), and at most two
other places. Places in the files a patch changes, and the task's `neutral`
ones (an import a signature change leaves valid, a local variable), never
count against it.
"""
import json, re, sys, tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
CITE = re.compile(r"^[\s>*+\d.)-]*[\w.:<>*$]+(?:\(\))?\s+at\s+([\w./-]*\w\.(?:py|pyi|rs|ts|tsx|js|kt|kts)):(\d+)", re.M)
NEAR = 2
EXTRAS = 2


def task(tid):
    with open(HERE / "tasks.toml", "rb") as fh:
        return next(t for t in tomllib.load(fh)["task"] if t["id"] == tid)


def changed_files(tid):
    files = set()
    for p in sorted((HERE / "diff" / tid).glob("*.patch")):
        files.update(m.group(1) for m in re.finditer(r"^\+\+\+ b/(\S+)", p.read_text(errors="replace"), re.M))
    return files


def same_file(cited, path):
    return path == cited or path.endswith("/" + cited)


def at(cited, place):
    for alt in place.split("|"):
        path, line = alt.rsplit(":", 1)
        if same_file(cited[0], path) and abs(cited[1] - int(line)) <= NEAR:
            return True
    return False


def score(t, answer):
    cites = [(m.group(1).lstrip("./"), int(m.group(2))) for m in CITE.finditer(answer.replace("`", ""))]
    found = [s for s in t["sites"] if any(at(c, s) for c in cites)]
    traps = sorted({tr for tr in t.get("traps", []) for c in cites if at(c, tr)})
    changed = changed_files(t["id"])
    extras = sorted({f"{p}:{n}" for p, n in cites
                     if not any(at((p, n), s) for s in t["sites"] + t.get("traps", []) + t.get("neutral", []))
                     and not any(same_file(p, f) for f in changed)})
    return {"recall": len(found) / len(t["sites"]), "missed": [s for s in t["sites"] if s not in found],
            "traps": traps, "extras": extras}


def main():
    args = [a for a in sys.argv[1:] if a != "--score"]
    t = task(args[0])
    answer = Path(args[1] if len(args) > 1 else "answer.txt").read_text(errors="replace")
    s = score(t, answer)
    if "--score" in sys.argv:
        print(json.dumps(s))
    ok = s["recall"] == 1 and not s["traps"] and len(s["extras"]) <= EXTRAS
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
