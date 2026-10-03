import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "agent"))
import analyze  # noqa: E402


def record(task, arm, run, success=True, cost=0.1):
    return {"task": task, "arm": arm, "run": run, "success": success, "cost_usd": cost,
            "turns": 2, "search": {"rg": 1, "grep": 0, "greeg": 0},
            "search_evidence": {"hook": 0, "output": 0, "intent": 1}, "tool_output_chars": 10,
            "output_tokens": 5, "input_tokens": 1, "cache_creation": 0, "cache_read": 0,
            "duration_ms": 1000, "invalid": []}


def analyzed(runs):
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as fh:
        json.dump(runs, fh)
    out = io.StringIO()
    argv = sys.argv
    sys.argv = ["analyze.py", fh.name, "--pairs", "100"]
    try:
        with redirect_stdout(out):
            analyze.main()
    finally:
        sys.argv = argv
        os.unlink(fh.name)
    return out.getvalue()


class AgentAnalyzeTests(unittest.TestCase):
    def test_unscored_runs_and_unknown_costs_are_not_counted(self):
        out = analyzed([record("t", "A", "1"), record("t", "A", "2", success=None, cost=None),
                        record("t", "B", "1"), record("t", "B", "2")])
        row = next(l for l in out.splitlines() if l.startswith("t "))
        self.assertIn("1/1", row)
        self.assertIn("0.100", row)

    def test_without_a_valid_baseline_it_says_so(self):
        bad = record("t", "A", "1")
        bad["invalid"] = ["no result record"]
        with self.assertRaises(SystemExit) as e:
            analyzed([bad, record("t", "B", "1")])
        self.assertIn("arm A", str(e.exception))


if __name__ == "__main__":
    unittest.main()
