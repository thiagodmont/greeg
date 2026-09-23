#!/usr/bin/env python3
"""Paired hook-configuration checks in disposable homes.

python3 bench/hook_config.py BASELINE CANDIDATE --runs 51 --output result.json
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import platform
import random
import statistics
import subprocess
import tempfile
import time
import tomllib
import shutil
from typing import NamedTuple

from hooks import benchmark_environment


class Case(NamedTuple):
    name: str
    uninstall: bool
    original: bytes | None
    expected: bytes | dict | None
    code: int = 0
    retained: tuple[bytes, ...] = ()


def fixtures(agent):
    command = "greeg hook run" + (" --agent codex" if agent == "codex" else "")
    handler = {"type": "command", "command": command}
    other = {"type": "command", "command": "audit", "timeout": 7}
    def config(handlers, matcher="Bash", label=False):
        entry = {"hooks": handlers}
        if matcher is not None:
            entry["matcher"] = matcher
        if label:
            entry["label"] = "shared"
        return {"hooks": {"PreToolUse": [entry]}}
    def encode(value):
        if agent == "claude":
            return json.dumps(value).encode()
        entries = value["hooks"]["PreToolUse"]
        entry = entries[0]
        lines = ["# fixture", "[[hooks.PreToolUse]]"]
        if "matcher" in entry:
            lines.append(f"matcher = {json.dumps(entry['matcher'])}")
        if "label" in entry:
            lines.append('label = "shared"')
        for h in entry["hooks"]:
            lines.append("[[hooks.PreToolUse.hooks]]")
            if h == other:
                lines.append("# retained handler")
            lines.extend(f"{key} = {json.dumps(val)}" for key, val in h.items())
        return ("\n".join(lines) + "\n").encode()
    mixed = config([handler, other], label=True)
    mixed_original = encode(mixed)
    mixed_expected = config([other], label=True)
    retained = ()
    if agent == "codex":
        state = b'\n[hooks.state."config.toml:pre_tool_use:0:1"]\ntrusted_hash = "sha256:keep"\nenabled = true\n'
        mixed_original += state
        mixed_expected["hooks"]["state"] = tomllib.loads(state.decode())["hooks"]["state"]
        retained = (b"# fixture", b"# retained handler", state)
    prefix = config([{"type": "command", "command": "greeg hook-helper"}])
    prompt = config([{"type": "prompt", "command": command}])
    wrong = config([handler], matcher="Edit")
    installed = config([handler])
    appended = {"hooks": {"PreToolUse": wrong["hooks"]["PreToolUse"] + installed["hooks"]["PreToolUse"]}}
    removed_all = {"hooks": {"PreToolUse": []}} if agent == "claude" else {}
    custom_empty = config([], matcher="Edit")
    if agent == "codex":
        del custom_empty["hooks"]["PreToolUse"][0]["hooks"]
    return [
        Case("mixed_uninstall", True, mixed_original, mixed_expected, retained=retained),
        Case("prefix_uninstall", True, encode(prefix), encode(prefix)),
        Case("prompt_uninstall", True, encode(prompt), encode(prompt)),
        Case("wrong_matcher_install", False, encode(wrong), appended),
        Case("installed_noop", False, encode(installed), encode(installed)),
        Case("matcherless_uninstall", True, encode(config([handler], matcher=None)), removed_all),
        Case("custom_matcher_uninstall", True, encode(wrong), custom_empty),
        Case("invalid_utf8", False, b"{\xff}", b"{\xff}", 2),
        Case("absent_uninstall", True, None, None),
        Case("empty_install", False, None, installed),
    ]


def invoke(binary, agent, case, base, env):
    home = base / "home"
    if home.exists():
        shutil.rmtree(home)
    path = home / (".claude/settings.json" if agent == "claude" else ".codex/config.toml")
    _, uninstall, original, expected, code, retained = case
    if original is not None:
        path.parent.mkdir(parents=True)
        path.write_bytes(original)
    args = [str(binary), "hook", agent] + (["--uninstall"] if uninstall else [])
    start = time.perf_counter()
    try:
        run = subprocess.run(args, cwd=base, env=env, stdin=subprocess.DEVNULL,
                             capture_output=True, timeout=10)
    except (subprocess.TimeoutExpired, OSError) as exc:
        return {"ms": (time.perf_counter() - start) * 1000, "contract": False,
                "status": None, "error": type(exc).__name__,
                "stdout_bytes": len(getattr(exc, "stdout", None) or b""),
                "stderr_bytes": len(getattr(exc, "stderr", None) or b"")}
    elapsed = (time.perf_counter() - start) * 1000
    actual = path.read_bytes() if path.exists() else None
    text_preserved = all(fragment in (actual or b"") for fragment in retained)
    if isinstance(expected, dict):
        try:
            actual = json.loads(actual) if agent == "claude" else tomllib.loads(actual.decode())
        except (ValueError, AttributeError, TypeError):
            actual = None
    return {"ms": elapsed, "contract": run.returncode == code and actual == expected and text_preserved,
            "status": run.returncode, "stdout_bytes": len(run.stdout), "stderr_bytes": len(run.stderr)}


def summary(samples):
    times = [s["ms"] for s in samples]
    return {"samples_ms": times, "median_ms": statistics.median(times),
            "p95_ms": sorted(times)[math.ceil(.95 * len(times)) - 1],
            "contract": all(s["contract"] for s in samples),
            "statuses": sorted({s["status"] for s in samples if s["status"] is not None}),
            "errors": sorted({s["error"] for s in samples if "error" in s}),
            "failed_invocations": sum("error" in s for s in samples),
            "stdout_bytes": sorted({s["stdout_bytes"] for s in samples}),
            "stderr_bytes": sorted({s["stderr_bytes"] for s in samples})}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--runs", type=int, default=51)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    binaries = {k: getattr(args, k).resolve(strict=True) for k in ("baseline", "candidate")}
    report = {"protocol": 2, "platform": platform.platform(), "python": platform.python_version(),
              "runs": args.runs, "warmups": 3, "seed": 20260923,
              "scope": "Installer configuration ownership and process wall time; no live-host, native-search or agent-token claim",
              "failure_policy": "Invocation timeouts/launch errors are failed contracts with actual elapsed time and partial output sizes; timing ratios omitted for rows with invocation failures",
              "environment_policy": "Isolated HOME/CODEX_HOME/config/cache/index; scrub inherited GREEG settings; stats disabled",
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "fixtures_sha256": hashlib.sha256(repr([(a, fixtures(a)) for a in ("claude", "codex")]).encode()).hexdigest(),
              "binaries": {}, "results": []}
    rng = random.Random(report["seed"])
    with tempfile.TemporaryDirectory(prefix="greeg-hook-config-") as temp:
        base = Path(temp)
        env = benchmark_environment(base)
        env["CODEX_HOME"] = str(base / "home/.codex")
        for name, binary in binaries.items():
            version = subprocess.run([str(binary), "--version"], env=env, cwd=base,
                                     capture_output=True, text=True, check=True, timeout=10).stdout.strip()
            report["binaries"][name] = {"version": version, "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
        for agent in ("claude", "codex"):
            for case in fixtures(agent):
                samples = {label: [] for label in binaries}
                for i in range(3 + args.runs):
                    labels = list(binaries)
                    rng.shuffle(labels)
                    for label in labels:
                        result = invoke(binaries[label], agent, case, base, env)
                        if i >= 3:
                            samples[label].append(result)
                row = {"agent": agent, "case": case[0], **{k: summary(v) for k, v in samples.items()}}
                for metric in ("median", "p95"):
                    row[f"{metric}_change_percent"] = (
                        None if any(row[k]["failed_invocations"] for k in binaries)
                        else (row["candidate"][f"{metric}_ms"] / row["baseline"][f"{metric}_ms"] - 1) * 100)
                report["results"].append(row)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    for label in binaries:
        print(f"{label}: {sum(r[label]['contract'] for r in report['results'])}/{len(report['results'])} configuration contracts")
    print(f"wrote {args.output}")
    return 0 if all(r["candidate"]["contract"] for r in report["results"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
