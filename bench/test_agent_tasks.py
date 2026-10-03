import os
import subprocess
import tempfile
import tomllib
import unittest

TASKS = os.path.join(os.path.dirname(__file__), "agent", "tasks.toml")


def passes(check, answer):
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "answer.txt"), "w") as fh:
            fh.write(answer)
        return subprocess.run(["sh", "-c", check], cwd=d).returncode == 0


class AgentTaskTests(unittest.TestCase):
    def test_each_check_takes_its_reference_answer_and_nothing_less(self):
        with open(TASKS, "rb") as fh:
            tasks = tomllib.load(fh)["task"]
        self.assertEqual(len({t["id"] for t in tasks}), len(tasks))
        for t in tasks:
            with self.subTest(t["id"]):
                self.assertTrue(passes(t["check"], t["answer"]))
                self.assertFalse(passes(t["check"], ""))
                self.assertFalse(passes(t["check"], t["prompt"]))


if __name__ == "__main__":
    unittest.main()
