"""Shared formatting for paired benchmark reports; no benchmark execution."""
from datetime import timezone
import math
import os
from urllib.parse import quote


def report_heading(entry, *, title=None, level=2):
    out = [f"{'#' * level} {entry.title if title is None else title}", ""]
    if entry.pr is not None:
        out += [f"Originating PR: [#{entry.pr}](https://github.com/thiagodmont/greeg/pull/{entry.pr}).", ""]
    if entry.measured_at is not None:
        timestamp = entry.measured_at.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")
        if entry.timestamp_source == "first_commit":
            commit = entry.timestamp_commit
            out += [f"Measurement time: {timestamp} (estimate from first dataset commit "
                    f"[{commit[:7]}](https://github.com/thiagodmont/greeg/commit/{commit}); "
                    "execution time was not recorded).", ""]
        else:
            out += [f"Measurement time: {timestamp} (recorded).", ""]
    if entry.notes:
        out += [entry.notes, ""]
    return out


def result_link(results_dir, output_dir, filename):
    return quote(os.path.relpath(os.path.realpath(os.path.join(results_dir, filename)),
                                 os.path.realpath(output_dir)))


def finite_number(value):
    return isinstance(value, (int, float)) and math.isfinite(value)


def paired_table(rows, group_key, group_title, *, tokenizer=None, token_key=None, token_title="Tokens"):
    headers = [group_title, "Case", "Median ms, before → after", "p95 ms, before → after"]
    if token_key:
        headers.append(f"{token_title}, before → after")
    out = ["| " + " | ".join(headers) + " |", "|---|---|" + "---:|" * (len(headers) - 2)]
    for row in rows:
        cells = [row[group_key], row["case"].replace("_", " ")]
        for metric in ("median_ms", "p95_ms"):
            values = [row[label].get(metric) for label in ("baseline", "candidate")]
            cells.append(" → ".join(f"{v:.3f}" if finite_number(v) else "n/a" for v in values))
        if token_key:
            cells.append(" → ".join(
                str(row[label][token_key]) if tokenizer and row[label].get(token_key) is not None else "n/a"
                for label in ("baseline", "candidate")))
        out.append("| " + " | ".join(cells) + " |")
    return out


def latency_summary(rows):
    """Return no comparison for empty, failed or undefined timing pairs."""
    changes = []
    flagged = 0
    for row in rows:
        before, after = row["baseline"], row["candidate"]
        if any(v.get("failed_invocations", 0) for v in (before, after)):
            return None
        pair = []
        for metric in ("median_ms", "p95_ms"):
            a, b = before.get(metric), after.get(metric)
            if not all(finite_number(v) for v in (a, b)) or a <= 0 or b < 0:
                return None
            pair.append(100 * (b / a - 1))
        changes.append(pair)
        flagged += after["median_ms"] > before["median_ms"] * 1.1 or after["p95_ms"] > before["p95_ms"] * 1.2
    if not changes:
        return None
    return {"flagged": flagged,
            "median_min": min(m for m, _ in changes), "median_max": max(m for m, _ in changes),
            "p95_max": max(p for _, p in changes)}
