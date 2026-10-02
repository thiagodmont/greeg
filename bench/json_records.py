"""Records of `rg --json` and `greeg --json=greeg`, with paths kept lossless."""
import base64
import json
import os


def text_of(t):
    """A `{"text"}` or `{"bytes"}` value as a str; bytes decode as file names do."""
    return t["text"] if "text" in t else os.fsdecode(base64.b64decode(t["bytes"]))


def path_of(t):
    return text_of(t).removeprefix("./")


def records(text):
    """(type, data, path) per record. A greeg `match` or `context` record has
    no path of its own: it takes the one of its file's `begin`."""
    path = None
    for line in text.splitlines():
        if not line.startswith("{"):
            continue
        try:
            j = json.loads(line)
        except ValueError:
            continue
        data = j.get("data")
        if not isinstance(data, dict):
            continue
        if j.get("type") == "begin":
            path = path_of(data["path"])
        own = data.get("path")
        yield j.get("type"), data, path_of(own) if isinstance(own, dict) else path


def matches(text):
    """(path, line, data) of each match record."""
    for kind, data, path in records(text):
        if kind == "match":
            yield path, data.get("line_number", data.get("line")), data
