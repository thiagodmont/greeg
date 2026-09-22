import copy
import json
import subprocess
import unittest

from matching import definition_contract, json_exact_contract, stable_stdout


class JsonContractTests(unittest.TestCase):
    def setUp(self):
        self.match = {"type": "match", "data": {
            "path": {"text": "src/a.rs"}, "lines": {"text": "fn load_config() {}\n"},
            "line_number": 1, "absolute_offset": 0,
            "submatches": [{"match": {"text": "load_config"}, "start": 3, "end": 14}],
        }}
        self.summary = {"type": "summary", "data": {
            "elapsed_total": {"secs": 1}, "stats": {"elapsed": {"secs": 1}, "matched_lines": 1},
        }}
        self.footer = {"type": "footer", "data": {"elapsed_ms": 1, "rung": "exact", "hits_total": 1}}

    def output(self, rows, status=0):
        return subprocess.CompletedProcess([], status, b"\n".join(json.dumps(r).encode() for r in rows), b"")

    def test_normalization_ignores_only_timing(self):
        rows = [self.match, self.summary, self.footer]
        before = stable_stdout(self.output(rows).stdout, True)
        self.summary["data"]["elapsed_total"] = {"secs": 99}
        self.summary["data"]["stats"]["elapsed"] = {"secs": 99}
        self.footer["data"]["elapsed_ms"] = 99
        self.assertEqual(before, stable_stdout(self.output(rows).stdout, True))
        self.footer["data"]["hits_total"] = 99
        self.assertNotEqual(before, stable_stdout(self.output(rows).stdout, True))

    def test_contract_detects_wrong_matches_status_rung_and_counts(self):
        oracle = self.output([self.match, self.summary])
        rows = [self.match, self.summary, self.footer]
        self.assertTrue(json_exact_contract(self.output(rows), oracle))
        for field, value in [("rung", "case-insensitive"), ("hits_total", 0)]:
            altered = copy.deepcopy(rows)
            altered[-1]["data"][field] = value
            self.assertFalse(json_exact_contract(self.output(altered), oracle))
        altered = copy.deepcopy(rows)
        altered[0]["data"]["submatches"][0]["start"] = 4
        self.assertFalse(json_exact_contract(self.output(altered), oracle))
        self.assertFalse(json_exact_contract(self.output(rows, 1), oracle))
        self.assertFalse(json_exact_contract(self.output(rows[1:]), oracle))

    def test_empty_json_contract_requires_exact_footer(self):
        self.summary["data"]["stats"]["matched_lines"] = 0
        self.footer["data"]["hits_total"] = 0
        oracle = self.output([self.summary], 1)
        self.assertTrue(json_exact_contract(self.output([self.summary, self.footer], 1), oracle))
        self.assertFalse(json_exact_contract(self.output([self.summary], 1), oracle))

    def test_definition_contract_rejects_missing_or_relaxed_paths(self):
        oracle = subprocess.CompletedProcess([], 0, b"src/unit_000.rs\n", b"")
        actual = subprocess.CompletedProcess([], 0, b"def load_config 1 of 1 definitions\nsrc/unit_000.rs\n  97 fn load_config()\n", b"")
        self.assertTrue(definition_contract(actual, oracle))
        actual.stdout = b"no definitions\n"
        self.assertFalse(definition_contract(actual, oracle))
        actual.stdout = b"matched case-insensitive\nsrc/unit_000.rs\n"
        self.assertFalse(definition_contract(actual, oracle))


if __name__ == "__main__":
    unittest.main()
