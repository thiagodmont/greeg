"""Write the fuzz targets' seed inputs to fuzz/seeds/TARGET/.

Index seeds are the component bodies of a real index of a small tree, built
with the greeg binary given (default target/debug/greeg).
usage: python3 fuzz/seeds.py [GREEG]"""
import os, pathlib, re, subprocess, sys, tempfile

HERE = pathlib.Path(__file__).resolve().parent
GREEG = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else HERE.parent / "target/debug/greeg")
INDEX_SRC = HERE.parent / "crates/greeg-index/src"


def const(file, name):
    m = re.search(rf"pub const {name}: [^=]+= (.+);", (INDEX_SRC / file).read_text())
    if not m:
        sys.exit(f"{name} not found in {file}")
    return m.group(1)


# the header this script reads: magic, format u32 at 8, component u8 at 12, payload length u64 at 16
HEADER_LEN = 48
MAGIC = b"GREEG\0\0\0"
if (const("format.rs", "HEADER_LEN"), const("format.rs", "MAGIC")) != ("48", r'b"GREEG\0\0\0"'):
    sys.exit("the index header changed: update seeds.py")
FORMAT_VERSION = int(const("lib.rs", "FORMAT_VERSION"))
# component (format::COMP_*) -> the index_decode selector
SELECTOR = {
    int(const("format.rs", f"COMP_{name}")): sel
    for name, sel in [("FILES", 0), ("GRAMS", 1), ("WORDS", 2), ("SYMBOLS", 3), ("SPANS", 4), ("GRAPH", 5), ("SKIPPED", 7)]
}

SOURCES = {
    "src/lib.rs": "use std::io;\n/// Doc.\npub fn spawn_task(n: u32) -> io::Result<u32> {\n    helper(n) // call\n}\nfn helper(n: u32) -> io::Result<u32> { Ok(n) }\npub struct Pool { size: usize }\nimpl Pool {\n    pub fn new() -> Self { Pool { size: 4 } }\n}\n",
    "src/util.py": "import os\nfrom pathlib import Path\n\nclass Loader:\n    \"\"\"Loads.\"\"\"\n    def load(self, p):\n        return Path(p).read_text()  # read\n",
    "web/app.ts": "import { x } from './dep';\nexport interface Shape { area(): number }\nexport class Square implements Shape {\n  area() { return x * x; }\n}\n",
    "web/dep.ts": "export const x = 3;\n",
    "web/view.js": "import { x } from './dep';\nexport function render(n) { return `${n}:${x}`; }\n",
    "app/Main.kt": "package app\n\nimport kotlin.math.max\n\nclass Main {\n    fun run(a: Int): Int = max(a, 1)\n}\n",
    "README.md": "# demo\nspawn_task loads things.\n",
}

TEXT = {
    "regex_plan": {
        "word": b"\x00spawn_task\x00let t = spawn_task(4);",
        "class": b"\x00[a-c]{2,}\\d+\x00zz abc42",
        "casei": b"\x02Hello\\s+World\x00hello   WORLD",
        "literal": b"\x01a.b(c)\x00x a.b(c) y",
        "alt": b"\x00(?:foo|ba[rz])_\\w+\x00let baz_q = 1;",
        "unicode": b"\x02stra\xc3\x9fe\x00STRASSE stra\xc3\x9fe",
        "anchors": b"\x00^fn \\w+\\($\x00fn main(",
        "multiline": b"\x04^fn \\w+\\($\x00// x\nfn main(\n",
    },
    "shell_rewrite": {
        "plain": b"rg -n foo src",
        "quoted": b"rg -i 'a b' -g '*.rs' .",
        "dashdash": b"rg -e pattern -- -x file",
        "flags": b"rg -uu -w --max-columns 80 -C 2 Needle",
        "dash_value": b"rg --glob=-a.txt -g -b.rs Needle",
        "fixed": b'rg -F "x.y" src/lib.rs',
        "verb": b"rg def",
        "pipe": b"rg foo | head",
        "grep": b"grep -r foo .",
        "head-list": b"rg -n a | head -5; rg b",
        "grep-head": b"grep -rn a . | head -n 3",
        "redirections": b"rg -n a 2>/dev/null | head -5; grep -rl b . 2>&1 </dev/null",
    },
    "lang_extract": {
        "python": b"\x00" + SOURCES["src/util.py"].encode(),
        "rust": b"\x01" + SOURCES["src/lib.rs"].encode(),
        "javascript": b"\x02" + SOURCES["web/view.js"].encode(),
        "typescript": b"\x03" + SOURCES["web/app.ts"].encode(),
        "tsx": b"\x83const A = () => <div className=\"a\">{x}</div>;\nexport default A;\n",
        "kotlin": b"\x04" + SOURCES["app/Main.kt"].encode(),
        "unterminated": b"\x01fn a() { let s = \"open /* r#\"x",
    },
}


def write(target, name, data):
    d = HERE / "seeds" / target
    d.mkdir(parents=True, exist_ok=True)
    (d / name).write_bytes(data)


for target, seeds in TEXT.items():
    for name, data in seeds.items():
        write(target, name, data)

with tempfile.TemporaryDirectory() as t:
    root, index = pathlib.Path(t, "tree"), pathlib.Path(t, "index")
    (root / ".git").mkdir(parents=True)
    for rel, body in SOURCES.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(body)
    env = dict(os.environ, HOME=t, GREEG_INDEX_DIR=str(index), GREEG_STATS="0", GREEG_CONFIG_DIR="/dev/null/greeg-config")
    subprocess.run([GREEG, "index", "--quiet"], cwd=root, env=env, check=True)
    found = set()
    for f in sorted(index.rglob("*")):
        b = f.read_bytes() if f.is_file() else b""
        if len(b) < HEADER_LEN or b[:8] != MAGIC:
            continue
        version = int.from_bytes(b[8:12], "little")
        if version != FORMAT_VERSION:
            sys.exit(f"{f}: index format {version}, want {FORMAT_VERSION}")
        comp = b[12]
        if comp not in SELECTOR:
            continue
        n = int.from_bytes(b[16:24], "little")
        if HEADER_LEN + n > len(b):
            sys.exit(f"{f}: payload of {n} bytes past the end of a {len(b)}-byte file")
        body = b[HEADER_LEN:HEADER_LEN + n]
        write("index_decode", f"comp{comp}", bytes([SELECTOR[comp]]) + body)
        if SELECTOR[comp] == 7:
            # the skipped list is also read from its whole file
            write("index_decode", "skipped_file", bytes([SELECTOR[comp]]) + b)
        found.add(comp)
    if found != SELECTOR.keys():
        sys.exit(f"found index components {sorted(found)}, want {sorted(SELECTOR)}")
print("seeds written to", HERE / "seeds")
