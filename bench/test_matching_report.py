import contextlib
import copy
import io
import json
from pathlib import Path
import re
import shlex
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import unquote

import bench


class MatchingReportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.results = self.root / "bench results"
        self.results.mkdir()
        self.matrix_name = "w01a-matching-darwin-arm64.json"
        self.matrix = json.loads((Path(bench.RESULTS) / self.matrix_name).read_text())
        self.followup = json.loads((Path(bench.RESULTS) / "exact-search-json-darwin-arm64.json").read_text())
        self.recheck_name = "w01a-ranked-recheck-darwin-arm64.json"
        recheck = Path(bench.RESULTS) / self.recheck_name
        (self.results / self.recheck_name).write_bytes(recheck.read_bytes())
        self.addCleanup(patch.stopall)
        patch.object(bench, "RESULTS", str(self.results)).start()
        patch.object(bench, "ROOT", str(self.root)).start()

    def render(self, matrix=None, relative="references/BENCH.md", default=False):
        (self.results / self.matrix_name).write_text(json.dumps(self.matrix if matrix is None else matrix))
        output = self.root / relative
        output.parent.mkdir(parents=True, exist_ok=True)
        with contextlib.redirect_stdout(io.StringIO()):
            bench.report(SimpleNamespace(out=None if default else str(output)))
        return output, output.read_text()

    def test_partial_case_selections(self):
        for cases in [[], ["ranked_hit"], ["hit_files"], ["absent_files"], ["ranked_discovery"]]:
            with self.subTest(cases=cases):
                matrix = copy.deepcopy(self.matrix)
                matrix["results"] = [r for r in matrix["results"] if r["case"] in cases]
                _, text = self.render(matrix)
                self.assertNotIn("0/0", text)

    def test_unmeasured_tokens(self):
        self.matrix["tokenizer"] = None
        for row in self.matrix["results"]:
            for label in ("baseline", "candidate"):
                row[label]["tokens_both_streams"] = None
        _, text = self.render()
        self.assertNotIn("Tokens count stdout plus stderr with `o200k_base`", text)
        self.assertNotIn("None", text)
        self.assertIn("n/a", text)

    def test_links_resolve_from_each_output_directory(self):
        for relative in ["references/BENCH.md", "BENCH.md", "export dir/deep/report.md"]:
            with self.subTest(relative=relative):
                output, text = self.render(relative=relative)
                links = re.findall(r"\]\(([^)]+\.json)\)", text)
                self.assertEqual(len(links), 2)
                for link in links:
                    self.assertTrue((output.parent / unquote(link)).is_file(), link)

    def test_repeat_generation_and_default_path(self):
        _, first = self.render(default=True)
        _, second = self.render(default=True)
        self.assertEqual(first, second)
        self.assertEqual(first.count("## W01a:"), 1)
        commands = re.findall(r"`(python3 bench/matching.py [^`]+)`", first)
        self.assertEqual(len(commands), 2)
        for command in commands:
            args = shlex.split(command)
            self.assertEqual(args[2:4], ["BASELINE", "CANDIDATE"])
            self.assertIn("--tokens", args)
            self.assertTrue(args[args.index("--output") + 1].endswith(".json"))

    def test_links_resolve_through_a_symlinked_output_directory(self):
        target = self.root / "one" / "two"
        target.mkdir(parents=True)
        (self.root / "alias").symlink_to(target, target_is_directory=True)
        output, text = self.render(relative="alias/report.md")
        for link in re.findall(r"\]\(([^)]+\.json)\)", text):
            self.assertTrue((output.parent / unquote(link)).is_file(), link)

    def test_followup_report_without_tokens_and_custom_path(self):
        self.followup["tokenizer"] = None
        for row in self.followup["results"]:
            for label in ("baseline", "candidate"):
                row[label]["tokens_both_streams"] = None
        (self.results / "exact-search-json-darwin-arm64.json").write_text(json.dumps(self.followup))
        output, text = self.render(relative="export dir/report.md")
        self.assertIn("## JSON exact-default coverage", text)
        self.assertIn("n/a → n/a", text)
        self.assertNotIn("None", text)
        for link in re.findall(r"\]\(([^)]+\.json)\)", text):
            self.assertTrue((output.parent / unquote(link)).is_file(), link)


if __name__ == "__main__":
    unittest.main()
