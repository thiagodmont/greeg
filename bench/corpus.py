"""Owned snapshots for benchmarks that modify source files."""
from contextlib import contextmanager
import errno
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time


def executable(value):
    path = shutil.which(value)
    if path is None:
        raise ValueError(f"executable not found: {value}")
    return str(Path(path).resolve())


@contextmanager
def interrupted_cleanup():
    def interrupt(signum, frame):
        raise KeyboardInterrupt
    previous = signal.signal(signal.SIGTERM, interrupt)
    try:
        yield
    finally:
        signal.signal(signal.SIGTERM, previous)


class Corpus:
    @staticmethod
    def validate_source(source):
        source = Path(source).resolve(strict=True)
        if not source.is_dir():
            raise ValueError(f"not a corpus directory: {source}")
        if Path(tempfile.gettempdir()).resolve().is_relative_to(source):
            raise ValueError("temporary directory is inside the corpus; set TMPDIR outside all sources")
        return source

    def __init__(self, source):
        self.source = self.validate_source(source)
        self.temporary = tempfile.TemporaryDirectory(prefix="greeg-bench-")
        self.base = Path(self.temporary.name)
        self.snapshot = self.base / "snapshot"
        self.root = self.base / "work"
        try:
            # Never copy Git metadata that could point back at a caller's worktree.
            shutil.copytree(self.source, self.snapshot, symlinks=True,
                            ignore=shutil.ignore_patterns(".git"))
            if (self.source / ".git").exists():
                (self.snapshot / ".git").mkdir()
            self.restore()
        except BaseException:
            self.temporary.cleanup()
            raise
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("GREEG_")}
        self.env.update(HOME=str(self.base / "home"),
                        XDG_CONFIG_HOME=str(self.base / "config"),
                        XDG_CACHE_HOME=str(self.base / "cache"),
                        GREEG_INDEX_DIR=str(self.base / "index"),
                        GREEG_STATS="0", GREEG_SESSION="",
                        CLAUDE_CODE_SESSION_ID="", RIPGREP_CONFIG_PATH="")

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        # A detached index writer may finish while its owned cache is removed.
        for attempt in range(20):
            try:
                self.temporary.cleanup()
                return
            except OSError as error:
                if error.errno not in (errno.ENOTEMPTY, errno.ENOENT) or attempt == 19:
                    raise
                time.sleep(0.05)

    def restore(self):
        if self.root.exists():
            shutil.rmtree(self.root)
        shutil.copytree(self.snapshot, self.root, symlinks=True)

    def path(self, relative):
        relative = Path(relative)
        if relative.is_absolute() or ".." in relative.parts or ".git" in relative.parts:
            raise ValueError(f"unsafe mutation path: {relative}")
        path = self.root
        for part in relative.parts:
            path = path / part
            if path.is_symlink():
                raise ValueError(f"refusing to mutate symlink: {relative}")
        return path

    def regular_files(self, names):
        result = []
        for name in names:
            try:
                if self.path(name).is_file():
                    result.append(name)
            except ValueError:
                continue
        return result

    def run(self, args, **kwargs):
        return subprocess.run(args, cwd=self.root, capture_output=True,
                              stdin=subprocess.DEVNULL, env=self.env,
                              timeout=kwargs.pop("timeout", 120), **kwargs)
