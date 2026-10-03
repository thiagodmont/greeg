import os
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "agent"))
from run import corpus_path  # noqa: E402

TASKS = os.path.join(os.path.dirname(__file__), "agent", "tasks.toml")


def tasks():
    with open(TASKS, "rb") as fh:
        return tomllib.load(fh)["task"]


def passes(check, answer):
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "answer.txt"), "w") as fh:
            fh.write(answer)
        return subprocess.run(["sh", "-c", check], cwd=d).returncode == 0


class AgentTaskTests(unittest.TestCase):
    def test_each_check_takes_its_reference_answer_and_nothing_less(self):
        ts = tasks()
        self.assertEqual(len({t["id"] for t in ts}), len(ts))
        for t in ts:
            with self.subTest(t["id"]):
                for answer, want in ((t["answer"], True), ("", False), (t["prompt"], False)):
                    self.assertEqual(passes(t["check"], answer), want,
                                     f"check {t['check']!r} on answer {answer!r}")

    def test_reference_answers_cite_lines_that_exist(self):
        """Each path:line a reference answer cites is in the pinned corpus,
        when the corpus is fetched; the line holds the name cited before it."""
        cited, fetched = 0, 0
        for t in tasks():
            root = corpus_path(t["corpus"])
            if not root.is_dir():
                continue
            fetched += 1
            for name, path, line in re.findall(r"([\w.:]+)\W+(?:at |\()([\w./-]+/[\w.-]+):(\d+)", t["answer"]):
                with self.subTest(t["id"], path=path):
                    lines = (root / path).read_text(errors="replace").splitlines()
                    self.assertGreaterEqual(len(lines), int(line))
                    last = re.split(r"::|\.", name)[-1]
                    self.assertRegex(lines[int(line) - 1], rf"\b{re.escape(last)}\b", f"{path}:{line}")
                    cited += 1
        if not fetched:
            self.skipTest("no corpus fetched")
        self.assertGreater(cited, 0, "corpora fetched, but no citation parsed")


if __name__ == "__main__":
    unittest.main()
