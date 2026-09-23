#!/usr/bin/env python3
"""Paired session persistence checks on a disposable corpus and isolated indexes."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shutil
import stat
import subprocess
import tempfile
import time

from hook_config import summary
from hooks import benchmark_environment, compiler_version

MAX_BYTES = 4 * 1024 * 1024
CASES = ("no_session", "fresh", "warm", "compaction", "expired", "oversized", "symlink")
CORPUS = {f"file{i}.rs": f"pub fn needle_{i}() {{}}\n" for i in range(12)}


def fixture(case, now):
    record = {"t": now, "q": "unrelated", "pat": "legacy pattern", "hits": 0, "files": [], "ctx": [], "shown": []}
    if case == "expired":
        record["t"] -= 86401
    if case == "oversized":
        record["q"] = "x" * MAX_BYTES
    count = 2000 if case in ("compaction", "expired") else 100 if case == "warm" else 1
    return ((json.dumps(record) + "\n") * count).encode()


def invoke(binary, backend, case, base, env, expected, encoding, now):
    index = Path(env["GREEG_INDEX_DIR"])
    session = index / "session"
    if session.exists():
        shutil.rmtree(session)
    target = base / "outside.jsonl"
    target.write_bytes(b"outside sentinel\n")
    path = session / "measured.jsonl"
    if case not in ("no_session", "fresh"):
        session.mkdir(mode=0o700, parents=True)
        if case == "symlink":
            path.symlink_to(target)
        else:
            path.write_bytes(fixture(case, now))
            path.chmod(0o600)
    args = [str(binary), "needle", ".", "--matching", "exact", "--budget", "400"]
    args += ["--no-index"] if backend == "scan" else ["--fresh", "none"]
    args += ["--no-session"] if case == "no_session" else ["--session", "measured"]
    start = time.perf_counter()
    try:
        run = subprocess.run(args, cwd=base / "corpus", env=env, stdin=subprocess.DEVNULL,
                             capture_output=True, timeout=10, umask=0)
    except (subprocess.TimeoutExpired, OSError) as exc:
        return {"ms": (time.perf_counter() - start) * 1000, "contract": False,
                "status": None, "error": type(exc).__name__, "search_equal": False,
                "stdout_bytes": len(getattr(exc, "stdout", None) or b""),
                "stderr_bytes": len(getattr(exc, "stderr", None) or b""), "tokens": None}
    elapsed = (time.perf_counter() - start) * 1000
    search_equal = (run.returncode, run.stdout, run.stderr) == expected
    if case == "no_session":
        stored = not session.exists()
    elif case == "symlink":
        stored = path.is_symlink() and target.read_bytes() == b"outside sentinel\n"
    else:
        try:
            records = [json.loads(line) for line in path.read_bytes().splitlines()]
            expected_count = {"fresh": 1, "warm": 101, "compaction": 1001, "expired": 1, "oversized": 1}[case]
            stored = (len(records) == expected_count and path.stat().st_size <= MAX_BYTES
                      and stat.S_IMODE(path.stat().st_mode) == 0o600
                      and stat.S_IMODE(session.stat().st_mode) == 0o700
                      and all(now - 86400 < r["t"] <= int(time.time()) for r in records))
        except (OSError, ValueError, KeyError, TypeError):
            stored = False
    return {"ms": elapsed, "contract": search_equal and stored, "search_equal": search_equal,
            "status": run.returncode, "stdout_bytes": len(run.stdout), "stderr_bytes": len(run.stderr),
            "tokens": len(encoding.encode((run.stdout + run.stderr).decode())) if encoding else None}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("baseline", type=Path)
    p.add_argument("candidate", type=Path)
    p.add_argument("--runs", type=int, default=51)
    p.add_argument("--tokens", action="store_true")
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    if a.runs < 5:
        p.error("--runs must be at least 5")
    encoding = None
    if a.tokens:
        import tiktoken
        encoding = tiktoken.get_encoding("o200k_base")
    binaries = {k: v.resolve() for k, v in (("baseline", a.baseline), ("candidate", a.candidate))}
    report = {"protocol": 2, "platform": platform.platform(), "machine": platform.machine(),
              "cpu_count": os.cpu_count(), "python": platform.python_version(),
              "measured_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
              "runs": a.runs, "warmups": 3, "seed": 20260923,
              "tokenizer": "o200k_base" if encoding else None,
              "scope": "Warm process wall time for search plus session load/write; fixture reset and index build excluded. No cold-cache, RSS, or agent-task savings claim.",
              "environment_policy": "Disposable corpus; isolated HOME/config/cache/index; stats disabled; sessions enabled only in named cases; umask 000.",
              "binaries": {}, "results": [], "corpus": CORPUS,
              "corpus_sha256": hashlib.sha256(json.dumps(CORPUS, sort_keys=True).encode()).hexdigest(),
              "harness_sha256": {name: hashlib.sha256(Path(__file__).with_name(name).read_bytes()).hexdigest() for name in ("sessions.py", "hook_config.py", "hooks.py")},
              "compiler": compiler_version()}
    rng = random.Random(report["seed"])
    with tempfile.TemporaryDirectory(prefix="greeg-session-bench-") as temp:
        base = Path(temp)
        (base / "corpus/.git").mkdir(parents=True)
        for name, contents in CORPUS.items():
            (base / "corpus" / name).write_text(contents)
        envs = {}
        for label, binary in binaries.items():
            env = benchmark_environment(base)
            env["GREEG_INDEX_DIR"] = str(base / f"index-{label}")
            envs[label] = env
            version = subprocess.run([str(binary), "--version"], env=env, capture_output=True, text=True, check=True, timeout=10).stdout.strip()
            report["binaries"][label] = {"version": version, "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
            subprocess.run([str(binary), "index", "--root", str(base / "corpus")], env=env, capture_output=True, check=True, timeout=30)
        expected = {}
        file_sets = []
        for backend in ("scan", "index"):
            mode = ["--no-index"] if backend == "scan" else ["--fresh", "none"]
            oracle = subprocess.run([str(binaries["baseline"]), "needle", ".", "--matching", "exact", "--budget", "400", "--no-session", *mode], cwd=base / "corpus", env=envs["baseline"], capture_output=True, check=True, timeout=10, stdin=subprocess.DEVNULL)
            expected[backend] = (oracle.returncode, oracle.stdout, oracle.stderr)
            for label, binary in binaries.items():
                run = subprocess.run([str(binary), "needle", ".", "--matching", "exact", "-l", "--no-session", *mode], cwd=base / "corpus", env=envs[label], capture_output=True, check=True, timeout=10, stdin=subprocess.DEVNULL)
                file_sets.append(sorted(run.stdout.splitlines()))
        report["scan_index_files_equal"] = all(files == file_sets[0] for files in file_sets)
        if not report["scan_index_files_equal"]:
            raise ValueError("scan/index file-set preflight failed")
        report["output_sha256"] = {backend: hashlib.sha256(value[1] + b"\0" + value[2]).hexdigest() for backend, value in expected.items()}
        now = int(time.time())
        report["fixture_time"] = now
        report["fixture_sha256"] = {case: hashlib.sha256(fixture(case, now)).hexdigest() for case in CASES}
        for backend in ("scan", "index"):
            for case in CASES:
                samples = {label: [] for label in binaries}
                for i in range(report["warmups"] + a.runs):
                    labels = list(binaries)
                    rng.shuffle(labels)
                    for label in labels:
                        result = invoke(binaries[label], backend, case, base, envs[label], expected[backend], encoding, now)
                        if i >= report["warmups"]:
                            samples[label].append(result)
                row = {"backend": backend, "case": case}
                for label, values in samples.items():
                    row[label] = summary(values)
                    row[label]["search_equal"] = all(v["search_equal"] for v in values)
                    tokens = {v["tokens"] for v in values}
                    row[label]["tokens_both_streams"] = tokens.pop() if len(tokens) == 1 else None
                report["results"].append(row)
    report["completed_at"] = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(report, indent=2) + "\n")
    for label in binaries:
        print(f"{label}: {sum(r[label]['contract'] for r in report['results'])}/{len(report['results'])} session/search contracts")
    print(f"wrote {a.output}")
    return 0 if all(r["candidate"]["contract"] for r in report["results"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
