//! The on-disk layout pinned to `FORMAT_VERSION`. A separate test binary:
//! it sets `GREEG_DEBUG_FIXED_STAMPS` for the whole process, so records hold
//! no change time, inode or device and the digest is the same on every machine.

use greeg_index::Index;
use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Mode};
use std::fs;
use std::path::{Path, PathBuf};

struct Tmp {
    root: PathBuf,
    dir: PathBuf,
    base: PathBuf,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Pinned, so no environment setting (`GREEG_BUILD_BUDGET_MB`) changes the
/// build's output.
fn opts() -> BuildOpts {
    BuildOpts {
        reader_threads: 1,
        quiet: true,
        phase1_only: false,
        posting_budget: 256 << 20,
    }
}

fn check(idx: &Index, root: &Path) -> fresh::Changes {
    fresh::check(idx, root, Mode::Stat, 2).unwrap()
}

/// Set every file's and then every directory's modification time, so two
/// builds of the tree write the same bytes.
fn pin_times(dir: &Path) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            pin_times(&p);
        } else {
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }
    fs::File::open(dir).unwrap().set_modified(t).unwrap();
}

/// The fingerprint's own tree, separate from the fixtures other tests edit.
/// Changing it changes every digest recorded below.
fn fingerprint_tree() -> Tmp {
    // created here, so no file left by another run joins the fixture
    let base = (0..)
        .map(|n| {
            std::env::temp_dir().join(format!(
                "greeg-index-fingerprint-{}-{n}",
                std::process::id()
            ))
        })
        .find(|d| match fs::create_dir(d) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => panic!("create {}: {e}", d.display()),
        })
        .unwrap();
    let root = base.join("tree");
    let dir = base.join("index");
    fs::create_dir_all(root.join(".git")).unwrap();
    for (name, body) in [
        (
            "src/lib.rs",
            "pub mod util;\npub fn alpha() -> u32 {\n    util::beta()\n}\n",
        ),
        ("src/util.rs", "pub fn beta() -> u32 {\n    1 // beta\n}\n"),
        (
            "tools/run.py",
            "def gamma():\n    \"\"\"gamma\"\"\"\n    return 1\n",
        ),
        ("notes.txt", "plain delta text\n"),
        (".hidden.txt", "hidden\n"),
        ("out/gen.txt", "ignored\n"),
        (".gitignore", "out/\n"),
    ] {
        let p = root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    Tmp { root, dir, base }
}

/// Every file a build and then a refresh with one edit write, and the
/// manifest without its machine- and time-dependent values.
fn layout_digest(t: &Tmp) -> String {
    let _ = fs::remove_dir_all(&t.dir);
    fs::write(
        t.root.join("src/util.rs"),
        "pub fn beta() -> u32 {\n    1 // beta\n}\n",
    )
    .unwrap();
    pin_times(&t.root);
    build(&t.root, &t.dir, &opts()).unwrap();
    fs::write(
        t.root.join("src/util.rs"),
        "pub fn beta() -> u32 {\n    2 // beta, edited\n}\n",
    )
    .unwrap();
    pin_times(&t.root);
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(
        fresh::apply(&idx, &t.root, &check(&idx, &t.root)).unwrap(),
        1
    );
    drop(idx);
    let mut files = Vec::new();
    let mut stack = vec![t.dir.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p.strip_prefix(&t.dir).unwrap().to_path_buf());
            }
        }
    }
    files.sort();
    let mut h = blake3::Hasher::new();
    for f in &files {
        let name = f.to_str().unwrap();
        if matches!(name, "manifest" | "LOCK" | "OWNER") {
            continue;
        }
        h.update(name.as_bytes());
        h.update(&fs::read(t.dir.join(f)).unwrap());
    }
    let mut m: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&fs::read(t.dir.join("manifest")).unwrap()).unwrap();
    for volatile in [
        "root",
        "root_id",
        "built_unix_ms",
        "build_ms",
        "phase2_ms",
        "peak_rss",
        "fsevents_id",
        "verified_unix_ms",
        "ignore_inputs",
    ] {
        assert!(m.remove(volatile).is_some(), "manifest has no {volatile}");
    }
    let mut fields: Vec<_> = m.into_iter().collect();
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in fields {
        h.update(k.as_bytes());
        h.update(v.to_string().as_bytes());
    }
    h.finalize().to_hex()[..16].to_string()
}

/// Binaries of two layouts must never share an index directory, so a change
/// to what a build or refresh writes needs a new `FORMAT_VERSION`. The
/// current version's digest of the fixture is recorded here; every component
/// header holds the version, so digests of two versions never match.
#[test]
fn layout_fingerprint_matches_format_version() {
    // SAFETY: the only test in this binary, and no thread has started yet.
    unsafe { std::env::set_var("GREEG_DEBUG_FIXED_STAMPS", "1") };
    const LAYOUT: (u32, &str) = (7, "f896972b698270a7");
    let t = fingerprint_tree();
    let digest = layout_digest(&t);
    assert_eq!(
        digest,
        layout_digest(&t),
        "the fixture build is not reproducible"
    );
    let v = greeg_index::FORMAT_VERSION;
    assert_eq!(LAYOUT.0, v, "record ({v}, \"{digest}\") as LAYOUT");
    assert_eq!(
        LAYOUT.1,
        digest,
        "what a build or refresh writes changed: if the layout changed, bump \
         FORMAT_VERSION and record ({}, \"...\"); if only extracted content \
         changed within the same layout, re-record ({v}, \"{digest}\")",
        v + 1
    );
}
