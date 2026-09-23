import os
import signal
import errno
import shutil
import stat
from unittest.mock import patch

from corpus import Corpus
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


BENCH = Path(__file__).resolve().parent


class CorpusSafetyTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("rg"), "ripgrep required")
    def test_zero_minute_soak_preserves_dirty_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "source"
            source.mkdir()
            env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
            def git(*args):
                subprocess.run(["git", *args], cwd=source, env=env, check=True,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            git("init", "-q")
            tracked = source / "main.py"
            tracked.write_text("def original(): pass\n")
            git("add", "main.py")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "commit", "-qm", "fixture")
            tracked.write_text("def dirty(): pass\n")
            note = source / "notes.txt"
            note.write_bytes(b"untracked work\x00")
            fake = root / "greeg"
            fake.write_text(f"#!{sys.executable}\n")
            fake.chmod(0o700)
            result = subprocess.run([sys.executable, str(BENCH / "soak.py"), "0",
                                     str(fake), str(source)], capture_output=True, env=env)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(tracked.read_text(), "def dirty(): pass\n")
            self.assertEqual(note.read_bytes(), b"untracked work\x00")

    def test_snapshot_restore_preserves_bytes_modes_and_source_edits(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source"
            source.mkdir()
            (source / "main.py").write_bytes(b"dirty source\x00")
            (source / "main.py").chmod(0o751)
            (source / ".gitignore").write_text("ignored/\n")
            (source / "ignored").mkdir()
            (source / "ignored/note").write_bytes(b"private work")
            (source / ".git").write_text("gitdir: /not/owned\n")
            with Corpus(source) as corpus:
                base = corpus.base
                copied = corpus.path("main.py")
                self.assertNotEqual(copied.stat().st_ino, (source / "main.py").stat().st_ino)
                self.assertEqual(stat.S_IMODE(copied.stat().st_mode), 0o751)
                self.assertTrue((corpus.root / ".git").is_dir())
                self.assertEqual(list((corpus.root / ".git").iterdir()), [])
                copied.write_bytes(b"benchmark edit")
                corpus.path("new.py").write_text("temporary")
                corpus.path("ignored/note").unlink()
                (source / "main.py").write_bytes(b"concurrent caller edit")
                corpus.restore()
                self.assertEqual(corpus.path("main.py").read_bytes(), b"dirty source\x00")
                self.assertEqual(corpus.path("ignored/note").read_bytes(), b"private work")
                self.assertFalse(corpus.path("new.py").exists())
            self.assertFalse(base.exists())
            self.assertEqual((source / "main.py").read_bytes(), b"concurrent caller edit")

    def test_symlinks_and_escaping_paths_cannot_be_mutated(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source"
            source.mkdir()
            outside = Path(tmp) / "outside"
            outside.mkdir()
            (outside / "sentinel").write_bytes(b"keep")
            (source / "link").symlink_to(outside, target_is_directory=True)
            (source / "file.py").symlink_to(outside / "sentinel")
            (source / "ok.py").write_text("pass")
            with Corpus(source) as corpus:
                self.assertTrue((corpus.root / "link").is_symlink())
                for name in ["link/sentinel", "link/new", "file.py", "../outside", str(outside), ".git/config"]:
                    with self.subTest(name=name), self.assertRaises(ValueError):
                        corpus.path(name)
                self.assertEqual(corpus.regular_files(["file.py", "link/sentinel", "ok.py"]), ["ok.py"])
                corpus.restore()
            self.assertEqual((outside / "sentinel").read_bytes(), b"keep")

    def test_environment_and_cache_are_private_per_corpus(self):
        with tempfile.TemporaryDirectory() as source:
            with patch.dict(os.environ, {"GREEG_DEBUG_PANIC": "1", "GREEG_STATS": "1",
                                         "GREEG_INDEX_DIR": source, "RIPGREP_CONFIG_PATH": "untrusted"}):
                with Corpus(source) as first, Corpus(source) as second:
                    self.assertNotIn("GREEG_DEBUG_PANIC", first.env)
                    self.assertEqual(first.env["GREEG_STATS"], "0")
                    self.assertEqual(first.env["RIPGREP_CONFIG_PATH"], "")
                    self.assertNotEqual(first.env["GREEG_INDEX_DIR"], second.env["GREEG_INDEX_DIR"])
                    for key in ["GREEG_INDEX_DIR", "HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"]:
                        self.assertTrue(Path(first.env[key]).is_relative_to(first.base))

    def test_timeout_and_exception_cleanup(self):
        with tempfile.TemporaryDirectory() as source:
            with self.assertRaises(subprocess.TimeoutExpired):
                with Corpus(source) as corpus:
                    base = corpus.base
                    corpus.run([sys.executable, "-c", "import time; time.sleep(10)"], timeout=0.05)
            self.assertFalse(base.exists())
            self.assertEqual(list(Path(source).iterdir()), [])

    def test_cleanup_retries_a_finishing_background_write(self):
        with tempfile.TemporaryDirectory() as source:
            corpus = Corpus(source)
            cleanup = corpus.temporary.cleanup
            with patch.object(corpus.temporary, "cleanup", side_effect=[OSError(errno.ENOTEMPTY, "busy"), None]) as retry:
                corpus.__exit__(None, None, None)
                self.assertEqual(retry.call_count, 2)
            cleanup()
            self.assertFalse(corpus.base.exists())

    def test_temp_directory_inside_source_is_rejected_before_writes(self):
        with tempfile.TemporaryDirectory() as source:
            with patch("corpus.tempfile.gettempdir", return_value=source):
                with self.assertRaisesRegex(ValueError, "TMPDIR"):
                    Corpus(source)
            self.assertEqual(list(Path(source).iterdir()), [])

    @unittest.skipUnless(shutil.which("rg"), "ripgrep required")
    def test_failed_and_interrupted_harnesses_preserve_source(self):
        for script in ["soak.py", "edits.py"]:
            for interrupt in [False, True]:
                with self.subTest(script=script, interrupt=interrupt), tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    source = root / "source"
                    source.mkdir()
                    (source / "main.py").write_bytes(b"caller data")
                    fake = root / "fake"
                    fake.write_text(f"#!{sys.executable}\nimport sys, time\n" +
                                    ("time.sleep(30)\n" if interrupt else "sys.exit(2)\n"))
                    fake.chmod(0o700)
                    args = ["1", "./fake", str(source)] if script == "soak.py" else [str(source), "./fake"]
                    with subprocess.Popen([sys.executable, str(BENCH / script), *args], cwd=root,
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as process:
                        line = process.stdout.readline()
                        if line.startswith("Random seed:"):
                            line = process.stdout.readline()
                        self.assertTrue(line.startswith("Disposable corpus:"), line)
                        work = Path(line.split(": ", 1)[1].split(" (source:", 1)[0].strip())
                        if interrupt:
                            process.send_signal(signal.SIGTERM)
                        stdout, stderr = process.communicate(timeout=10)
                        self.assertNotEqual(process.returncode, 0, stdout + stderr)
                    self.assertFalse(work.parent.exists())
                    self.assertEqual((source / "main.py").read_bytes(), b"caller data")
                    self.assertEqual(list(source.iterdir()), [source / "main.py"])

    @unittest.skipUnless((BENCH.parent / "target/release/greeg").is_file() and shutil.which("rg"),
                         "release binary and ripgrep required")
    def test_edit_harness_handles_empty_and_tiny_corpora(self):
        for count in [0, 1, 3]:
            with self.subTest(count=count), tempfile.TemporaryDirectory() as tmp:
                source = Path(tmp) / "source"
                source.mkdir()
                for i in range(count):
                    (source / f"file{i}.py").write_text(f"def example{i}(): return 1\n")
                before = {p.name: p.read_bytes() for p in source.iterdir()}
                result = subprocess.run([sys.executable, str(BENCH / "edits.py"), str(source),
                                         str(BENCH.parent / "target/release/greeg")], capture_output=True, timeout=60)
                self.assertEqual(result.returncode, 0, result.stdout.decode() + result.stderr.decode())
                self.assertEqual({p.name: p.read_bytes() for p in source.iterdir()}, before)


if __name__ == "__main__":
    unittest.main()
