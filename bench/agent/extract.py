#!/usr/bin/env python3
"""Extract tool calls, tokens, turns and cost from `claude -p --output-format stream-json` logs.

    bench/agent/extract.py RUNS_DIR > runs.json

RUNS_DIR holds one subdirectory per run named <task>/<arm>/<n>/ with
`stream.jsonl` (the agent's stream-json output), `answer.txt` (the final
result text) and `check.txt` (`0` if the task's check passed). Emits one JSON
record per run: tool calls by name, search calls (rg/grep/greeg, by tool),
input/output tokens, number of turns, wall time, cost and success.
"""
import json, os, re, sys

SEARCH = re.compile(r"^\s*(rg|grep|greeg)\b")


def read_run(d):
    rec = {"tools": {}, "search": {"rg": 0, "grep": 0, "greeg": 0}, "input_tokens": 0, "output_tokens": 0, "cache_read": 0, "turns": 0, "duration_ms": None, "cost_usd": None}
    try:
        lines = open(os.path.join(d, "stream.jsonl")).read().splitlines()
    except OSError:
        return None
    for l in lines:
        try:
            j = json.loads(l)
        except ValueError:
            continue
        t = j.get("type")
        if t == "assistant":
            msg = j.get("message", {})
            u = msg.get("usage", {})
            rec["input_tokens"] += u.get("input_tokens", 0)
            rec["output_tokens"] += u.get("output_tokens", 0)
            rec["cache_read"] += u.get("cache_read_input_tokens", 0)
            for blk in msg.get("content", []):
                if blk.get("type") == "tool_use":
                    name = blk.get("name", "?")
                    rec["tools"][name] = rec["tools"].get(name, 0) + 1
                    if name == "Bash":
                        cmd = (blk.get("input") or {}).get("command", "")
                        for seg in re.split(r"\s*(?:\|\||&&|\||;)\s*", cmd):
                            m = SEARCH.match(seg)
                            if m:
                                rec["search"][m.group(1)] += 1
                    elif name == "Grep":
                        rec["search"]["rg"] += 1
        elif t == "result":
            rec["turns"] = j.get("num_turns", rec["turns"])
            rec["duration_ms"] = j.get("duration_ms")
            rec["cost_usd"] = j.get("total_cost_usd")
    try:
        rec["success"] = open(os.path.join(d, "check.txt")).read().strip() == "0"
    except OSError:
        rec["success"] = None
    return rec


def main():
    root = sys.argv[1]
    out = []
    for task in sorted(os.listdir(root)):
        for arm in sorted(os.listdir(os.path.join(root, task))):
            for n in sorted(os.listdir(os.path.join(root, task, arm))):
                d = os.path.join(root, task, arm, n)
                r = read_run(d)
                if r:
                    r.update({"task": task, "arm": arm, "run": n})
                    out.append(r)
    json.dump(out, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
