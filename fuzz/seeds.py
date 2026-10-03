"""Write the fuzz targets' seed inputs to fuzz/seeds/TARGET/.

Index seeds are the component bodies of a real index of a small tree, built
with the greeg binary given (default target/debug/greeg).
usage: python3 fuzz/seeds.py [GREEG]"""
import os, pathlib, subprocess, sys, tempfile

HERE = pathlib.Path(__file__).resolve().parent
GREEG = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else HERE.parent / "target/debug/greeg")
HEADER_LEN = 48  # format::HEADER_LEN
MAGIC = b"GREEG\0\0\0"  # format::MAGIC
# component id (format::COMP_*) -> the index_decode selector
SELECTOR = {1: 0, 2: 1, 8: 2, 5: 3, 6: 4, 7: 5, 9: 7}

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
        "flags": b"rg -uu -w --max-count 3 -C 2 Needle",
        "dash_value": b"rg --glob=-a.txt -g -b.rs Needle",
        "fixed": b'rg -F "x.y" src/lib.rs',
        "verb": b"rg def",
        "pipe": b"rg foo | head",
        "grep": b"grep -r foo .",
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
    found = 0
    for f in sorted(index.rglob("*")):
        b = f.read_bytes() if f.is_file() else b""
        if len(b) < HEADER_LEN or b[:8] != MAGIC:
            continue
        comp = b[12]
        if comp not in SELECTOR:
            continue
        n = int.from_bytes(b[16:24], "little")
        body = b[HEADER_LEN:HEADER_LEN + n]
        write("index_decode", f"comp{comp}", bytes([SELECTOR[comp]]) + body)
        if comp == 9:
            # the skipped list is also read from its whole file
            write("index_decode", "skipped_file", bytes([SELECTOR[comp]]) + b)
        found += 1
    if found < 7:
        sys.exit(f"found {found} index components, want 7")
print("seeds written to", HERE / "seeds")
