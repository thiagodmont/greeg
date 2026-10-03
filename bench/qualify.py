#!/usr/bin/env python3
"""Qualify a release archive as users get it: its files, its version, and a
small semantic suite run with the archive's own binary (scan, index build,
indexed lookup, symbols, an edit picked up, byte-safe JSON, exit statuses).
Prints the runtime floor the binary needs. Standard library only.

Usage: qualify.py ARCHIVE_DIR VERSION    (ARCHIVE_DIR = the extracted greeg-VERSION-TARGET/)"""
import base64, json, os, platform, re, shutil, subprocess, sys, tempfile, time

failures = []


def check(ok, what):
    print(f"  {'ok  ' if ok else 'FAIL'} {what}")
    if not ok:
        failures.append(what)


def main(archive, version):
    exe = os.path.join(archive, "greeg")
    print(f"archive {archive}")
    check(os.access(exe, os.X_OK), "greeg is executable")
    for name in ("README.md", "LICENSE-MIT"):
        check(os.path.isfile(os.path.join(archive, name)), f"{name} is present")
    man = os.path.join(archive, "greeg.1")
    page = open(man, "rb").read() if os.path.isfile(man) else b""
    check(f'.TH greeg 1  "greeg {version}"'.encode() in page, f"greeg.1 is the man page of greeg {version}")
    v = subprocess.run([exe, "--version"], capture_output=True, text=True)
    check(v.stdout.strip() == f"greeg {version}", f"--version is `greeg {version}` (got `{v.stdout.strip()}`)")
    floor(exe)
    # a detached refresh may still be writing the index when the suite ends
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as t:
        suite(exe, t)
    if failures:
        print(f"{len(failures)} check(s) failed")
        sys.exit(1)
    print("qualified")


def floor(exe):
    """The oldest system the binary can load on, as far as its headers say."""
    if platform.system() == "Darwin" and shutil.which("otool"):
        out = subprocess.run(["otool", "-l", exe], capture_output=True, text=True).stdout
        m = re.search(r"LC_BUILD_VERSION.*?minos (\S+)", out, re.S)
        print(f"  floor macOS {m.group(1) if m else '?'} (minimum the binary declares)")
    elif platform.system() == "Linux" and shutil.which("objdump"):
        out = subprocess.run(["objdump", "-T", exe], capture_output=True, text=True).stdout
        vs = sorted({tuple(map(int, g.split("."))) for g in re.findall(r"GLIBC_([0-9.]+)", out)})
        print(f"  floor glibc {'.'.join(map(str, vs[-1])) if vs else '?'} (newest symbol version it links)")


def suite(exe, t):
    root = os.path.join(t, "tree")
    home = os.path.join(t, "home")
    os.makedirs(os.path.join(root, "src"))
    os.makedirs(home)
    env = dict(os.environ, HOME=home, GREEG_STATS="0", GREEG_INDEX_DIR=os.path.join(t, "index"),
               GREEG_CONFIG_DIR="/dev/null/greeg-config", XDG_CACHE_HOME=os.path.join(t, "cache"))
    env.pop("GREEG_BUDGET", None)

    def write(rel, data):
        p = os.path.join(root, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(data)

    def run(*args):
        return subprocess.run([exe, *args, "--no-session"], cwd=root, env=env, capture_output=True,
                              stdin=subprocess.DEVNULL)

    write("src/lib.rs", b"pub fn alpha() -> u32 {\n    1\n}\n\npub fn caller() -> u32 {\n    alpha()\n}\n")
    write("app.py", b"def alpha_py():\n    return 1\n")
    write("notes.txt", "café alpha naïve\n".encode())
    write("wide.txt", b"\xff\xfe" + "alpha in utf-16\n".encode("utf-16-le"))
    raw = b"bad\xffname.txt"
    raw_ok = platform.system() == "Linux"
    if raw_ok:
        with open(os.path.join(root.encode(), raw), "wb") as f:
            f.write(b"alpha raw\n")

    print("semantic suite")
    scan = run("alpha", "--no-index", "--budget", "0")
    out = scan.stdout.decode(errors="replace")
    check(scan.returncode == 0 and "src/lib.rs" in out and "wide.txt" in out,
          "scan finds a match in UTF-8 and UTF-16 files, exit 0")
    check(run("zzz_absent", "--no-index").returncode == 1, "no match exits 1")
    check(run("(", "--no-index").returncode == 2, "an invalid pattern exits 2")
    built = run("index", "--quiet")
    check(built.returncode == 0, "index builds")
    idx = run("alpha", "--fresh", "stat", "--budget", "0", "--stats")
    check(idx.returncode == 0 and b"greeg: index" in idx.stderr and sorted(idx.stdout.splitlines()) ==
          sorted(scan.stdout.splitlines()), "the index answers as the scan does")
    d = run("def", "alpha", "--fresh", "stat")
    check(d.returncode == 0 and b"src/lib.rs" in d.stdout, "def finds the definition from the index")
    o = run("outline", "src/lib.rs")
    check(o.returncode == 0 and b"alpha" in o.stdout and b"caller" in o.stdout, "outline lists the file's symbols")
    time.sleep(0.15)  # past the freshness TTL
    with open(os.path.join(root, "src/lib.rs"), "ab") as f:
        f.write(b"\npub fn beta_new() {}\n")
    e = run("beta_new", "--fresh", "stat", "--budget", "0")
    check(e.returncode == 0 and b"src/lib.rs" in e.stdout, "an edit is answered by the next query")
    j = run("alpha", "--json=greeg", "--budget", "0", "--fresh", "stat")
    try:
        records = [json.loads(line) for line in j.stdout.splitlines()]
    except ValueError:
        records = []
    check(j.returncode == 0 and records and records[0].get("type") == "greeg"
          and records[-1].get("type") == "footer", "--json=greeg is valid JSON Lines, header to footer")
    if raw_ok:
        paths = [r["data"]["path"] for r in records if r.get("type") == "begin"]
        exact = any("bytes" in p and base64.b64decode(p["bytes"]) == raw for p in paths)
        check(exact, "a non-UTF-8 file name round-trips as bytes")
    rg = run("alpha", "--json=rg", "--fresh", "stat")
    try:
        ok = all(json.loads(line)["type"] for line in rg.stdout.splitlines())
    except (ValueError, KeyError):
        ok = False
    check(rg.returncode == 0 and ok, "--json=rg is valid JSON Lines")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
