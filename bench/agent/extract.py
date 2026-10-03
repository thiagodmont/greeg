#!/usr/bin/env python3
"""Extract tool calls, tokens, turns and cost from `claude -p --output-format stream-json` logs.

    bench/agent/extract.py RUNS_DIR > runs.json

RUNS_DIR holds one subdirectory per run named <task>/<arm>/<n>/ with
`stream.jsonl` (the agent's stream-json output), `answer.txt` (the final
result text) and `check.txt` (`0` if the task's check passed). Emits one JSON
record per run: tool calls by name, search calls (rg/grep/greeg, by tool), the
characters the tools returned, input/output and cache tokens, number of turns,
wall time, cost and success.

Counting search calls in arm B (the hook rewrites `rg`/`grep` to `greeg`): the
`tool_use` block in the assistant message is what the model *asked for*, before
the PreToolUse hook ran, so counting it would report every hook-rewritten call
as `rg`. The executed command is taken, in this order of preference:

1. a hook record for the same tool_use id carrying `updatedInput.command`.
   Claude Code writes these to the session transcript
   (`~/.claude/projects/<cwd-slug>/<session_id>.jsonl`) as
   `{"type": "attachment", "attachment": {"type": "hook_success", "toolUseID": …,
   "stdout": "{\"hookSpecificOutput\": {\"updatedInput\": {\"command\": …}}}"}}`;
   they are not part of the `-p` stream. Copy that transcript next to the stream
   as `transcript.jsonl` (the session id is in the stream's `init` and `result`
   records) and it is read here; the same shape is honoured if it ever appears in
   `stream.jsonl` itself;
2. otherwise the `tool_result` text for that tool_use id: greeg's output carries
   its own fingerprint (the `NAME  N matches · M files` header, `by kind` facets,
   the `N of M hits · … tokens` footer, or `greeg:` stderr lines), rg's does not;
3. otherwise the command text as the model wrote it.

Each search count says how it was decided (`search_evidence`), so a run scored
from the model's intent alone is visible in the table.

`run.py` passes `--include-hook-events`, so the stream itself carries each
PreToolUse response (`{"type": "system", "subtype": "hook_response", …}`). A
response names no tool_use id: it goes to the earliest Bash call still waiting
whose description matches, else the earliest; a response that could belong to
more than one is counted as ambiguous. Calls the permission rules
denied (`permission_denied`) ran nothing and are counted apart.

A run is marked `invalid`, with reasons, when it has no `meta.json` (only
`run.py` runs are compared), when its `meta.json` says the workspace changed,
when the stream has no result, when arm A ran greeg, when a search in arm B or
C ran without a hook response, or when a hook response could belong to more
than one waiting call.
"""
import json, os, re, sys
from pathlib import Path

SEARCH = re.compile(r"^\s*(?:command\s+|\\)?(rg|grep|greeg)\b")
GREEG_FINGERPRINT = re.compile(r"(^|\n)(?:\S.*  [\d,]+ matches · [\d,]+ files|by kind |[\d,]+ of [\d,]+ hits · |greeg: |definitions \(|top hits\n)")


def search_tools_in(cmd):
    """Search tools invoked by a shell command, one per pipeline segment."""
    out = []
    for seg in re.split(r"\s*(?:\|\||&&|\||;)\s*", cmd or ""):
        m = SEARCH.match(seg)
        if m:
            out.append(m.group(1))
    return out


def hook_updates(lines):
    """{tool_use_id: updated command} from hook records (transcript attachments or stream records)."""
    upd = {}
    for l in lines:
        try:
            j = json.loads(l)
        except ValueError:
            continue
        att = j.get("attachment") if j.get("type") == "attachment" else j
        if not isinstance(att, dict) or not str(att.get("type", "")).startswith("hook"):
            continue
        tid = att.get("toolUseID") or att.get("tool_use_id")
        raw = att.get("stdout") or ""
        try:
            hso = json.loads(raw).get("hookSpecificOutput", {}) if raw.strip().startswith("{") else {}
        except ValueError:
            hso = {}
        cmd = ((hso.get("updatedInput") or {}).get("command")) or ((att.get("updatedInput") or {}).get("command"))
        if tid and cmd:
            upd[tid] = cmd
    return upd


def stream_hook_updates(lines):
    """{tool_use_id: executed command} from hook responses in the stream (a
    declined call maps to its own command), and how many responses could
    belong to more than one waiting call."""
    waiting = []  # (tool_use id, input) of Bash calls not yet answered by the hook
    upd = {}
    ambiguous = 0
    for l in lines:
        try:
            j = json.loads(l)
        except ValueError:
            continue
        if j.get("type") == "assistant":
            for blk in j.get("message", {}).get("content", []):
                if blk.get("type") == "tool_use" and blk.get("name") == "Bash":
                    waiting.append((blk.get("id"), blk.get("input") or {}))
        elif j.get("type") == "system" and j.get("subtype") == "hook_response" and j.get("hook_name") == "PreToolUse:Bash" and waiting:
            raw = j.get("stdout") or j.get("output") or ""
            try:
                new = (json.loads(raw).get("hookSpecificOutput") or {}).get("updatedInput") or {} if raw.strip() else {}
            except ValueError:
                new = {}
            same = [i for i, (_, inp) in enumerate(waiting) if "description" in new and inp.get("description") == new["description"]]
            if len(same) > 1 or (not same and len(waiting) > 1):
                ambiguous += 1
            tid, inp = waiting.pop(same[0] if same else 0)
            upd[tid] = new.get("command") or inp.get("command", "")
    return upd, ambiguous


def result_text(block):
    c = block.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "\n".join(x.get("text", "") for x in c if isinstance(x, dict))
    return ""


def read_run(d):
    rec = {"tools": {}, "search": {"rg": 0, "grep": 0, "greeg": 0}, "search_evidence": {"hook": 0, "output": 0, "intent": 0}, "denied": 0, "tool_output_chars": 0, "input_tokens": 0, "output_tokens": 0, "cache_creation": 0, "cache_read": 0, "turns": 0, "duration_ms": None, "cost_usd": None, "model": None, "result": None}
    try:
        lines = Path(d, "stream.jsonl").read_text().splitlines()
    except OSError:
        return None
    try:
        transcript = Path(d, "transcript.jsonl").read_text().splitlines()
    except OSError:
        transcript = []
    updates = hook_updates(lines + transcript)
    from_stream, rec["ambiguous_hooks"] = stream_hook_updates(lines)
    updates.update(from_stream)
    denied = set()
    pending = {}  # tool_use id -> command as the model wrote it (Bash only)
    results = {}  # tool_use id -> result text
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
            rec["cache_creation"] += u.get("cache_creation_input_tokens", 0)
            rec["cache_read"] += u.get("cache_read_input_tokens", 0)
            for blk in msg.get("content", []):
                if blk.get("type") == "tool_use":
                    name = blk.get("name", "?")
                    rec["tools"][name] = rec["tools"].get(name, 0) + 1
                    if name == "Bash":
                        pending[blk.get("id")] = (blk.get("input") or {}).get("command", "")
                    elif name == "Grep":
                        rec["search"]["rg"] += 1
                        rec["search_evidence"]["intent"] += 1
        elif t == "user":
            for blk in (j.get("message", {}).get("content") or []):
                if isinstance(blk, dict) and blk.get("type") == "tool_result":
                    results[blk.get("tool_use_id")] = result_text(blk)
                    rec["tool_output_chars"] += len(results[blk.get("tool_use_id")])
        elif t == "system" and j.get("subtype") == "init":
            rec["model"] = j.get("model")
        elif t == "system" and j.get("subtype") == "permission_denied":
            denied.add(j.get("tool_use_id"))
        elif t == "result":
            rec["result"] = j.get("subtype")
            rec["turns"] = j.get("num_turns", rec["turns"])
            rec["duration_ms"] = j.get("duration_ms")
            rec["cost_usd"] = j.get("total_cost_usd")
    rec["denied"] = len(denied)
    unhooked = 0
    for tid, intent_cmd in pending.items():
        if tid in denied:
            continue
        intended = search_tools_in(intent_cmd)
        if intended and tid not in updates:
            unhooked += len(intended)
        if tid in updates:
            found, how = search_tools_in(updates[tid]), "hook"
        elif intended and tid in results and GREEG_FINGERPRINT.search(results[tid]):
            # the output is greeg's: every rg/grep segment the model wrote was rewritten
            found, how = ["greeg" if x in ("rg", "grep") else x for x in intended], "output"
        else:
            found, how = intended, "intent"
        for tool in found:
            rec["search"][tool] += 1
            rec["search_evidence"][how] += 1
    try:
        rec["success"] = Path(d, "check.txt").read_text().strip() == "0"
    except OSError:
        rec["success"] = None
    rec["unhooked"] = unhooked
    return rec


def invalid(rec, d, arm):
    """Why a run cannot be compared, if it cannot."""
    why = []
    try:
        meta = json.loads(Path(d, "meta.json").read_text())
    except (OSError, ValueError):
        meta = None
        why.append("no readable meta.json (not a run.py run)")
    if meta and meta.get("workspace_dirty"):
        why.append("the agent changed the workspace")
    if rec["result"] is None:
        why.append("no result record")
    if arm == "A" and rec["search"]["greeg"]:
        why.append("arm A ran greeg")
    if arm != "A" and rec["unhooked"]:
        why.append(f"{rec['unhooked']} searches ran without a hook response")
    if rec["ambiguous_hooks"]:
        why.append(f"{rec['ambiguous_hooks']} hook responses could belong to more than one call")
    return why


def main():
    root = sys.argv[1]
    out = []
    for task in sorted(os.listdir(root)):
        for arm in sorted(os.listdir(os.path.join(root, task))):
            for n in sorted(os.listdir(os.path.join(root, task, arm))):
                d = os.path.join(root, task, arm, n)
                r = read_run(d)
                if r:
                    r.update({"task": task, "arm": arm, "run": n, "invalid": invalid(r, d, arm)})
                    out.append(r)
    json.dump(out, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
