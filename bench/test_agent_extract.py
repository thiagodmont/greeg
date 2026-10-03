import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "agent"))
from extract import read_run, invalid  # noqa: E402


def bash(tid, command, description):
    return {"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": tid, "name": "Bash",
         "input": {"command": command, "description": description}}]}}


def hook(command=None, description=None):
    out = ""
    if command:
        out = json.dumps({"hookSpecificOutput": {"updatedInput": {"command": command, "description": description}}})
    return {"type": "system", "subtype": "hook_response", "hook_name": "PreToolUse:Bash", "stdout": out}


def result(tid, text):
    return {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": text}]}}


END = {"type": "result", "subtype": "success", "num_turns": 3, "duration_ms": 1000, "total_cost_usd": 0.1}


def run(records, arm, meta=None, check="0"):
    with tempfile.TemporaryDirectory() as d:
        return in_dir(d, records, arm, meta, check)


def in_dir(d, records, arm, meta, check):
    with open(os.path.join(d, "stream.jsonl"), "w") as fh:
        fh.write("\n".join(json.dumps(r) for r in records))
    with open(os.path.join(d, "check.txt"), "w") as fh:
        fh.write(check)
    if meta is not None:
        with open(os.path.join(d, "meta.json"), "w") as fh:
            json.dump(meta, fh)
    rec = read_run(d)
    return rec, invalid(rec, d, arm)


class AgentExtractTests(unittest.TestCase):
    def test_hook_responses_in_the_stream_decide_what_ran(self):
        rec, why = run([
            bash("a", "rg -n foo src", "first"),
            bash("b", "rg -n bar | head", "second"),
            hook("greeg bar", "second"),
            hook("greeg foo src", "first"),
            result("a", "x"), result("b", "y"), END], "B", meta={})
        self.assertEqual(rec["search"], {"rg": 0, "grep": 0, "greeg": 2})
        self.assertEqual(rec["search_evidence"]["hook"], 2)
        self.assertEqual(why, [])

    def test_a_declined_call_keeps_its_own_command(self):
        rec, why = run([bash("a", "grep -r foo .", "d"), hook(), result("a", "x"), END], "B", meta={})
        self.assertEqual(rec["search"]["grep"], 1)
        self.assertEqual(why, [])

    def test_denied_calls_are_counted_apart(self):
        rec, _ = run([bash("a", "rg foo", "d"), hook("greeg foo", "d"),
                      {"type": "system", "subtype": "permission_denied", "tool_use_id": "a"},
                      result("a", "denied"), END], "B", meta={})
        self.assertEqual(rec["denied"], 1)
        self.assertEqual(sum(rec["search"].values()), 0)

    def test_a_response_goes_to_the_call_its_description_names(self):
        # the response is for the second call; if it went to the first, the
        # two-search command would count as unhooked instead of one search
        _, why = run([
            bash("a", "rg x", "one"),
            bash("b", "rg y; rg z", "two"),
            hook("greeg y; greeg z", "two"),
            result("a", "x"), result("b", "y"), END], "B", meta={})
        self.assertEqual(why, ["1 searches ran without a hook response"])

    def test_a_response_that_fits_two_calls_is_ambiguous(self):
        rec, why = run([
            bash("a", "rg x", "same"), bash("b", "rg y", "same"),
            hook("greeg x", "same"), hook("greeg y", "same"),
            result("a", "x"), result("b", "y"), END], "B", meta={})
        self.assertEqual(rec["ambiguous_hooks"], 1)
        self.assertIn("1 hook responses could belong to more than one call", why)

    def test_a_transcript_attachment_names_its_call(self):
        with tempfile.TemporaryDirectory() as d:
            transcript = {"type": "attachment", "attachment": {
                "type": "hook_success", "toolUseID": "a",
                "stdout": json.dumps({"hookSpecificOutput": {"updatedInput": {"command": "greeg foo"}}})}}
            with open(os.path.join(d, "transcript.jsonl"), "w") as fh:
                fh.write(json.dumps(transcript))
            rec, _ = in_dir(d, [bash("a", "rg foo", "d"), result("a", "x"), END], "B", {}, "0")
        self.assertEqual(rec["search"]["greeg"], 1)
        self.assertEqual(rec["search_evidence"]["hook"], 1)

    def test_greeg_output_shows_a_rewrite_without_a_hook_record(self):
        rec, _ = run([bash("a", "rg foo", "d"), result("a", "foo  3 matches · 2 files\n"), END], "B")
        self.assertEqual(rec["search"]["greeg"], 1)
        self.assertEqual(rec["search_evidence"]["output"], 1)

    def test_runs_that_cannot_be_compared_say_why(self):
        _, why = run([bash("a", "rg foo", "d"), result("a", "x"), END], "B", meta={})
        self.assertEqual(why, ["1 searches ran without a hook response"])
        _, why = run([bash("a", "greeg foo", "d"), result("a", "x"), END], "A", meta={})
        self.assertEqual(why, ["arm A ran greeg"])
        _, why = run([END], "A", meta={"workspace_dirty": True})
        self.assertEqual(why, ["the agent changed the workspace"])
        _, why = run([bash("a", "rg foo", "d")], "A", meta={})
        self.assertEqual(why, ["no result record"])
        _, why = run([END], "A")
        self.assertEqual(why, ["no readable meta.json (not a run.py run)"])


if __name__ == "__main__":
    unittest.main()
