//! Index answers equal scan answers: seeded random trees and patterns, every
//! match compared as ripgrep's records (`--json=rg`), before and after edits
//! the index has not published. With `rg` on PATH, ripgrep's answer too
//! (`GREEG_TEST_RG=1` requires it). `GREEG_TEST_SEED` runs another seed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_greeg");
static N: AtomicUsize = AtomicUsize::new(0);

/// xorshift64: small, seeded, reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick(&mut self, v: &[&'static str]) -> &'static str {
        v[self.below(v.len())]
    }
}

const WORDS: &[&str] = &[
    "foo", "fooBar", "foo_bar", "FOO", "Foo", "foobar", "bar", "baz", "qux", "x1", "ab", "abc",
    "fn", "def", "class", "return", "self", "café", "naïve", "日本", "Δx", "a1b2", "_tmp",
];
const GLUE: &[&str] = &[
    " ", " ", " ", "(", ")", ".", ", ", " = ", "::", "->", "\t", "  ", "\"", "'", ";", "{", "}",
    "[", "]", " + ", "#", "//", "/*", "*/", "0", "42", "1234",
];

fn line(rng: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..rng.below(9) {
        s.push_str(rng.pick(WORDS));
        s.push_str(rng.pick(GLUE));
    }
    s
}

fn file(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    if rng.below(20) == 0 {
        out.extend_from_slice(b"\xEF\xBB\xBF");
    }
    let crlf = rng.below(8) == 0;
    for _ in 0..1 + rng.below(40) {
        out.extend_from_slice(line(rng).as_bytes());
        out.extend_from_slice(if crlf { b"\r\n" } else { b"\n" });
    }
    out
}

const EXTS: &[&str] = &["rs", "py", "ts", "txt", "kt", "md"];
const DIRS: &[&str] = &["", "src/", "src/a/", "lib/", "tests/", "docs/x/"];

fn rel(rng: &mut Rng, i: usize) -> String {
    format!("{}f{i}.{}", rng.pick(DIRS), rng.pick(EXTS))
}

fn write(root: &Path, rel: &str, body: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A pattern and its flags: literals taken from the files, words, and regexes.
fn query(rng: &mut Rng, corpus: &[Vec<u8>]) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    match rng.below(10) {
        0..=3 => {
            let text = &corpus[rng.below(corpus.len())];
            let s = String::from_utf8_lossy(text).into_owned();
            let chars: Vec<char> = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            if chars.is_empty() {
                a.push("foo".into());
            } else {
                let start = rng.below(chars.len());
                let len = 1 + rng.below(12);
                let lit: String = chars[start..(start + len).min(chars.len())]
                    .iter()
                    .collect();
                a.extend(["-F".into(), "-e".into(), lit]);
            }
        }
        4..=6 => {
            a.push("-e".into());
            a.push(rng.pick(WORDS).to_string());
        }
        _ => {
            let re = rng.pick(&[
                r"foo\w+",
                r"[a-c]+x",
                r"^\s*fn",
                r"bar$",
                r"(foo|qux)_",
                r"\d{2,}",
                r"é+",
                r"Foo|FOO",
                r"\bab\b",
                r"x1|a1b2",
                r"self\.",
                r"[日Δ]",
                r"_?tmp",
                r"ba[rz]\(",
            ]);
            a.extend(["-e".into(), re.into()]);
        }
    }
    for (p, flag) in [(4, "-i"), (4, "-w"), (6, "-S"), (8, "-x")] {
        if rng.below(p) == 0 {
            a.push(flag.into());
        }
    }
    a
}

type Matches = BTreeSet<(String, u64, Vec<(u64, u64)>)>;

fn matches(o: &Output) -> Matches {
    let mut set = Matches::new();
    for l in o.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let v: serde_json::Value = serde_json::from_slice(l).unwrap();
        if v["type"] != "match" {
            continue;
        }
        let d = &v["data"];
        let path = d["path"]["text"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("./")
            .to_string();
        let subs = d["submatches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["start"].as_u64().unwrap(), s["end"].as_u64().unwrap()))
            .collect();
        set.insert((path, d["line_number"].as_u64().unwrap(), subs));
    }
    set
}

struct Tree {
    root: PathBuf,
    index: PathBuf,
    home: PathBuf,
}

impl Tree {
    /// A new directory of its own: one left by another run is never reused.
    fn new() -> Tree {
        let base = loop {
            let base = std::env::temp_dir().join(format!(
                "greeg-diff-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&base) {
                Ok(()) => break base,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("{}: {e}", base.display()),
            }
        };
        let t = Tree {
            root: base.join("tree"),
            index: base.join("index"),
            home: base.join("home"),
        };
        fs::create_dir_all(&t.root).unwrap();
        fs::create_dir_all(&t.home).unwrap();
        t
    }

    fn greeg(&self, args: &[String], extra: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .args(extra)
            .args(["--json=rg", "--no-session"])
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("GREEG_STATS", "0")
            .env("GREEG_CONFIG_DIR", "/dev/null/greeg-config")
            .env("GREEG_INDEX_DIR", &self.index)
            .output()
            .unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.root.parent().unwrap());
    }
}

fn rg_available() -> bool {
    let found = Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        found || std::env::var_os("GREEG_TEST_RG").is_none(),
        "GREEG_TEST_RG is set but rg is not on PATH"
    );
    found
}

/// The seed: `GREEG_TEST_SEED` (0, which xorshift cannot use, runs as 1),
/// else a fixed one.
fn seed(fixed: u64) -> u64 {
    std::env::var("GREEG_TEST_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(fixed, |s| s.max(1))
}

#[test]
fn index_and_scan_find_the_same_matches() {
    let t = Tree::new();
    let seed = seed(0x2545_f491_4f6c_dd1d);
    let mut rng = Rng(seed);
    let mut files: Vec<(String, Vec<u8>)> = (0..60)
        .map(|i| (rel(&mut rng, i), file(&mut rng)))
        .collect();
    for (r, b) in &files {
        write(&t.root, r, b);
    }
    let built = Command::new(BIN)
        .args(["index", "--quiet"])
        .current_dir(&t.root)
        .env("HOME", &t.home)
        .env("GREEG_STATS", "0")
        .env("GREEG_INDEX_DIR", &t.index)
        .output()
        .unwrap();
    assert!(built.status.success());
    let rg = rg_available();
    let mut compared = 0;
    for phase in ["built", "edited"] {
        if phase == "edited" {
            // edits the index has not published: answered from disk
            for _ in 0..8 {
                let i = rng.below(files.len());
                files[i].1 = file(&mut rng);
                write(&t.root, &files[i].0, &files[i].1);
            }
            let gone = files.remove(rng.below(files.len()));
            fs::remove_file(t.root.join(&gone.0)).unwrap();
            let new = (rel(&mut rng, 100), file(&mut rng));
            write(&t.root, &new.0, &new.1);
            files.push(new);
        }
        let corpus: Vec<Vec<u8>> = files.iter().map(|(_, b)| b.clone()).collect();
        for _ in 0..120 {
            let q = query(&mut rng, &corpus);
            let index = t.greeg(&q, &["--fresh", "stat"]);
            let scan = t.greeg(&q, &["--no-index"]);
            let ctx = format!("seed {seed} {phase} {q:?}");
            assert_eq!(
                index.status.code(),
                scan.status.code(),
                "{ctx}\n{}",
                String::from_utf8_lossy(&index.stderr)
            );
            if scan.status.code() == Some(2) {
                continue;
            }
            let (mi, ms) = (matches(&index), matches(&scan));
            assert_eq!(
                mi.symmetric_difference(&ms).take(5).collect::<Vec<_>>(),
                Vec::<&(String, u64, Vec<(u64, u64)>)>::new(),
                "{ctx}: index {} vs scan {} matches",
                mi.len(),
                ms.len()
            );
            if rg {
                let r = Command::new("rg")
                    .args(&q)
                    .args(["--json", "--no-config", "."])
                    .current_dir(&t.root)
                    .output()
                    .unwrap();
                assert_eq!(matches(&r), ms, "{ctx}: ripgrep");
            }
            compared += 1;
        }
    }
    assert!(compared > 200, "{compared} queries compared");
}
