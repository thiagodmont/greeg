"""Catalog and report-facing validation for paired benchmark datasets."""
from dataclasses import dataclass
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import sys
import tomllib


class ReportDataError(ValueError):
    pass


def require(condition, source, field, expected):
    if not condition:
        raise ReportDataError(f"{source}: {field}: expected {expected}")


def integer(value, minimum=0):
    return type(value) is int and value >= minimum


def text(value):
    return isinstance(value, str) and bool(value.strip())


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate field {key!r}")
        result[key] = value
    return result


@dataclass(frozen=True)
class Dataset:
    id: str
    section: str
    filename: str
    title: str
    pr: int | None = None
    measured_at: datetime | None = None
    timestamp_source: str = ""
    timestamp_commit: str = ""
    notes: str = ""
    show_rows: bool = True
    analysis: str = ""
    recheck_of: str = ""
    comparison_intro: str = ""
    comparison_limits: str = ""


SECTIONS = {"matching", "matching_recheck", "matching_review", "hook", "hook_config"}


def load_catalog(path):
    try:
        with Path(path).open("rb") as file:
            data = tomllib.load(file)
    except (OSError, ValueError) as error:
        raise ReportDataError(f"{path}: invalid catalog: {error}") from error
    require(data.get("version") == 1 and type(data.get("version")) is int, path, "version", "1")
    require(set(data) == {"version", "datasets"}, path, "catalog", "version and datasets")
    require(isinstance(data["datasets"], list) and data["datasets"], path, "datasets", "nonempty array")
    entries = []
    for i, item in enumerate(data["datasets"]):
        field = f"datasets[{i}]"
        require(isinstance(item, dict), path, field, "table")
        require(set(item) <= Dataset.__dataclass_fields__.keys(), path, field, "known catalog fields")
        for key in ("id", "section", "filename", "title"):
            require(text(item.get(key)), path, f"{field}.{key}", "nonempty string")
        require("pr" not in item or integer(item["pr"], 1), path, f"{field}.pr", "positive PR number")
        timestamp_fields = {"measured_at", "timestamp_source", "timestamp_commit"}
        if timestamp_fields & item.keys():
            measured_at = item.get("measured_at")
            require(isinstance(measured_at, datetime) and measured_at.utcoffset() is not None,
                    path, f"{field}.measured_at", "TOML datetime with an explicit UTC offset")
            try:
                measured_at.astimezone(timezone.utc)
            except OverflowError as error:
                raise ReportDataError(f"{path}: {field}.measured_at: outside supported UTC range") from error
            require(item.get("timestamp_source") in ("measurement", "first_commit"),
                    path, f"{field}.timestamp_source", "measurement or first_commit")
            if item["timestamp_source"] == "first_commit":
                commit = item.get("timestamp_commit")
                require(isinstance(commit, str) and re.fullmatch(r"[0-9a-f]{40}", commit),
                        path, f"{field}.timestamp_commit", "full lowercase source commit SHA")
            else:
                require("timestamp_commit" not in item, path, f"{field}.timestamp_commit",
                        "omitted for a recorded measurement time")
        require(re.fullmatch(r"[a-z][a-z0-9_]*", item["id"]), path, f"{field}.id", "lowercase identifier")
        require(item["section"] in SECTIONS, path, f"{field}.section", "supported report section")
        require(re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]*\.json", item["filename"]),
                path, f"{field}.filename", "JSON basename without directory traversal")
        require(type(item.get("show_rows", True)) is bool, path, f"{field}.show_rows", "boolean")
        require(item.get("show_rows", True) or item["section"] == "matching_review",
                path, f"{field}.show_rows", "false only for matching_review")
        require(item.get("analysis", "") in ("", "publication", "thresholds"), path, f"{field}.analysis", "supported analysis")
        for key in ("notes", "recheck_of", "comparison_intro", "comparison_limits"):
            require(isinstance(item.get(key, ""), str), path, f"{field}.{key}", "string")
        require(not item.get("analysis") or item["section"] == "hook_config", path, field, "analysis on hook_config only")
        require(not item.get("recheck_of") or item["section"] == "hook_config", path, field, "recheck on hook_config only")
        entries.append(Dataset(**item))
    by_id = {e.id: e for e in entries}
    require(len(by_id) == len(entries), path, "datasets.id", "unique identifiers")
    require(len({e.filename for e in entries}) == len(entries), path, "datasets.filename", "unique filenames")
    for section in ("matching", "matching_recheck"):
        require(sum(e.section == section for e in entries) == 1, path, section, "exactly one dataset")
    for entry in entries:
        if entry.recheck_of:
            original = by_id.get(entry.recheck_of)
            require(original and original.section == entry.section and original != entry and not original.recheck_of,
                    path, f"{entry.id}.recheck_of", "a different, non-recheck dataset in the same section")
        require(not (entry.comparison_intro or entry.comparison_limits) or entry.recheck_of,
                path, entry.id, "recheck_of for comparison notes")
    return entries


def validate_dataset(data, entry):
    source = entry.filename
    def check(condition, field, expected):
        require(condition, source, field, expected)
    def obj(value, field):
        check(isinstance(value, dict), field, "object")
        return value
    def string(value, field):
        check(text(value), field, "nonempty string")
    def count(value, field, minimum=0):
        check(integer(value, minimum), field, f"integer >= {minimum}")
    def boolean(value, field, nullable=False):
        check(type(value) is bool or (nullable and value is None), field, "boolean" + (" or null" if nullable else ""))

    obj(data, "dataset")
    if entry.section == "hook_config":
        check(data.get("suite", "configuration") in ("configuration", "skills"), "suite", "configuration or skills")
    protocol = data.get("protocol", 1)
    check(type(protocol) is int and protocol in (1, 2), "protocol", "supported version 1 or 2")
    count(data.get("runs"), "runs", 1)
    count(data.get("warmups"), "warmups")
    for label in ("baseline", "candidate"):
        binary = obj(obj(data.get("binaries"), "binaries").get(label), f"binaries.{label}")
        string(binary.get("version"), f"binaries.{label}.version")
        if binary.get("sha256") is not None:
            check(isinstance(binary["sha256"], str) and re.fullmatch(r"[0-9a-f]{64}", binary["sha256"]),
                  f"binaries.{label}.sha256", "64 lowercase hexadecimal characters or null")
    check(data.get("tokenizer") is None or text(data["tokenizer"]), "tokenizer", "nonempty string or null")
    rows = data.get("results")
    check(isinstance(rows, list), "results", "array (empty is allowed)")
    matching = entry.section.startswith("matching")
    if matching:
        corpus = obj(data.get("corpus"), "corpus")
        count(corpus.get("files"), "corpus.files")
        if entry.section == "matching":
            count(corpus.get("bytes"), "corpus.bytes")
            count(data.get("cpu_count"), "cpu_count", 1)
            string(data.get("platform"), "platform")
            string(data.get("ripgrep"), "ripgrep")
        if entry.section == "matching_review":
            check(isinstance(data.get("cases"), list) and all(text(c) for c in data["cases"]), "cases", "string array")
    if entry.section == "hook":
        string(data.get("ripgrep"), "ripgrep")
        searches = data.get("search_contracts")
        check(isinstance(searches, list), "search_contracts", "array")
        for i, search in enumerate(searches):
            field = f"search_contracts[{i}]"
            obj(search, field)
            check(search.get("binary") in ("baseline", "candidate"), field + ".binary", "baseline or candidate")
            boolean(search.get("stdout_and_status_equal"), field + ".stdout_and_status_equal")
    seen = set()
    for i, row in enumerate(rows):
        field = f"results[{i}]"
        obj(row, field)
        group = "backend" if matching else "agent"
        string(row.get(group), f"{field}.{group}")
        string(row.get("case"), field + ".case")
        key = row[group], row["case"]
        check(key not in seen, field, "unique group/case pair")
        seen.add(key)
        if entry.section == "matching" and row["case"].startswith("hit_"):
            boolean(row.get("stdout_and_status_unchanged"), field + ".stdout_and_status_unchanged")
        for label in ("baseline", "candidate"):
            prefix = f"{field}.{label}"
            sample = obj(row.get(label), prefix)
            for metric in ("median_ms", "p95_ms"):
                check(metric in sample, prefix + "." + metric, "timing or explicit null")
                value = sample[metric]
                check(value is None or (type(value) in (int, float) and 0 <= value <= sys.float_info.max),
                      prefix + "." + metric, "finite nonnegative timing or null")
            failure_default = None if entry.section == "hook_config" and protocol == 2 else 0
            count(sample.get("failed_invocations", failure_default), prefix + ".failed_invocations")
            check(sample.get("failed_invocations", 0) <= data["runs"], prefix + ".failed_invocations", "count <= runs")
            token_key = "tokens_both_streams" if matching else "reply_tokens"
            if sample.get(token_key) is not None:
                count(sample[token_key], prefix + "." + token_key)
            if matching:
                for contract in ("rg_stdout_and_status_equal", "json_exact_contract", "definition_contract"):
                    boolean(sample.get(contract), prefix + "." + contract, nullable=True)
                if entry.section == "matching":
                    check("rg_stdout_and_status_equal" in sample, prefix + ".rg_stdout_and_status_equal", "boolean or explicit null")
            else:
                boolean(sample.get("contract"), prefix + ".contract")
                check(not sample.get("failed_invocations", 0) or not sample["contract"],
                      prefix + ".contract", "false when invocations failed")
        if entry.section == "matching" and row["candidate"]["rg_stdout_and_status_equal"] is not None:
            boolean(row["baseline"]["rg_stdout_and_status_equal"], field + ".baseline.rg_stdout_and_status_equal")
    return data


class ReportDatasets:
    def __init__(self, results_dir, catalog_path=None):
        self.root = Path(results_dir)
        self.entries = load_catalog(catalog_path or Path(__file__).with_name("reports.toml"))
        self._cache = {}

    def section(self, name):
        return [entry for entry in self.entries if entry.section == name]

    def get(self, entry):
        if entry.id not in self._cache:
            path = self.root / entry.filename
            try:
                raw = path.read_text(encoding="utf-8")
            except FileNotFoundError:
                self._cache[entry.id] = None
                return None
            except (OSError, UnicodeError) as error:
                raise ReportDataError(f"{entry.filename}: cannot read dataset: {error}") from error
            try:
                data = json.loads(raw, object_pairs_hook=unique_object)
            except ValueError as error:
                raise ReportDataError(f"{entry.filename}: invalid JSON: {error}") from error
            self._cache[entry.id] = validate_dataset(data, entry)
        return self._cache[entry.id]
