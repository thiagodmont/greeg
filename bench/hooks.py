#!/usr/bin/env python3
"""Measure hook replies and exact file/count searches on disposable fixtures.

python3 bench/hooks.py BASELINE CANDIDATE --runs 31 --tokens --output result.json
Requires rg; rustc is optional host metadata. Token counts describe hook protocol
replies, not agent-task savings.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import shlex
import shutil
import statistics
import subprocess
import tempfile
import time


CASES = {
    "ranked": ("rg needle", True),
    "files": ("rg -l needle", True),
    "count_miss": ("rg -c NEEDLE", True),
    "quoted": ("rg -F 'needle()'", True),
    "trailing_newline": ("rg needle\n", True),
    "size_limit": ("rg --max-filesize 4194304 -l large_needle", True),
    "recursive_grep": ("grep -rl needle .", False),
    "grep_file": ("grep needle file.rs", False),
    "json": ("rg --json needle", False),
    "executable_path": ("./rg needle", False),
    "comment": ("rg needle # comment", False),
    "pipeline": ("rg -l needle | sort", False),
    "compound": ("rg needle; echo done", False),
    "config": ("rg needle", False),
}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def compiler_version():
    try:
        return subprocess.check_output(["rustc", "--version"], text=True,
                                       stderr=subprocess.DEVNULL, timeout=5).strip()
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return None


def benchmark_environment(base):
    env = {k: v for k, v in os.environ.items() if not k.startswith("GREEG_")}
    env.update(HOME=str(base / "home"), XDG_CONFIG_HOME=str(base / "config"),
               XDG_CACHE_HOME=str(base / "cache"), GREEG_STATS="0", GREEG_SESSION="",
               GREEG_INDEX_DIR=str(base / "unused-index"), CLAUDE_CODE_SESSION_ID="",
               RIPGREP_CONFIG_PATH="")
    return env


def invoke(argv, root, env, payload=None):
    start = time.perf_counter_ns()
    out = subprocess.run(argv, cwd=root, env=env, input=payload or b"",
                         capture_output=True, timeout=30, check=False)
    return out, (time.perf_counter_ns() - start) / 1e6


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--runs", type=int, default=31)
    parser.add_argument("--tokens", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("--runs must be at least 5")
    rg = shutil.which("rg")
    if not rg:
        parser.error("rg is required")
    encoding = None
    if args.tokens:
        import tiktoken
        encoding = tiktoken.get_encoding("o200k_base")
    binaries = {k: str(v.resolve()) for k, v in (("baseline", args.baseline), ("candidate", args.candidate))}
    report = {"protocol": 2, "platform": platform.platform(), "machine": platform.machine(),
              "cpu_count": os.cpu_count(), "python": platform.python_version(), "runs": args.runs,
              "warmups": 3, "order_seed": 20260922, "tokenizer": "o200k_base" if encoding else None,
              "scope": "Hook process wall time and reply bytes/tokens; no live host, RSS, cold-cache or agent-task claim",
              "binaries": {}, "results": [], "search_contracts": []}
    report["harness_sha256"] = digest(Path(__file__).read_bytes())
    report["compiler"] = compiler_version()
    report["environment_policy"] = "Remove all inherited GREEG_* settings; isolate home/config/cache/index; disable sessions/stats and ripgrep config"
    randomizer = random.Random(report["order_seed"])
    with tempfile.TemporaryDirectory(prefix="greeg-hooks-") as tmp:
        base = Path(tmp)
        root = base / "tree"
        (root / ".git").mkdir(parents=True)
        corpus = {"src/file.rs": b"pub fn needle() {}\n", "src/other.rs": b"// needle\nneedle();\n",
                  "src/.hidden.rs": b"needle\n", "src/ignored.rs": b"needle\n", ".gitignore": b"ignored.rs\n",
                  "src/large.txt": b"x" * ((4 << 20) + 1) + b"\nlarge_needle\n"}
        for name, body in corpus.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_bytes(body)
        report["corpus"] = {"files": len(corpus), "sha256": digest(b"".join(
            name.encode() + b"\0" + body for name, body in sorted(corpus.items())))}
        env = benchmark_environment(base)
        Path(env["HOME"]).mkdir()
        report["ripgrep"] = invoke([rg, "--version"], root, env)[0].stdout.decode().splitlines()[0]
        for label, binary in binaries.items():
            report["binaries"][label] = {"sha256": digest(Path(binary).read_bytes()),
                "version": invoke([binary, "--version"], root, env)[0].stdout.decode().strip()}
        for agent in ("claude", "codex"):
            for case, (command, accepted) in CASES.items():
                payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}}).encode()
                case_env = dict(env, RIPGREP_CONFIG_PATH=str(base / "config-unknown")) if case == "config" else env
                samples = {label: [] for label in binaries}
                outputs = {}
                for i in range(args.runs + report["warmups"]):
                    order = list(binaries)
                    randomizer.shuffle(order)
                    for label in order:
                        out, ms = invoke([binaries[label], "hook", "run", "--agent", agent], root, case_env, payload)
                        if out.returncode != 0 or out.stderr:
                            raise RuntimeError(f"{label}/{case}: {out}")
                        signature = (out.stdout, out.stderr, out.returncode)
                        if label in outputs and signature != outputs[label]:
                            raise RuntimeError(f"nondeterministic reply: {label}/{case}")
                        outputs[label] = signature
                        if i >= report["warmups"]:
                            samples[label].append(ms)
                row = {"agent": agent, "case": case, "command": command, "expected_rewrite": accepted}
                for label in binaries:
                    stdout = outputs[label][0]
                    reply = json.loads(stdout) if stdout else None
                    rewritten = reply["hookSpecificOutput"]["updatedInput"]["command"] if reply else None
                    argv = shlex.split(rewritten) if rewritten else []
                    exact = argv[1:3] == ["--matching", "exact"]
                    row[label] = {"samples_ms": samples[label], "median_ms": statistics.median(samples[label]),
                        "p95_ms": sorted(samples[label])[math.ceil(len(samples[label]) * .95) - 1],
                        "reply_bytes": len(stdout), "reply_tokens": len(encoding.encode(stdout.decode())) if encoding else None,
                        "rewritten": rewritten, "contract": bool(reply) == accepted and (not accepted or exact)}
                row["median_change_percent"] = (row["candidate"]["median_ms"] / row["baseline"]["median_ms"] - 1) * 100
                report["results"].append(row)

        # The oracle covers machine output, not the deliberately different ranked text dialect.
        for label, binary in binaries.items():
            search_env = dict(env, GREEG_INDEX_DIR=str(base / (label + "-index")))
            built, _ = invoke([binary, "index", "--quiet"], root, search_env)
            if built.returncode:
                raise RuntimeError(f"index build failed: {built}")
            for backend in ("scan", "index"):
                for query, expected_status in (
                    ("rg -w -l needle", 0), ("rg -w -c needle", 0), ("rg -l NEEDLE", 1),
                    ("rg -w -c needl", 1), ("rg -w -i -l NEEDLE", 0), ("rg -F -l 'needle()'", 0),
                    ("rg -l large_needle", 0), ("rg --max-filesize 4194304 -l large_needle", 1),
                    ("rg --max-filesize=5242880 -l large_needle", 0), ("rg --max-filesize 0 -l needle", 1),
                ):
                    command = query + " src"
                    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}}).encode()
                    hook, _ = invoke([binary, "hook", "run"], root, search_env, payload)
                    rewritten = json.loads(hook.stdout)["hookSpecificOutput"]["updatedInput"]["command"]
                    flags = ["--no-index"] if backend == "scan" else ["--fresh", "stat"]
                    actual, _ = invoke([binary, "--no-session", *flags, *shlex.split(rewritten)[1:]], root, search_env)
                    oracle, _ = invoke([rg, *shlex.split(command)[1:]], root, search_env)
                    if oracle.returncode != expected_status or bool(oracle.stdout) != (expected_status == 0):
                        raise RuntimeError(f"oracle did not exercise the expected hit/miss: {command}: {oracle}")
                    report["search_contracts"].append({"binary": label, "backend": backend, "command": command,
                        "stdout_and_status_equal": actual.returncode == oracle.returncode and
                            sorted(actual.stdout.splitlines()) == sorted(oracle.stdout.splitlines()),
                        "stdout_bytes": len(actual.stdout), "stderr_bytes": len(actual.stderr),
                        "exit": actual.returncode, "stdout_sha256": digest(actual.stdout)})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    failures = [r for r in report["results"] if not r["candidate"]["contract"]]
    failures += [r for r in report["search_contracts"] if r["binary"] == "candidate" and not r["stdout_and_status_equal"]]
    print(f"wrote {args.output}: {len(report['results'])} hook cases, {len(report['search_contracts'])} search checks; {len(failures)} candidate failures")
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
