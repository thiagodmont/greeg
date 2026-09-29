//! Two greeg builds on one repository: another build (in CI, the last
//! release) and this one, alternately and at once. Each keeps its own index:
//! neither rebuilds the other's, removes it, or answers wrongly.
//!
//! `GREEG_OTHER_BIN=<greeg> cargo test --test mixed_versions -- --ignored`

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const THIS: &str = env!("CARGO_BIN_EXE_greeg");

/// Long enough for a query's detached build or refresh to finish.
const SETTLE: Duration = Duration::from_millis(1500);

const QUERIES: [&[&str]; 3] = [&["-c", "needle"], &["-l", "-w", "needle"], &["needle"]];

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    index: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn w(p: &Path, body: impl AsRef<[u8]>) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("greeg-mixed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        fs::create_dir_all(root.join(".git/info")).unwrap();
        w(
            &root.join("src/lib.rs"),
            "pub fn needle() -> u32 {\n    1\n}\n",
        );
        w(
            &root.join("src/main.rs"),
            "fn main() {\n    let n = needle();\n}\n",
        );
        w(
            &root.join("app.py"),
            "def needle_py():\n    return 'needle'\n",
        );
        for i in 0..100 {
            w(
                &root.join(format!("filler/f{i:03}.txt")),
                format!("filler {i}\n"),
            );
        }
        let index = base.join("index");
        Fixture { base, root, index }
    }

    fn run(&self, bin: &Path, args: &[&str]) -> Output {
        Command::new(bin)
            .current_dir(&self.root)
            .env("HOME", self.base.join("home"))
            .env("XDG_CACHE_HOME", self.base.join("cache"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("GREEG_STATS", "0")
            .env("GREEG_INDEX_DIR", &self.index)
            .env_remove("GREEG_SESSION")
            .env_remove("RIPGREP_CONFIG_PATH")
            .args(args)
            .arg("--no-session")
            .output()
            .expect("run greeg")
    }

    /// Each layout directory and the build its manifest names; a rebuild
    /// changes the build.
    fn builds(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for e in fs::read_dir(&self.index).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let Ok(bytes) = fs::read(e.path().join("manifest")) else {
                continue;
            };
            let m: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            out.insert(name, m["epoch"].as_u64().unwrap());
        }
        out
    }

    /// Every query answers as that binary's own scan does.
    fn answers_like_a_scan(&self, bin: &Path) {
        for q in QUERIES {
            let indexed = self.run(bin, q);
            let mut scan_args = q.to_vec();
            scan_args.push("--no-index");
            let scan = self.run(bin, &scan_args);
            assert_eq!(indexed.status.code(), Some(0), "{bin:?} {q:?}: {indexed:?}");
            assert_eq!(
                String::from_utf8_lossy(&indexed.stdout),
                String::from_utf8_lossy(&scan.stdout),
                "{bin:?} {q:?}"
            );
        }
        let def = self.run(bin, &["def", "needle"]);
        assert_eq!(def.status.code(), Some(0), "{bin:?} def: {def:?}");
        assert!(
            String::from_utf8_lossy(&def.stdout).contains("src/lib.rs"),
            "{bin:?} def: {def:?}"
        );
    }
}

#[test]
#[ignore = "needs GREEG_OTHER_BIN, another greeg build"]
fn old_and_new_binaries_alternate_without_rebuild_loops() {
    let other = PathBuf::from(std::env::var_os("GREEG_OTHER_BIN").expect("GREEG_OTHER_BIN"));
    let this = PathBuf::from(THIS);
    let f = Fixture::new();
    for bin in [&other, &this] {
        let o = f.run(bin, &["index"]);
        assert!(o.status.success(), "{bin:?} index: {o:?}");
    }
    std::thread::sleep(SETTLE);
    let built = f.builds();
    assert!(!built.is_empty(), "no index was built");

    // upgrade, downgrade, upgrade, and so on
    for _ in 0..3 {
        for bin in [&other, &this] {
            f.answers_like_a_scan(bin);
        }
    }
    std::thread::sleep(SETTLE);
    assert_eq!(f.builds(), built, "a switch rebuilt or removed an index");

    // an edit reaches both, without a rebuild
    let lib = f.root.join("src/lib.rs");
    let mut body = fs::read_to_string(&lib).unwrap();
    body.push_str("pub fn needle_two() -> u32 {\n    needle()\n}\n");
    fs::write(&lib, body).unwrap();
    for bin in [&other, &this, &other, &this] {
        f.answers_like_a_scan(bin);
    }
    std::thread::sleep(SETTLE);
    assert_eq!(f.builds(), built, "an edit rebuilt or removed an index");

    // both at once
    let expected: Vec<Vec<u8>> = [&other, &this]
        .iter()
        .map(|b| f.run(b, &["-c", "needle"]).stdout)
        .collect();
    std::thread::scope(|s| {
        let runs: Vec<_> = (0..8)
            .map(|i| {
                let (bin, want) = if i % 2 == 0 {
                    (&other, &expected[0])
                } else {
                    (&this, &expected[1])
                };
                let f = &f;
                s.spawn(move || {
                    let o = f.run(bin, &["-c", "needle"]);
                    assert_eq!(o.status.code(), Some(0), "{bin:?}: {o:?}");
                    assert_eq!(&o.stdout, want, "{bin:?}");
                })
            })
            .collect();
        for r in runs {
            r.join().unwrap();
        }
    });
    std::thread::sleep(SETTLE);
    assert_eq!(
        f.builds(),
        built,
        "concurrent runs rebuilt or removed an index"
    );
}
