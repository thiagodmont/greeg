import contextlib
import copy
import io
import json
import re
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import unquote

import bench
from report_catalog import ReportDataError, ReportDatasets, load_catalog, validate_dataset


def config_dataset():
    return {"protocol": 2, "runs": 2, "warmups": 0,
            "binaries": {label: {"version": label} for label in ("baseline", "candidate")},
            "results": [{"agent": "codex", "case": "installed_noop", **{
                label: {"median_ms": 1.0, "p95_ms": 2.0, "failed_invocations": 0, "contract": True}
                for label in ("baseline", "candidate")}}]}


class ReportValidationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.filename = "atomic-config-review-2026-09-23-darwin-arm64.json"
        self.data = config_dataset()
        override = patch.object(bench, "RESULTS", str(self.root))
        override.start()
        self.addCleanup(override.stop)
        self.store = ReportDatasets(self.root)
        self.entry = next(e for e in self.store.entries if e.filename == self.filename)
        validate_dataset(self.data, self.entry)

    def test_malformed_present_data_preserves_previous_report(self):
        variants = [("{broken", "JSON"), ("[]", "object")]
        variants.append(('{"runs": 1,' + json.dumps(self.data)[1:], "duplicate field"))
        for field, value in (("runs", "51"), ("protocol", 999), ("results", {})):
            data = copy.deepcopy(self.data)
            data[field] = value
            variants.append((json.dumps(data), field))
        data = copy.deepcopy(self.data)
        data["results"][0]["candidate"]["contract"] = "false"
        variants.append((json.dumps(data), "results"))
        output = self.root / "report.md"
        for payload, field in variants:
            with self.subTest(field=field):
                (self.root / self.filename).write_text(payload)
                output.write_text("previous report")
                with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, field) as error:
                    bench.report(SimpleNamespace(out=str(output)))
                self.assertIn(self.filename, str(error.exception))
                self.assertEqual(output.read_text(), "previous report")

    def test_missing_empty_and_null_measurements_remain_distinct(self):
        self.assertIsNone(self.store.get(self.entry))
        empty = copy.deepcopy(self.data)
        empty["results"] = []
        self.assertEqual(validate_dataset(empty, self.entry)["results"], [])
        data = copy.deepcopy(self.data)
        data["results"][0]["candidate"]["median_ms"] = None
        self.assertIsNone(validate_dataset(data, self.entry)["results"][0]["candidate"]["median_ms"])
        del data["results"][0]["candidate"]["median_ms"]
        with self.assertRaisesRegex(ReportDataError, r"results\[0\].candidate.median_ms"):
            validate_dataset(data, self.entry)

    def test_invalid_row_values_are_rejected_with_field_paths(self):
        for field, value in (("contract", 1), ("median_ms", True), ("median_ms", -1),
                             ("p95_ms", float("nan")), ("p95_ms", float("inf")), ("p95_ms", 10**400),
                             ("failed_invocations", -1), ("reply_tokens", "10")):
            data = copy.deepcopy(self.data)
            data["results"][0]["candidate"][field] = value
            with self.subTest(field=field, value=value), self.assertRaisesRegex(ReportDataError, field):
                validate_dataset(data, self.entry)
        data = copy.deepcopy(self.data)
        data["results"].append(data["results"][0])
        with self.assertRaisesRegex(ReportDataError, "unique group/case"):
            validate_dataset(data, self.entry)

    def test_cache_is_per_report_and_reads_each_dataset_once(self):
        path = self.root / self.filename
        path.write_text(json.dumps(self.data))
        first = self.store.get(self.entry)
        path.write_text("{broken")
        self.assertIs(self.store.get(self.entry), first)
        with self.assertRaisesRegex(ReportDataError, "JSON"):
            ReportDatasets(self.root).get(self.entry)

    def test_invalid_encoding_is_rejected(self):
        (self.root / self.filename).write_bytes(b"\xff")
        with self.assertRaisesRegex(ReportDataError, "cannot read dataset"):
            self.store.get(self.entry)

    def test_failure_count_cannot_exceed_runs(self):
        sample = self.data["results"][0]["candidate"]
        sample.update(failed_invocations=self.data["runs"], contract=False)
        validate_dataset(self.data, self.entry)
        sample["failed_invocations"] += 1
        with self.assertRaisesRegex(ReportDataError, r"candidate\.failed_invocations: expected count <= runs"):
            validate_dataset(self.data, self.entry)

    def test_failed_invocations_cannot_have_a_passing_contract(self):
        sample = self.data["results"][0]["candidate"]
        sample.update(failed_invocations=1, contract=False)
        validate_dataset(self.data, self.entry)
        sample["contract"] = True
        with self.assertRaisesRegex(ReportDataError, r"candidate\.contract: expected false when invocations failed"):
            validate_dataset(self.data, self.entry)

    def test_protocol_two_requires_failure_counts(self):
        data = copy.deepcopy(self.data)
        del data["results"][0]["candidate"]["failed_invocations"]
        with self.assertRaisesRegex(ReportDataError, "failed_invocations"):
            validate_dataset(data, self.entry)

    def test_cli_has_actionable_error_without_traceback(self):
        (self.root / self.filename).write_text("{broken")
        with patch("sys.argv", ["bench.py", "report", "--out", str(self.root / "out.md")]), \
                self.assertRaisesRegex(SystemExit, "report error:.*invalid JSON"):
            bench.main()
        self.assertFalse((self.root / "out.md").exists())


class CatalogTests(unittest.TestCase):
    def setUp(self):
        self.path = Path(bench.HERE) / "reports.toml"
        self.source = self.path.read_text()

    def invalid_catalog(self, source, message):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "reports.toml"
            path.write_text(source)
            with self.assertRaisesRegex(ReportDataError, message):
                load_catalog(path)

    def test_catalog_rejects_paths_duplicates_and_unknown_options(self):
        filename = "exact-search-matching-2026-09-22-darwin-arm64.json"
        for replacement in ("../escape.json", "/tmp/escape.json", "..\\\\escape.json"):
            with self.subTest(replacement=replacement):
                self.invalid_catalog(self.source.replace(filename, replacement), "filename")
        self.invalid_catalog(self.source.replace('id = "ranked_recheck"', 'id = "exact_defaults"'), "unique identifiers")
        self.invalid_catalog(self.source.replace('section = "matching"', 'section = "typo"', 1), "section")
        self.invalid_catalog(self.source.replace('version = 1', 'version = 99', 1), "version")
        self.invalid_catalog(self.source.replace('analysis = "thresholds"', 'analysis = "typo"', 1), "analysis")

    def test_recheck_references_are_validated(self):
        self.invalid_catalog(self.source.replace('recheck_of = "atomic_review"', 'recheck_of = "missing"'), "recheck_of")
        self.invalid_catalog(self.source.replace('recheck_of = "atomic_review"', 'recheck_of = "atomic_confirmation"'), "recheck_of")
        self.invalid_catalog(self.source.replace('recheck_of = "atomic_review"', 'recheck_of = "exact_defaults"'), "recheck_of")

    def test_pr_references_must_be_positive_integers(self):
        for value in ('0', '-1', 'true', '"13"', '13.5'):
            with self.subTest(value=value):
                self.invalid_catalog(self.source.replace('pr = 13', f'pr = {value}', 1), r"\.pr:")

    def test_all_registered_artifacts_validate(self):
        store = ReportDatasets(Path(bench.HERE) / "results")
        self.assertTrue(store.entries)
        for entry in store.entries:
            with self.subTest(dataset=entry.id):
                self.assertIsNotNone(store.get(entry))

    def test_matching_contract_applicability_cannot_break_counts(self):
        entry = next(e for e in load_catalog(self.path) if e.section == "matching")
        data = {"protocol": 1, "runs": 1, "warmups": 0, "platform": "test", "cpu_count": 1,
                "corpus": {"files": 1, "bytes": 10}, "ripgrep": "test",
                "binaries": {label: {"version": label} for label in ("baseline", "candidate")},
                "results": [{"backend": "scan", "case": "hit_files", "stdout_and_status_unchanged": True,
                             **{label: {"median_ms": 1.0, "p95_ms": 1.0, "rg_stdout_and_status_equal": True}
                                for label in ("baseline", "candidate")}}]}
        validate_dataset(data, entry)
        row = data["results"][0]
        row["baseline"]["rg_stdout_and_status_equal"] = None
        with self.assertRaisesRegex(ReportDataError, "baseline.rg_stdout_and_status_equal"):
            validate_dataset(data, entry)

    def test_catalog_controls_new_dataset_title_order_and_analysis(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "reports.toml"
            path.write_text(self.source + '\n[[datasets]]\nid = "new_run"\nsection = "hook_config"\n'
                            'filename = "new-run.json"\ntitle = "New measurement"\npr = 19\nanalysis = "thresholds"\n')
            data = config_dataset()
            (root / "new-run.json").write_text(json.dumps(data))
            store = ReportDatasets(root, path)
            self.assertEqual(store.section("hook_config")[-1].id, "new_run")
            text = "\n".join(bench.hook_config_report(str(root), store))
            self.assertIn("## New measurement", text)
            self.assertIn("Originating PR: [#19](https://github.com/thiagodmont/greeg/pull/19).", text)
            self.assertIn("Maximum median/p95 increases", text)
            self.assertNotIn("Atomic configuration:", text)
            for link in re.findall(r"\]\(([^)]+\.json)\)", text):
                self.assertTrue((root / unquote(link)).is_file(), link)


if __name__ == "__main__":
    unittest.main()
