import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import subprocess

import sessions


class SessionHarnessTests(unittest.TestCase):
    def test_fixtures_distinguish_expiry_and_byte_limit(self):
        live = [json.loads(x) for x in sessions.fixture("warm", 100000).splitlines()]
        expired = [json.loads(x) for x in sessions.fixture("expired", 100000).splitlines()]
        self.assertEqual(len(live), 100)
        self.assertEqual(len(expired), 2000)
        self.assertTrue(all(r["t"] < 100000 - 86400 for r in expired))
        self.assertGreater(len(sessions.fixture("oversized", 100000)), sessions.MAX_BYTES)

    def test_timeout_is_a_failed_contract_with_partial_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            env = {"GREEG_INDEX_DIR": str(root / "index")}
            with patch.object(sessions.subprocess, "run", side_effect=subprocess.TimeoutExpired("binary", 10, output=b"partial")):
                row = sessions.invoke(Path("fake"), "scan", "no_session", root, env, (0, b"", b""), None, 100000)
            self.assertFalse(row["contract"])
            self.assertFalse(row["search_equal"])
            self.assertEqual(row["stdout_bytes"], 7)
            self.assertEqual(row["error"], "TimeoutExpired")

    def test_non_object_storage_rows_fail_contract_without_aborting(self):
        for value in (None, 42, "text", [], {"t": None}):
            with self.subTest(value=value), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                env = {"GREEG_INDEX_DIR": str(root / "index")}
                def run(*args, **kwargs):
                    directory = root / "index/session"
                    directory.mkdir(parents=True, mode=0o700)
                    path = directory / "measured.jsonl"
                    path.write_text(json.dumps(value) + "\n")
                    path.chmod(0o600)
                    return subprocess.CompletedProcess([], 0, b"", b"")
                with patch.object(sessions.subprocess, "run", side_effect=run):
                    row = sessions.invoke(Path("fake"), "scan", "fresh", root, env, (0, b"", b""), None, 100000)
                self.assertFalse(row["contract"])
                self.assertTrue(row["search_equal"])

    def test_missing_compiler_does_not_abort_prebuilt_benchmark(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "result.json"
            with patch("sys.argv", ["sessions.py", "/fake/baseline", "/fake/candidate", "--output", str(output)]), \
                    patch.object(sessions.subprocess, "check_output", side_effect=FileNotFoundError("rustc")), \
                    patch.object(sessions.tempfile, "TemporaryDirectory", side_effect=RuntimeError("measurement reached")):
                with self.assertRaisesRegex(RuntimeError, "measurement reached"):
                    sessions.main()

    def test_storage_success_does_not_hide_search_output_drift(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            env = {"GREEG_INDEX_DIR": str(root / "index")}
            result = subprocess.CompletedProcess([], 0, b"changed", b"")
            with patch.object(sessions.subprocess, "run", return_value=result):
                row = sessions.invoke(Path("fake"), "scan", "no_session", root, env, (0, b"expected", b""), None, 100000)
            self.assertFalse(row["contract"])
            self.assertFalse(row["search_equal"])


if __name__ == "__main__":
    unittest.main()
