import json
import unittest

from json_records import matches, path_of

NOT_UTF8 = "/2E="  # base64 of b"\xffa"


def lines(*records):
    return "\n".join(json.dumps(r) for r in records)


class JsonRecordsTests(unittest.TestCase):
    def test_paths_that_are_not_utf8_survive_and_compare_across_dialects(self):
        rg = lines({"type": "begin", "data": {"path": {"bytes": NOT_UTF8}}},
                   {"type": "match", "data": {"path": {"bytes": NOT_UTF8}, "line_number": 3}})
        greeg = lines({"type": "greeg", "data": {"schema": 2}},
                      {"type": "begin", "data": {"path": {"bytes": NOT_UTF8}}},
                      {"type": "match", "data": {"line": 3}},
                      {"type": "footer", "data": {}})
        self.assertEqual([(p, l) for p, l, _ in matches(rg)], [(p, l) for p, l, _ in matches(greeg)])
        self.assertEqual(path_of({"bytes": NOT_UTF8}).encode("utf-8", "surrogateescape"), b"\xffa")

    def test_match_records_take_their_files_path(self):
        greeg = lines({"type": "begin", "data": {"path": {"text": "./a.rs"}}},
                      {"type": "match", "data": {"line": 1}},
                      {"type": "context", "data": {"line": 2}},
                      {"type": "begin", "data": {"path": {"text": "b.rs"}}},
                      {"type": "match", "data": {"line": 5}})
        self.assertEqual([(p, l) for p, l, _ in matches(greeg)], [("a.rs", 1), ("b.rs", 5)])


if __name__ == "__main__":
    unittest.main()
