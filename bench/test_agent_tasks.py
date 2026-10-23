import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

AGENT = os.path.join(os.path.dirname(__file__), "agent")
sys.path.insert(0, AGENT)
from run import corpus_path, patches  # noqa: E402

TASKS = os.path.join(AGENT, "tasks.toml")


def tasks():
    with open(TASKS, "rb") as fh:
        return tomllib.load(fh)["task"]


def passes(check, answer):
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "answer.txt"), "w") as fh:
            fh.write(answer)
        env = {**os.environ, "AGENT_DIR": AGENT}
        return subprocess.run(["sh", "-c", check], cwd=d, env=env).returncode == 0


def patched(t, root, out):
    """The files a diff task's patches touch, copied from the corpus into
    `out` and patched there; `None` for a task without patches."""
    ps = patches(t)
    if not ps:
        return None
    for patch, _ in ps:
        for path in re.findall(r"^--- a/(\S+)", patch.read_text(errors="replace"), re.M):
            if not (out / path).exists():
                (out / path).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(root / path, out / path)
    for patch, _ in ps:
        subprocess.run(["git", "apply", str(patch)], cwd=out, check=True, capture_output=True)
    return out


class AgentTaskTests(unittest.TestCase):
    def test_each_check_takes_its_reference_answer_and_nothing_less(self):
        ts = tasks()
        self.assertEqual(len({t["id"] for t in ts}), len(ts))
        for t in ts:
            with self.subTest(t["id"]):
                self.assertTrue(t["incomplete"], "an incomplete answer the check must fail")
                cases = [(t["answer"], True), ("", False), (t["prompt"], False),
                         (re.sub(r":\d+", "", t["answer"]), False)]
                cases += [(answer, False) for answer in t["incomplete"]]
                for answer, want in cases:
                    self.assertEqual(passes(t["check"], answer), want,
                                     f"check {t['check']!r} on answer {answer!r}")

    def test_a_diff_answer_may_name_what_it_left_out(self):
        """Only `name at path:line` lines cite: prose that names a trap
        while explaining why it was left out does not."""
        for t in tasks():
            if not t.get("traps"):
                continue
            with self.subTest(t["id"]):
                trap = t["traps"][0]
                prose = f"{t['answer']}\n\nI left out {trap}, which is another definition's use."
                self.assertTrue(passes(t["check"], prose))
                self.assertFalse(passes(t["check"], f"{t['answer']}\n- x at {trap}"))

    def test_a_citation_may_carry_a_label_and_a_note(self):
        sys.path.insert(0, AGENT)
        from check_sites import CITE
        cites = lambda text: [(m.group(1), m.group(2)) for m in CITE.finditer(text)]
        self.assertEqual(cites("- Definition: f at a/b.py:3"), [("a/b.py", "3")])
        self.assertEqual(cites("- poll (`Future for Timeout<T>`) at a/t.rs:9"), [("a/t.rs", "9")])
        self.assertEqual(cites("f (in `g(x)`) at a/b.kt:4"), [("a/b.kt", "4")])
        self.assertEqual(cites("I left it out; it is at a/b.py:3."), [])

    def test_reference_answers_cite_lines_that_exist(self):
        """Each path:line a reference answer cites is in the pinned corpus,
        when the corpus is fetched; the line holds the name cited before it."""
        cited, fetched = 0, 0
        for t in tasks():
            root = corpus_path(t["corpus"])
            if not root.is_dir():
                continue
            fetched += 1
            found = re.findall(r"([\w.:]+)\W+(?:at |\()([\w./-]+/[\w.-]+):(\d+)", t["answer"])
            # a citation the pattern does not take (a bare file name) would go unchecked
            self.assertEqual(sorted(f"{p}:{n}" for _, p, n in found),
                             sorted(re.findall(r"[\w./-]+\.\w+:\d+", t["answer"])), t["id"])
            with tempfile.TemporaryDirectory() as tmp:
                after = patched(t, root, Path(tmp))
                # a trap names the same thing as the sites, in another definition's use
                name = found[0][0] if found else ""
                found += [(name, *p.rsplit(":", 1)) for p in t.get("traps", [])]
                for name, path, line in found:
                    with self.subTest(t["id"], path=path):
                        base = after if after and (after / path).exists() else root
                        lines = (base / path).read_text(errors="replace").splitlines()
                        self.assertGreaterEqual(len(lines), int(line))
                        last = re.split(r"::|\.", name)[-1]
                        self.assertRegex(lines[int(line) - 1], rf"\b{re.escape(last)}\b", f"{path}:{line}")
                        cited += 1
        if not fetched:
            self.skipTest("no corpus fetched")
        self.assertGreater(cited, 0, "corpora fetched, but no citation parsed")


if __name__ == "__main__":
    unittest.main()
