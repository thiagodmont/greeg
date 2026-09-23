from pathlib import Path
import json
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import hook_config


class HookConfigHarnessTests(unittest.TestCase):
    def test_mixed_toml_requires_comments_and_trust_data(self):
        case = next(c for c in hook_config.fixtures("codex") if c.name == "mixed_uninstall")
        removed = b'[[hooks.PreToolUse.hooks]]\ntype = "command"\ncommand = "greeg hook run --agent codex"\n'
        correct = case.original.replace(removed, b"")
        variants = [(correct, True),
                    (correct.replace(b"# retained handler", b""), False),
                    (correct.replace(b"# fixture", b""), False),
                    (correct.replace(b'sha256:keep', b'sha256:changed'), False)]
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            for output, expected in variants:
                def run(*args, **kwargs):
                    (base / "home/.codex/config.toml").write_bytes(output)
                    return subprocess.CompletedProcess(args, 0, b"", b"")
                with self.subTest(output=output), patch.object(hook_config.subprocess, "run", side_effect=run):
                    sample = hook_config.invoke(Path("/fixture"), "codex", case, base, {})
                    self.assertEqual(sample["contract"], expected)

    def test_installed_noop_requires_identical_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            for agent in ("claude", "codex"):
                case = next(c for c in hook_config.fixtures(agent) if c.name == "installed_noop")
                for suffix in (b"", b"\n"):
                    def run(*args, **kwargs):
                        path = base / "home" / (".claude/settings.json" if agent == "claude" else ".codex/config.toml")
                        path.write_bytes(case.original + suffix)
                        return subprocess.CompletedProcess(args, 0, b"", b"")
                    with self.subTest(agent=agent, suffix=suffix), patch.object(hook_config.subprocess, "run", side_effect=run):
                        sample = hook_config.invoke(Path("/fixture"), agent, case, base, {})
                        self.assertEqual(sample["contract"], not suffix)

    def test_report_survives_invocation_failures(self):
        def run(args, **kwargs):
            if args[-1] == "--version":
                return subprocess.CompletedProcess(args, 0, "fixture version", "")
            raise subprocess.TimeoutExpired(args, 10, output=b"partial")
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "result.json"
            binary = str(Path(__file__).resolve())
            with patch("sys.argv", ["hook_config.py", binary, binary, "--runs", "1", "--output", str(output)]), \
                    patch.object(hook_config.platform, "platform", return_value="test platform"), \
                    patch.object(hook_config.subprocess, "run", side_effect=run):
                self.assertEqual(hook_config.main(), 1)
            report = json.loads(output.read_text())
            self.assertEqual(len(report["results"]), 20)
            for row in report["results"]:
                self.assertIsNone(row["median_change_percent"])
                for label in ("baseline", "candidate"):
                    self.assertFalse(row[label]["contract"])
                    self.assertEqual(row[label]["failed_invocations"], 1)
                    self.assertEqual(row[label]["errors"], ["TimeoutExpired"])

    def test_failed_invocations_are_recorded_with_elapsed_time(self):
        errors = [subprocess.TimeoutExpired("fixture", 10, output=b"partial", stderr=b"error"),
                  OSError("cannot launch")]
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            for error in errors:
                with self.subTest(error=type(error).__name__), patch.object(
                        hook_config.subprocess, "run", side_effect=error), patch.object(
                        hook_config.time, "perf_counter", side_effect=[1.0, 11.0]):
                    case = next(c for c in hook_config.fixtures("claude") if c[0] == "absent_uninstall")
                    sample = hook_config.invoke(Path("/fixture"), "claude", case, base, {})
                    self.assertFalse(sample["contract"])
                    self.assertIsNone(sample["status"])
                    self.assertEqual(sample["ms"], 10000)
                    self.assertEqual(sample["error"], type(error).__name__)
                    self.assertEqual(sample["stdout_bytes"], 7 if isinstance(error, subprocess.TimeoutExpired) else 0)
                    result = hook_config.summary([sample, {"ms": 1, "contract": True, "status": 0,
                                                           "stdout_bytes": 0, "stderr_bytes": 0}])
                    self.assertFalse(result["contract"])
                    self.assertEqual(result["errors"], [type(error).__name__])


if __name__ == "__main__":
    unittest.main()
