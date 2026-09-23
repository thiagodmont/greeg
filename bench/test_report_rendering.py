import copy
from dataclasses import replace
from datetime import datetime
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import bench
from report_catalog import Dataset
from reporting import latency_summary, paired_table, report_heading


class ConfigReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.results = Path(self.temp.name)
        self.original = "atomic-config-2026-09-23-darwin-arm64.json"
        self.review = "atomic-config-review-2026-09-23-darwin-arm64.json"
        self.confirmation = "atomic-config-review-confirmation-2026-09-23-darwin-arm64.json"
        self.data = json.loads((Path(bench.RESULTS) / self.original).read_text())
        self.patch = patch.object(bench, "RESULTS", str(self.results))
        self.patch.start()
        self.addCleanup(self.patch.stop)

    def render(self, filename, data):
        (self.results / filename).write_text(json.dumps(data))
        return "\n".join(bench.hook_config_dataset_report(str(self.results), filename, "fixture"))

    def test_empty_rows_are_skipped_and_missing_rows_are_invalid(self):
        for filename in (self.original, self.review, self.confirmation):
            for rows in ([], None):
                data = copy.deepcopy(self.data)
                if rows is None:
                    del data["results"]
                else:
                    data["results"] = rows
                with self.subTest(filename=filename, rows=rows):
                    if rows is None:
                        with self.assertRaisesRegex(ValueError, "results"):
                            self.render(filename, data)
                    else:
                        self.assertEqual(self.render(filename, data), "")

    def test_partial_atomic_cases_do_not_claim_both_groups(self):
        for case in ("mixed_uninstall", "installed_noop"):
            data = copy.deepcopy(self.data)
            data["results"] = [r for r in data["results"] if r["case"] == case]
            with self.subTest(case=case):
                text = self.render(self.original, data)
                self.assertIn("Configuration contracts", text)
                self.assertNotIn("**Latency investigation:**", text)

    def test_recheck_without_initial_run_does_not_claim_it_exists(self):
        self.render(self.confirmation, self.data)
        text = "\n".join(bench.hook_config_report(str(self.results)))
        self.assertNotIn("Both runs use the same binary digests", text)
        self.assertNotIn("The initial 51-pair run", text)

    def test_recheck_checks_recorded_binary_identity(self):
        self.render(self.review, self.data)
        for digest in (self.data["binaries"]["candidate"]["sha256"], "0" * 64, None):
            data = copy.deepcopy(self.data)
            data["binaries"]["candidate"]["sha256"] = digest
            self.render(self.confirmation, data)
            text = "\n".join(bench.hook_config_report(str(self.results)))
            with self.subTest(digest=digest):
                if digest == self.data["binaries"]["candidate"]["sha256"]:
                    self.assertIn("Both runs use the same binary digests", text)
                else:
                    self.assertIn("not a controlled recheck", text)

    def test_failed_review_invocations_do_not_produce_threshold_claims(self):
        self.data["results"][0]["candidate"]["failed_invocations"] = 1
        self.data["results"][0]["candidate"]["contract"] = False
        self.data["results"][0]["median_change_percent"] = None
        self.data["results"][0]["p95_change_percent"] = None
        text = self.render(self.review, self.data)
        self.assertIn("Invocation failures: 1", text)
        self.assertNotIn("Maximum median/p95 increases", text)

    def test_undefined_timings_render_without_comparisons(self):
        self.data["results"][0]["baseline"]["median_ms"] = None
        text = self.render(self.review, self.data)
        self.assertIn("n/a →", text)
        self.assertNotIn("Maximum median/p95 increases", text)


class PairedRenderingTests(unittest.TestCase):
    def test_headings_link_known_prs_and_do_not_guess_missing_ones(self):
        entry = Dataset("test", "hook", "test.json", "Measurement")
        self.assertEqual(report_heading(entry), ["## Measurement", ""])
        self.assertEqual(report_heading(replace(entry, notes="Run-specific limits.")),
                         ["## Measurement", "", "Run-specific limits.", ""])
        self.assertEqual(report_heading(replace(entry, title="Recheck", pr=18), level=3),
                         ["### Recheck", "", "Originating PR: [#18](https://github.com/thiagodmont/greeg/pull/18).", ""])

    def test_timestamp_distinguishes_recorded_time_from_commit_estimate(self):
        entry = Dataset("test", "hook", "test.json", "Measurement",
                        measured_at=datetime.fromisoformat("2026-09-22T22:59:39-04:00"),
                        timestamp_source="measurement")
        self.assertIn("Measurement time: 2026-09-23T02:59:39Z (recorded).", report_heading(entry))
        entry = replace(entry, timestamp_source="first_commit", timestamp_commit="a" * 40)
        rendered = "\n".join(report_heading(entry))
        self.assertIn("2026-09-23T02:59:39Z (estimate from first dataset commit", rendered)
        self.assertIn("[aaaaaaa](https://github.com/thiagodmont/greeg/commit/" + "a" * 40 + ")", rendered)
        self.assertIn("execution time was not recorded", rendered)

    def row(self, median=100, p95=100):
        return {"agent": "codex", "case": "installed_noop",
                "baseline": {"median_ms": 100, "p95_ms": 100, "tokens": 0},
                "candidate": {"median_ms": median, "p95_ms": p95, "tokens": None}}

    def test_latency_thresholds_are_strict_and_count_each_case_once(self):
        rows = [self.row(110, 120), self.row(110.01, 100), self.row(100, 120.01), self.row(120, 130)]
        summary = latency_summary(rows)
        self.assertEqual(summary["flagged"], 3)
        self.assertAlmostEqual(summary["median_max"], 20)
        self.assertAlmostEqual(summary["p95_max"], 30)

    def test_empty_failed_or_undefined_latencies_cannot_pass(self):
        self.assertIsNone(latency_summary([]))
        for value in (None, 0, -1, float("nan"), float("inf")):
            row = self.row()
            row["baseline"]["median_ms"] = value
            with self.subTest(value=value):
                self.assertIsNone(latency_summary([row]))
        row = self.row()
        row["candidate"]["failed_invocations"] = 1
        self.assertIsNone(latency_summary([row]))

    def test_table_distinguishes_zero_and_unmeasured_tokens(self):
        rows = [self.row()]
        measured = paired_table(rows, "agent", "Host", tokenizer="test", token_key="tokens")
        self.assertEqual(measured[-1], "| codex | installed noop | 100.000 → 100.000 | 100.000 → 100.000 | 0 → n/a |")
        unmeasured = paired_table(rows, "agent", "Host", token_key="tokens")
        self.assertTrue(unmeasured[-1].endswith("| n/a → n/a |"))
        self.assertNotIn("Tokens", "\n".join(paired_table(rows, "agent", "Host")))


class MatchingReviewReportTests(unittest.TestCase):
    def test_run_specific_notes_stay_under_their_own_heading(self):
        report = "\n".join(bench.matching_review_report(bench.RESULTS))
        specific = report.index("uses prefix ranges")
        self.assertLess(report.index("## Review fixes: optimized lookup recheck"), specific)
        self.assertLess(specific, report.index("## Quick correctness fixes: exact-search regression"))
        self.assertTrue(report.rstrip().endswith("not general symbol-resolution accuracy."))


if __name__ == "__main__":
    unittest.main()

