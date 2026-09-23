from pathlib import Path
import os
import subprocess
import unittest
from unittest.mock import patch

import hooks


class HookHarnessTests(unittest.TestCase):
    def test_compiler_metadata_is_optional(self):
        for error in [FileNotFoundError(), subprocess.CalledProcessError(1, "rustc"),
                      subprocess.TimeoutExpired("rustc", 5)]:
            with self.subTest(error=type(error).__name__), patch.object(
                    hooks.subprocess, "check_output", side_effect=error):
                self.assertIsNone(hooks.compiler_version())

    def test_environment_discards_inherited_greeg_settings(self):
        with patch.dict(os.environ, {"GREEG_DEBUG_START": "1", "GREEG_DEBUG_PANIC": "1",
                                   "GREEG_INDEX_DIR": "/unrelated", "GREEG_FUTURE_SETTING": "1"}):
            env = hooks.benchmark_environment(Path("/fixture"))
        self.assertEqual({k for k in env if k.startswith("GREEG_")},
                         {"GREEG_STATS", "GREEG_SESSION", "GREEG_INDEX_DIR"})
        self.assertEqual(env["GREEG_INDEX_DIR"], "/fixture/unused-index")
        self.assertEqual(env["GREEG_STATS"], "0")


if __name__ == "__main__":
    unittest.main()
