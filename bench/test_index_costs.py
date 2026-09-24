import sys
import unittest

import tempfile
from pathlib import Path

from index_costs import change, edit_target, flagged, manifest_path, p95, sample, summary


class IndexCostTests(unittest.TestCase):
    def test_summary_uses_the_nearest_rank_p95(self):
        values = list(range(1, 21))
        self.assertEqual(p95(values), 19)
        self.assertEqual(summary(values), {"median": 10.5, "p95": 19})

    def test_only_slowdowns_past_the_thresholds_are_flagged(self):
        before = {"median": 10.0, "p95": 20.0}
        self.assertFalse(flagged(change(before, {"median": 10.9, "p95": 23.9})))
        self.assertTrue(flagged(change(before, {"median": 11.2, "p95": 20.0})))
        self.assertTrue(flagged(change(before, {"median": 10.0, "p95": 24.2})))
        self.assertFalse(flagged(change(before, {"median": 5.0, "p95": 5.0})))

    def test_the_edit_target_is_a_stable_source_file(self):
        files = ["README.md", "src/c.rs", "src/a.rs", "src/b.rs", "docs/x.py"]
        self.assertEqual(edit_target(files, "rust"), "src/b.rs")
        self.assertEqual(edit_target(list(reversed(files)), "rust"), "src/b.rs")
        self.assertEqual(edit_target(["a.txt", "b.txt"], "zig"), "b.txt")

    def test_a_sample_reports_status_output_and_resources(self):
        wall, cpu, rss, code, out, err = sample(
            [sys.executable, "-c", "import sys; print('x'); sys.stderr.write('e'); sys.exit(3)"], ".", None)
        self.assertEqual((code, out, err), (3, b"x\n", b"e"))
        self.assertGreater(wall, 0)
        self.assertLessEqual(cpu, wall)
        # a Python interpreter's peak RSS is some MB, whatever the platform's unit
        self.assertTrue(1 < rss < 1000, rss)

    def test_the_manifest_is_found_in_either_layout(self):
        with tempfile.TemporaryDirectory() as d:
            old = Path(d, "old")
            (old / "session").mkdir(parents=True)
            (old / "manifest").write_text("{}")
            self.assertEqual(manifest_path(old), old / "manifest")
            new = Path(d, "new")
            (new / "v6").mkdir(parents=True)
            (new / "v6" / "manifest").write_text("{}")
            self.assertEqual(manifest_path(new), new / "v6" / "manifest")


if __name__ == "__main__":
    unittest.main()
