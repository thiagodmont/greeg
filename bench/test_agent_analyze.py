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

    def test_an_invalid_run_takes_its_pairs_out(self):
        bad = record("t", "B", "1", cost=0.5)
        bad["invalid"] = ["no result record"]
        out = analyzed([record("t", "A", "1", cost=0.3), bad,
                        record("t", "A", "2"), record("t", "B", "2")])
        self.assertIn("left out: t A #1: a run it pairs with is invalid", out)
        row = next(l for l in out.splitlines() if l.startswith("t "))
        self.assertEqual(row.split()[1:], ["1/1", "0.100", "2.0", "1/1", "0.100", "2.0"])

    def test_a_run_without_a_pair_in_every_arm_is_left_out(self):
        out = analyzed([record("t", "A", "1"), record("t", "B", "1"), record("t", "A", "2", cost=0.5)])
        self.assertIn("left out: t A #2: no run in arm B", out)
        row = next(l for l in out.splitlines() if l.startswith("t "))
        self.assertEqual(row.split()[1:], ["1/1", "0.100", "2.0", "1/1", "0.100", "2.0"])

    def test_success_rates_get_a_wilson_interval(self):
        lo, hi = analyze.wilson(0, 10)
        self.assertAlmostEqual(lo, 0.0, places=6)
        self.assertAlmostEqual(hi, 0.2775, places=3)
        lo, hi = analyze.wilson(10, 10)
        self.assertAlmostEqual(lo, 0.7225, places=3)
        out = analyzed([record("t", "A", "1"), record("t", "A", "2", success=False),
                        record("t", "B", "1"), record("t", "B", "2")])
        self.assertIn("success A: 1/2 (50 %, 95 % Wilson [9, 91])", out)
        self.assertIn("success B: 2/2 (100 %, 95 % Wilson [34, 100])", out)

    def test_runs_of_one_task_are_not_independent_evidence(self):
        # every run of a task agrees: four pairs per task, but only two tasks
        runs = []
        for task, cost in [("t1", 0.1), ("t2", 0.5)]:
            for n in "1234":
                runs += [record(task, "A", n, cost=0.2), record(task, "B", n, cost=cost)]
        line = next(l for l in analyzed(runs).splitlines() if l.startswith("cost USD"))
        pairs, tasks = line.split("[")[1:3]
        width = lambda i: float(i.split(",")[1].split("]")[0]) - float(i.split(",")[0])
        self.assertGreater(width(tasks), width(pairs))
        self.assertIn("n=8 tasks=2", line)

    def test_held_out_tasks_are_reported_apart(self):
        with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as fh:
            fh.write('[[task]]\nid = "h"\nheld_out = true\n[[task]]\nid = "d"\n')
        argv = sys.argv
        try:
            sys.argv = ["analyze.py", "-", "--pairs", "100", "--tasks", fh.name]
            runs = [record("d", "A", "1"), record("d", "B", "1"),
                    record("h", "A", "1", success=False), record("h", "B", "1")]
            with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as rf:
                json.dump(runs, rf)
            sys.argv[1] = rf.name
            out = io.StringIO()
            with redirect_stdout(out):
                analyze.main()
        finally:
            sys.argv = argv
            os.unlink(fh.name)
            os.unlink(rf.name)
        dev, test = out.getvalue().split("== held-out tasks: 1 tasks")
        self.assertIn("== development tasks: 1 tasks", dev)
        self.assertIn("success A: 1/1", dev)
        self.assertIn("success A: 0/1", test)

    def test_without_a_valid_baseline_it_says_so(self):
        bad = record("t", "A", "1")
        bad["invalid"] = ["no result record"]
        with self.assertRaises(SystemExit) as e:
            analyzed([bad, record("t", "B", "1")])
        self.assertIn("arm A", str(e.exception))


if __name__ == "__main__":
    unittest.main()
