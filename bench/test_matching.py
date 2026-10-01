import copy
import json
import subprocess
import unittest

from matching import (definition_contract, json_exact_contract, rg_dialect_contract,
                      rg_dialect_query, stable_stdout)


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

    def test_definition_contract_rejects_every_unexpected_path(self):
        oracle = subprocess.CompletedProcess([], 0, b"src/unit_000.rs\n", b"")
        valid = b"def load_config 1 of 1 definitions\nsrc/unit_000.rs\n  97 fn load_config()\n"
        for extra in [
            b"src/unexpected.rs", b"other/unit_000.rs", b"/tmp/unit_000.rs",
            b'"src/unit_000.rs"', b"  src/unit_000.rs", b"  97 /tmp/unit_000.rs", b"src/unit_000.rs:97",
            b"src/unit_000.rs [unexpected]", b"src/unit_000.rs",
        ]:
            with self.subTest(extra=extra):
                actual = subprocess.CompletedProcess([], 0, valid + extra + b"\nnext: refs load_config\n", b"")
                self.assertFalse(definition_contract(actual, oracle))
        miss = subprocess.CompletedProcess([], 1, b"def load_confiq  no definition found (scan)\nnext: greeg load_confiq\n", b"")
        empty = subprocess.CompletedProcess([], 1, b"", b"")
        self.assertTrue(definition_contract(miss, empty))
        miss.stdout += b"src/unexpected.rs\n"
        self.assertFalse(definition_contract(miss, empty))


class RgDialectTests(unittest.TestCase):
    MATCH = (b'{"type":"match","data":{"path":{"text":"src/a.rs"},"lines":{"text":"load_config\\n"},'
             b'"line_number":1,"absolute_offset":0,"submatches":[]}}')
    END = (b'{"type":"end","data":{"path":{"text":"src/a.rs"},"binary_offset":null,"stats":{"elapsed":'
           b'{"secs":0,"nanos":%d,"human":"x"},"searches":1,"searches_with_match":1,"bytes_searched":%d,'
           b'"bytes_printed":9,"matched_lines":1,"matches":1}}}')
    SUMMARY = (b'{"data":{"elapsed_total":{"human":"x","nanos":%d,"secs":0},"stats":{"bytes_printed":9,'
               b'"bytes_searched":%d,"elapsed":{"human":"x","nanos":0,"secs":0},"matched_lines":1,'
               b'"matches":1,"searches":%d,"searches_with_match":1}},"type":"summary"}')

    def output(self, nanos=0, file_bytes=12, total_bytes=12, searches=1, status=0, match=MATCH):
        lines = [match, self.END % (nanos, file_bytes), self.SUMMARY % (nanos, total_bytes, searches)]
        return subprocess.CompletedProcess([], status, b"\n".join(lines) + b"\n", b"")

    def test_only_timings_differ(self):
        oracle = self.output(nanos=5)
        self.assertTrue(rg_dialect_contract(self.output(), oracle, "scan"))
        self.assertFalse(rg_dialect_contract(self.output(status=1), oracle, "scan"))
        self.assertFalse(rg_dialect_contract(self.output(file_bytes=13), oracle, "scan"))
        self.assertFalse(rg_dialect_contract(self.output(match=self.MATCH.replace(b':1,"abs', b':2,"abs')),
                                             oracle, "scan"))
        reordered = self.MATCH.replace(b'"line_number":1,"absolute_offset":0',
                                       b'"absolute_offset":0,"line_number":1')
        self.assertFalse(rg_dialect_contract(self.output(match=reordered), oracle, "scan"))

    def test_an_index_may_read_fewer_files_but_not_report_other_ones(self):
        oracle = self.output(total_bytes=99, searches=8)
        self.assertFalse(rg_dialect_contract(self.output(), oracle, "scan"))
        self.assertTrue(rg_dialect_contract(self.output(), oracle, "index"))
        self.assertFalse(rg_dialect_contract(self.output(file_bytes=13), self.output(), "index"))

    def test_text_layouts_compare_as_text(self):
        plain = subprocess.CompletedProcess([], 0, b"src/a.rs\n", b"")
        self.assertTrue(rg_dialect_contract(plain, plain, "scan"))
        self.assertFalse(rg_dialect_contract(plain, subprocess.CompletedProcess([], 0, b"src/b.rs\n", b""), "scan"))

    def test_query_drops_budget_and_bare_json(self):
        self.assertEqual(rg_dialect_query(["--budget", "0", "x"]), ["x"])
        self.assertEqual(rg_dialect_query(["--json", "x"]), ["x"])
        self.assertEqual(rg_dialect_query(["-c", "-w", "x"]), ["-c", "-w", "x"])


if __name__ == "__main__":
    unittest.main()
