#!/usr/bin/env python3
"""Compare warm scan/index latency, output size, and parity on disposable fixtures.

Usage: python3 bench/matching.py BASELINE CANDIDATE --output result.json
Requires installed rg; --tokens counts o200k_base tokens with installed tiktoken.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shutil
import statistics
import subprocess
import tempfile
import time


CASES = {
    "case_miss_files": ["-l", "LOAD_CONFIG"],
    "word_miss_count": ["-c", "-w", "load_conf"],
    "split_miss_unlimited": ["--budget", "0", "LoadConfig"],
    "fuzzy_miss_unlimited": ["--budget", "0", "load_confiq"],
    "absent_files": ["-l", "nonexistent_zzzz_symbol"],
    "hit_files": ["-l", "load_config"],
    "hit_count": ["-c", "-i", "LOAD_CONFIG"],
    "hit_unlimited": ["--budget", "0", "load_config"],
    "case_miss_json": ["--json", "LOAD_CONFIG"],
    "hit_json": ["--json", "load_config"],
    "def_hit": ["def", "load_config", "--matching", "exact"],
    "def_case_hit": ["def", "LOAD_CONFIG", "-i", "--matching", "exact"],
    "def_case_miss": ["def", "load_confiq", "-i", "--matching", "exact"],
    "ranked_hit": ["load_config"],
    "ranked_discovery": ["LOAD_CONFIG"],
}


def run(argv, root, env):
    start = time.perf_counter_ns()
    result = subprocess.run(argv, cwd=root, env=env, stdin=subprocess.DEVNULL,
                            capture_output=True, timeout=30, check=False)
    ms = (time.perf_counter_ns() - start) / 1e6
    if result.returncode not in (0, 1):
        raise RuntimeError(f"{argv}: {result.returncode}: {result.stderr!r}")
    return result, ms


def digest(data):
    return hashlib.sha256(data).hexdigest()


def stable_stdout(stdout, is_json):
    if not is_json:
        return stdout
    records = [json.loads(line) for line in stdout.splitlines()]
    for record in records:
        data = record.get("data", {})
        if record["type"] == "summary":
            data.pop("elapsed_total", None)
            data.get("stats", {}).pop("elapsed", None)
        elif record["type"] == "footer":
            data.pop("elapsed_ms", None)
    return json.dumps(records, sort_keys=True, separators=(",", ":")).encode()


def json_exact_contract(output, oracle):
    records = [json.loads(line) for line in output.stdout.splitlines()]
    expected = [json.loads(line) for line in oracle.stdout.splitlines()]
    def matches(rows):
        keys = ("path", "lines", "line_number", "absolute_offset", "submatches")
        return sorted(json.dumps({k: r["data"][k] for k in keys}, sort_keys=True)
                      for r in rows if r["type"] == "match")
    footers = [r["data"] for r in records if r["type"] == "footer"]
    summaries = [r["data"]["stats"] for r in expected if r["type"] == "summary"]
    return (output.returncode == oracle.returncode and matches(records) == matches(expected)
            and len(footers) == len(summaries) == 1 and footers[0]["rung"] == "exact"
            and footers[0]["hits_total"] == summaries[0]["matched_lines"])


def definition_contract(output, oracle):
    paths = re.findall(rb"^src/unit_[0-9]+\.rs(?=\s|$)", output.stdout, re.MULTILINE)
    return (output.returncode == oracle.returncode and sorted(paths) == sorted(oracle.stdout.splitlines())
            and b"matched case-insensitive" not in output.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--runs", type=int, default=31)
    parser.add_argument("--cases", nargs="+", choices=list(CASES), default=list(CASES))
    parser.add_argument("--tokens", action="store_true", help="use tiktoken o200k_base (pre-cache its public dictionary)")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("--runs must be at least 5")
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    rg = shutil.which("rg")
    if not rg:
        parser.error("rg is required for match-set/status checks")
    encoding = None
    if args.tokens:
        import tiktoken
        encoding = tiktoken.get_encoding("o200k_base")
    metadata = {
        "protocol": 2, "platform": platform.platform(), "machine": platform.machine(),
        "processor": platform.processor(), "cpu_count": os.cpu_count(), "python": platform.python_version(),
        "runs": args.runs, "cases": args.cases, "warmups": 3, "order_seed": 20260922,
        "freshness": "stat", "cache_state": "warm; full index built before queries",
        "tokenizer": "o200k_base" if encoding else None,
        "output_measurement": "first raw sample; JSON comparison removes only elapsed fields",
        "scope": "synthetic local corpus; process wall time; no cold-cache/RSS/agent-task claim",
        "binaries": {}, "results": [],
    }
    with tempfile.TemporaryDirectory(prefix="greeg-matching-") as temporary:
        base = Path(temporary)
        root = base / "tree"
        root.mkdir()
        (root / ".git").mkdir()
        corpus = hashlib.sha256()
        corpus_bytes = 0
        for i in range(256):
            name = f"src/unit_{i:03}.rs"
            body = "".join(f"pub fn task_{i}_{j}(value: u32) -> u32 {{ value + {j} }}\n"
                           for j in range(96))
            if i % 32 == 0:
                body += "pub fn load_config() -> u32 { 7 }\nfn caller() { load_config(); }\n"
            data = body.encode()
            path = root / name
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(data)
            corpus.update(name.encode() + b"\0" + data)
            corpus_bytes += len(data)
        metadata["corpus"] = {"files": 256, "bytes": corpus_bytes,
                              "sha256": corpus.hexdigest(), "generator": "bench/matching.py protocol 1"}
        environments = {}
        common = ["--no-session", "--fresh", "stat", "-j", "1"]
        for label, binary in binaries.items():
            private = base / label
            private.mkdir()
            env = os.environ.copy()
            env.update(HOME=str(private), XDG_CONFIG_HOME=str(private / "config"),
                       XDG_CACHE_HOME=str(private / "cache"), GREEG_STATS="0",
                       GREEG_INDEX_DIR=str(private / "index"))
            env.pop("RIPGREP_CONFIG_PATH", None)
            environments[label] = env
            version, _ = run([str(binary), "--version"], root, env)
            built, build_ms = run([str(binary), "index", "--quiet", "-j", "1"], root, env)
            if built.returncode != 0:
                raise RuntimeError("index build failed")
            metadata["binaries"][label] = {"path": str(binary), "sha256": digest(binary.read_bytes()),
                                            "bytes": binary.stat().st_size,
                                            "version": version.stdout.decode().strip(),
                                            "index_build_ms_single_sample": build_ms}
            probe, _ = run([str(binary), "load_config", ".", *common, "--stats"], root, env)
            if not any(line.startswith("greeg: index · walked ") for line in probe.stderr.decode().splitlines()):
                raise RuntimeError(f"{label} did not exercise the index: {probe.stderr!r}")
        rg_version, _ = run([rg, "--version"], root, environments["candidate"])
        metadata["ripgrep"] = rg_version.stdout.decode().splitlines()[0]
        rng = random.Random(metadata["order_seed"])
        for backend in ("scan", "index"):
            for name in args.cases:
                query = CASES[name]
                is_json = "--json" in query
                is_def = query[0] == "def"
                extra = ["--no-index"] if backend == "scan" else []
                if not name.startswith("ranked"):
                    extra += ["--sort", "path"]
                root_args = ["--root", "."] if is_def else ["."]
                commands = {label: [str(binary), *query, *root_args, *common, *extra]
                            for label, binary in binaries.items()}
                samples = {label: [] for label in binaries}
                outputs = {label: [] for label in binaries}
                for _ in range(metadata["warmups"]):
                    for label in binaries:
                        run(commands[label], root, environments[label])
                for _ in range(args.runs):
                    order = list(binaries)
                    rng.shuffle(order)
                    for label in order:
                        output, ms = run(commands[label], root, environments[label])
                        samples[label].append(ms)
                        outputs[label].append(output)
                row = {"case": name, "backend": backend, "query": query, "common_flags": common,
                       "backend_flags": extra}
                oracle = None
                if not name.startswith("ranked"):
                    rg_query = ["-n", "--with-filename", *query[2:]] if query[:2] == ["--budget", "0"] else query
                    if is_def:
                        rg_query = ["-l", "-w", query[1], *(["-i"] if "-i" in query else [])]
                    oracle, _ = run([rg, "--no-config", "--color", "never", "--sort", "path", *rg_query, "src"],
                                    root, environments["candidate"])
                for label in binaries:
                    values = samples[label]
                    outs = outputs[label]
                    first = outs[0]
                    first_stdout = stable_stdout(first.stdout, is_json)
                    if any((o.returncode, stable_stdout(o.stdout, is_json), o.stderr)
                           != (first.returncode, first_stdout, first.stderr) for o in outs):
                        raise RuntimeError(f"nondeterministic output: {backend}/{name}/{label}")
                    stream = first.stdout + first.stderr
                    row[label] = {
                        "median_ms": statistics.median(values),
                        "p95_ms": sorted(values)[math.ceil(len(values) * .95) - 1],
                        "samples_ms": values, "exit": first.returncode,
                        "stdout_bytes": len(first.stdout), "stderr_bytes": len(first.stderr),
                        "stdout_sha256": digest(first.stdout),
                        "stable_stdout_sha256": digest(first_stdout),
                        "tokens_both_streams": len(encoding.encode(stream.decode(), disallowed_special=())) if encoding else None,
                        "rg_stdout_and_status_equal": (first.stdout == oracle.stdout and first.returncode == oracle.returncode) if oracle and not is_json and not is_def else None,
                        "json_exact_contract": json_exact_contract(first, oracle) if is_json else None,
                        "definition_contract": definition_contract(first, oracle) if is_def else None,
                    }
                row["stdout_and_status_unchanged"] = (outputs["baseline"][0].returncode, stable_stdout(outputs["baseline"][0].stdout, is_json)) == (outputs["candidate"][0].returncode, stable_stdout(outputs["candidate"][0].stdout, is_json))
                row["median_change_percent"] = 100 * (row["candidate"]["median_ms"] / row["baseline"]["median_ms"] - 1)
                metadata["results"].append(row)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"Wrote {len(metadata['results'])} paired cases to {args.output}")
    failures = [r for r in metadata["results"] if r["candidate"]["rg_stdout_and_status_equal"] is False
                or r["candidate"]["json_exact_contract"] is False or r["candidate"]["definition_contract"] is False]
    if failures:
        raise SystemExit(f"candidate parity failures: {[(r['backend'], r['case']) for r in failures]}")


if __name__ == "__main__":
    main()
