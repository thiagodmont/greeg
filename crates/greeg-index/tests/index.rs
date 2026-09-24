//! Build → open → delta → tombstone roundtrips, freshness under stat mode,
//! ignore-file edits, corrupt deltas and writer races, on a temp tree.

use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Mode};
use greeg_index::{Index, plan, read_manifest};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

const HDR: usize = greeg_index::format::HEADER_LEN;

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

static N: AtomicU32 = AtomicU32::new(0);

fn tree() -> Tmp {
    let base = std::env::temp_dir().join(format!(
        "greeg-index-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join("tree");
    let dir = base.join("index");
    fs::create_dir_all(root.join("src/util")).unwrap();
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    // `.gitignore` is honoured only inside a git repository (ripgrep's require_git)
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    helper_alpha();\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/util/helper.rs"),
        "pub fn helper_alpha() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("lib/thing.py"),
        "def thing_beta():\n    return 'beta'\n",
    )
    .unwrap();
    fs::write(root.join("build/out.txt"), "generated_gamma\n").unwrap();
    fs::write(root.join("README.md"), "readme_delta\n").unwrap();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();
    Tmp { root, dir, base }
}

fn opts() -> BuildOpts {
    BuildOpts {
        reader_threads: 2,
        quiet: true,
        phase1_only: false,
        ..Default::default()
    }
}

/// A tree big enough that a small budget forces several spills per worker.
fn wide_tree(files: usize) -> Tmp {
    let base = std::env::temp_dir().join(format!(
        "greeg-index-wide-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join("tree");
    let dir = base.join("index");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    // deterministic, varied content: the point is many distinct grams and
    // words spread over many files, so the postings are wide
    let mut x: u64 = 987654321;
    for i in 0..files {
        let mut body = String::with_capacity(2048);
        body.push_str(&format!("pub fn f_{i}() -> u32 {{\n"));
        for _ in 0..24 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            body.push_str(&format!(
                "    let v_{a}_{b} = call_{c}(sym_{a}, \"lit_{b}\");\n",
                a = (x >> 33) % 211,
                b = (x >> 17) % 307,
                c = (x >> 5) % 97
            ));
        }
        body.push_str("    0\n}\n");
        fs::write(root.join(format!("src/f{i}.rs")), body).unwrap();
    }
    Tmp { root, dir, base }
}

/// The whole point of the byte budget: a build that spills must publish the
/// same index as one that does not. Byte-for-byte, so this also pins the
/// merge's key order and its posting deduplication.
#[test]
fn a_spilled_build_publishes_the_same_index_as_an_in_memory_one() {
    let t = wide_tree(400);
    let mem_dir = t.base.join("index-mem");
    let ext_dir = t.base.join("index-ext");
    build(
        &t.root,
        &mem_dir,
        &BuildOpts {
            reader_threads: 2,
            quiet: true,
            phase1_only: true,
            posting_budget: 1 << 30,
        },
    )
    .unwrap();
    build(
        &t.root,
        &ext_dir,
        &BuildOpts {
            reader_threads: 2,
            quiet: true,
            phase1_only: true,
            // below the 64 KiB per-chunk floor, so every chunk spills
            posting_budget: 1,
        },
    )
    .unwrap();
    assert!(
        !read_manifest(&mem_dir).unwrap().spilled,
        "the large budget must stay in memory"
    );
    assert!(
        read_manifest(&ext_dir).unwrap().spilled,
        "the tiny budget must spill, or this test proves nothing"
    );
    for c in ["grams", "words", "files"] {
        let a = fs::read(mem_dir.join(format!("{c}.1.bin"))).unwrap();
        let b = fs::read(ext_dir.join(format!("{c}.1.bin"))).unwrap();
        assert_eq!(a.len(), b.len(), "{c}.bin length");
        assert!(
            a == b,
            "{c}.bin differs between the spilled and in-memory builds"
        );
    }
    // and the scratch directory is gone, whichever path ran
    for d in [&mem_dir, &ext_dir] {
        let leftover: Vec<_> = fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("build-tmp"))
            .collect();
        assert!(leftover.is_empty(), "{d:?} kept {leftover:?}");
    }
}

/// A search over a spilled index must answer exactly as one over an
/// in-memory one: the candidate sets come from the same postings.
#[test]
fn a_spilled_index_answers_the_same_candidates() {
    let t = wide_tree(300);
    let ext_dir = t.base.join("index-ext");
    build(
        &t.root,
        &t.dir,
        &BuildOpts {
            reader_threads: 2,
            quiet: true,
            phase1_only: true,
            posting_budget: 1 << 30,
        },
    )
    .unwrap();
    build(
        &t.root,
        &ext_dir,
        &BuildOpts {
            reader_threads: 2,
            quiet: true,
            phase1_only: true,
            posting_budget: 1,
        },
    )
    .unwrap();
    assert!(read_manifest(&ext_dir).unwrap().spilled);
    let a = Index::open(&t.dir).unwrap();
    let b = Index::open(&ext_dir).unwrap();
    for pat in ["call_17", "sym_42", "lit_100", "f_7", "pub fn"] {
        assert_eq!(ids_for(&a, pat), ids_for(&b, pat), "candidates for {pat:?}");
    }
}

fn ids_for(idx: &Index, pat: &str) -> Vec<String> {
    let q = plan::plan(pat, true, false).unwrap();
    let mut v: Vec<String> = idx
        .candidates(&q)
        .iter()
        .map(|id| idx.path(id).unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn live_paths(idx: &Index) -> Vec<String> {
    let mut v: Vec<String> = idx.live_files().map(|(_, r, _)| r.to_string()).collect();
    v.sort();
    v
}

fn check(idx: &Index, root: &Path) -> fresh::Changes {
    fresh::check(idx, root, Mode::Stat, 2).unwrap()
}

#[test]
fn build_open_delta_tombstone_roundtrip() {
    let t = tree();
    let m = build(&t.root, &t.dir, &opts()).unwrap();
    assert!(m.phase1 && m.phase2);
    assert!(t.dir.join("LOCK").exists());
    let idx = Index::open(&t.dir).unwrap();
    // the ignore file is tracked but never searchable; build/ is ignored
    assert_eq!(
        live_paths(&idx),
        [
            "README.md",
            "lib/thing.py",
            "src/main.rs",
            "src/util/helper.rs"
        ]
    );
    assert!(idx.tracked_files().any(|(_, r, _)| r == ".gitignore"));
    assert_eq!(idx.live_count(), 4);
    assert_eq!(
        ids_for(&idx, "helper_alpha"),
        ["src/main.rs", "src/util/helper.rs"]
    );
    assert!(
        ids_for(&idx, "build/").is_empty(),
        ".gitignore content must not be searchable"
    );
    assert!(check(&idx, &t.root).is_empty());

    // modify: the old id is tombstoned, the new version lives in a delta
    fs::write(
        t.root.join("src/util/helper.rs"),
        "pub fn helper_omega() -> u32 {\n    43\n}\n",
    )
    .unwrap();
    let ch = check(&idx, &t.root);
    assert_eq!(ch.modified.len(), 1);
    assert_eq!(fresh::apply(&idx, &t.root, &ch).unwrap(), 1);
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.manifest.deltas, 1);
    assert_eq!(idx.deltas.len(), 1);
    assert_eq!(idx.tomb.len(), 1);
    assert_eq!(ids_for(&idx, "helper_alpha"), ["src/main.rs"]);
    assert_eq!(ids_for(&idx, "helper_omega"), ["src/util/helper.rs"]);
    assert_eq!(idx.live_count(), 4);
    assert!(
        !idx.lookup("helper_omega").is_empty(),
        "delta symbols are visible"
    );
    assert!(
        idx.lookup("helper_alpha").is_empty(),
        "tombstoned symbols are not"
    );
    assert!(check(&idx, &t.root).is_empty());

    // a stray delta file that the manifest does not name is ignored
    fs::write(t.dir.join("delta/0002.bin"), b"garbage").unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.deltas.len(), 1);

    // a rebuild starts a new generation and drops the deltas
    let m2 = build(&t.root, &t.dir, &opts()).unwrap();
    assert_eq!(m2.generation, m.generation + 1);
    assert!(!t.dir.join("delta").exists());
    assert!(!t.dir.join(format!("files.{}.bin", m.generation)).exists());
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.deltas.len(), 0);
    assert_eq!(ids_for(&idx, "helper_omega"), ["src/util/helper.rs"]);
}

#[test]
fn stat_mode_add_delete_rename() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();

    // add a file and a directory tree
    fs::write(t.root.join("src/extra.rs"), "fn extra_epsilon() {}\n").unwrap();
    fs::create_dir_all(t.root.join("newdir/deep")).unwrap();
    fs::write(t.root.join("newdir/deep/z.py"), "zeta_zeta = 1\n").unwrap();
    let ch = check(&idx, &t.root);
    let mut added: Vec<&str> = ch.added.iter().map(|w| w.rel.as_str()).collect();
    added.sort();
    assert_eq!(added, ["newdir/deep/z.py", "src/extra.rs"]);
    assert_eq!(ch.added_dirs.len(), 2);
    assert!(ch.deleted.is_empty() && ch.modified.is_empty());
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(ids_for(&idx, "zeta_zeta"), ["newdir/deep/z.py"]);
    assert_eq!(ids_for(&idx, "extra_epsilon"), ["src/extra.rs"]);
    assert!(
        check(&idx, &t.root).is_empty(),
        "new dirs are recorded with their mtime"
    );

    // delete a file
    fs::remove_file(t.root.join("lib/thing.py")).unwrap();
    let ch = check(&idx, &t.root);
    assert_eq!(ch.deleted.len(), 1);
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert!(ids_for(&idx, "thing_beta").is_empty());
    assert!(!live_paths(&idx).contains(&"lib/thing.py".to_string()));

    // rename a file: one deletion, one addition
    fs::rename(t.root.join("src/extra.rs"), t.root.join("src/moved.rs")).unwrap();
    let ch = check(&idx, &t.root);
    assert_eq!(ch.deleted.len(), 1);
    assert_eq!(
        ch.added.iter().map(|w| w.rel.as_str()).collect::<Vec<_>>(),
        ["src/moved.rs"]
    );
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(ids_for(&idx, "extra_epsilon"), ["src/moved.rs"]);

    // rename a directory: its files are deleted and re-added under the new name
    fs::rename(t.root.join("newdir"), t.root.join("renamed")).unwrap();
    let ch = check(&idx, &t.root);
    assert_eq!(ch.deleted.len(), 1);
    assert_eq!(
        ch.added.iter().map(|w| w.rel.as_str()).collect::<Vec<_>>(),
        ["renamed/deep/z.py"]
    );
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(ids_for(&idx, "zeta_zeta"), ["renamed/deep/z.py"]);
    assert_eq!(
        live_paths(&idx),
        [
            "README.md",
            "renamed/deep/z.py",
            "src/main.rs",
            "src/moved.rs",
            "src/util/helper.rs"
        ]
    );
    assert_eq!(idx.live_count(), 5);

    // delete a directory tree
    fs::remove_dir_all(t.root.join("renamed")).unwrap();
    let ch = check(&idx, &t.root);
    assert_eq!(ch.deleted.len(), 1);
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert!(ids_for(&idx, "zeta_zeta").is_empty());
    assert!(check(&idx, &t.root).is_empty());
}

#[test]
fn ignore_file_edits_force_rebuild() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();

    // modify: ignore lib/ too
    fs::write(t.root.join(".gitignore"), "build/\nlib/\n").unwrap();
    let ch = check(&idx, &t.root);
    assert!(
        ch.ignore_changed,
        "edited .gitignore must be detected: {ch:?}"
    );
    assert!(fresh::needs_rebuild(&idx, &ch));
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(
        live_paths(&idx),
        ["README.md", "src/main.rs", "src/util/helper.rs"]
    );
    assert!(check(&idx, &t.root).is_empty());

    // add a nested ignore file
    fs::write(t.root.join("src/.ignore"), "util/\n").unwrap();
    let ch = check(&idx, &t.root);
    assert!(
        ch.ignore_changed,
        "added src/.ignore must be detected: {ch:?}"
    );
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(live_paths(&idx), ["README.md", "src/main.rs"]);
    assert!(idx.tracked_files().any(|(_, r, _)| r == "src/.ignore"));

    // delete it again
    fs::remove_file(t.root.join("src/.ignore")).unwrap();
    let ch = check(&idx, &t.root);
    assert!(
        ch.ignore_changed,
        "deleted src/.ignore must be detected: {ch:?}"
    );
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(
        live_paths(&idx),
        ["README.md", "src/main.rs", "src/util/helper.rs"]
    );
}

#[test]
fn huge_files_stay_candidates() {
    let t = tree();
    let big = vec![b'x'; (greeg_index::build::MAX_FILE + 1) as usize];
    fs::write(t.root.join("src/blob.rs"), &big).unwrap();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let rec = idx
        .live_files()
        .find(|(_, r, _)| *r == "src/blob.rs")
        .map(|(_, _, rec)| *rec)
        .unwrap();
    assert!(greeg_lang::FileFlags(rec.flags).has(greeg_lang::FileFlags::HUGE));
    assert_eq!(
        ids_for(&idx, "helper_alpha"),
        ["src/blob.rs", "src/main.rs", "src/util/helper.rs"]
    );
    assert_eq!(ids_for(&idx, "no_such_gram_anywhere"), ["src/blob.rs"]);
    // and through a delta
    fs::write(t.root.join("src/blob.rs"), &big[..big.len() - 1]).unwrap();
    fs::write(t.root.join("lib/big.py"), &big).unwrap();
    let ch = check(&idx, &t.root);
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(ids_for(&idx, "no_such_gram_anywhere"), ["lib/big.py"]);
}

#[test]
fn truncated_delta_fails_open() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    fs::write(
        t.root.join("src/main.rs"),
        "fn main() { helper_alpha(); helper_alpha(); }\n",
    )
    .unwrap();
    let ch = check(&idx, &t.root);
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let p = t.dir.join("delta/0001.bin");
    let good = fs::read(&p).unwrap();
    assert!(Index::open(&t.dir).is_ok());

    // section length past the end of the payload
    let mut bad = good.clone();
    bad[HDR + 8..HDR + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&p, &bad).unwrap();
    assert!(
        Index::open(&t.dir).is_err(),
        "oversized files section must fail open"
    );

    // a section length that overflows into the tombstone bitmap
    let mut bad = good.clone();
    let gl = u32::from_le_bytes(good[HDR + 12..HDR + 16].try_into().unwrap());
    bad[HDR + 12..HDR + 16].copy_from_slice(&(gl + 8).to_le_bytes());
    fs::write(&p, &bad).unwrap();
    assert!(
        Index::open(&t.dir).is_err(),
        "corrupt tombstone bitmap must fail open"
    );

    // physically truncated file
    fs::write(&p, &good[..good.len() / 2]).unwrap();
    assert!(Index::open(&t.dir).is_err());

    // missing file named by the manifest
    fs::remove_file(&p).unwrap();
    assert!(Index::open(&t.dir).is_err());

    fs::write(&p, &good).unwrap();
    assert!(Index::open(&t.dir).is_ok());
}

#[test]
fn case_insensitive_long_s_stays_candidate() {
    let t = tree();
    fs::write(t.root.join("src/sess.rs"), "fn getU\u{17F}erSession() {}\n").unwrap();
    fs::write(t.root.join("src/kelvin.rs"), "let \u{212A}ilogram = 1;\n").unwrap();
    fs::write(
        t.root.join("src/plain.rs"),
        "fn getUserSession() {}\nlet kilogram = 1;\n",
    )
    .unwrap();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let cands = |pat: &str| -> Vec<String> {
        let q = plan::plan(pat, false, true).unwrap();
        assert_ne!(q, plan::Q::All, "{pat}: -i must keep grams through s/k");
        let mut v: Vec<String> = idx
            .candidates(&q)
            .iter()
            .map(|id| idx.path(id).unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(cands("getUserSession"), ["src/plain.rs", "src/sess.rs"]);
    assert_eq!(cands("kilogram"), ["src/kelvin.rs", "src/plain.rs"]);
    assert_eq!(cands("getUserSessionX"), Vec::<String>::new());
}

#[test]
fn apply_skips_when_manifest_moved() {
    let t = tree();
    let m1 = build(&t.root, &t.dir, &opts()).unwrap();
    let a = Index::open(&t.dir).unwrap();
    let b = Index::open(&t.dir).unwrap();

    // two queries computed the same change against the same generation
    fs::write(
        t.root.join("src/main.rs"),
        "fn main() { helper_alpha(); helper_alpha(); }\n",
    )
    .unwrap();
    let ch_a = check(&a, &t.root);
    let ch_b = check(&b, &t.root);
    assert_eq!(fresh::apply(&a, &t.root, &ch_a).unwrap(), 1);
    assert_eq!(
        fresh::apply(&b, &t.root, &ch_b).unwrap(),
        0,
        "second writer must skip: the delta count moved"
    );
    let m = read_manifest(&t.dir).unwrap();
    assert_eq!(m.deltas, 1);
    assert!(!t.dir.join("delta/0002.bin").exists());
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.live_count(), 4);

    // a build published a new generation between open and apply
    let stale = Index::open(&t.dir).unwrap();
    fs::write(
        t.root.join("lib/thing.py"),
        "def thing_beta():\n    return 'beta beta'\n",
    )
    .unwrap();
    let ch = check(&stale, &t.root);
    assert_eq!(ch.modified.len(), 1);
    let m2 = build(&t.root, &t.dir, &opts()).unwrap();
    assert_eq!(m2.generation, m1.generation + 1);
    assert_eq!(fresh::apply(&stale, &t.root, &ch).unwrap(), 0);
    let m = read_manifest(&t.dir).unwrap();
    assert_eq!((m.generation, m.deltas), (m2.generation, 0));
    assert!(!t.dir.join("delta").exists());
    assert!(m.phase2, "a stale writer must not revert the phase");
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(ids_for(&idx, "beta beta"), ["lib/thing.py"]);
    assert!(check(&idx, &t.root).is_empty());

    // an empty check refreshes the stamp only once per second and never
    // against a manifest it did not open
    let stale = Index::open(&t.dir).unwrap();
    build(&t.root, &t.dir, &opts()).unwrap();
    let before = read_manifest(&t.dir).unwrap();
    let empty = fresh::Changes {
        fsevents_id: 7,
        ..Default::default()
    };
    assert_eq!(fresh::apply(&stale, &t.root, &empty).unwrap(), 0);
    let after = read_manifest(&t.dir).unwrap();
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.verified_unix_ms, before.verified_unix_ms);
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
    let base = std::env::temp_dir().join(format!(
        "greeg-index-fingerprint-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
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
    let one = BuildOpts {
        reader_threads: 1,
        ..opts()
    };
    build(&t.root, &t.dir, &one).unwrap();
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
/// to what a build or refresh writes needs a new `FORMAT_VERSION`. Each
/// version's digest of the fixture is recorded here.
#[test]
fn layout_fingerprint_matches_format_version() {
    const LAYOUTS: &[(u32, &str)] = &[(6, "c0ff84dcd8ad9d18")];
    let t = fingerprint_tree();
    let digest = layout_digest(&t);
    assert_eq!(
        digest,
        layout_digest(&t),
        "the fixture build is not reproducible"
    );
    let v = greeg_index::FORMAT_VERSION;
    match LAYOUTS.iter().find(|(n, _)| *n == v) {
        Some((_, recorded)) => assert_eq!(
            *recorded,
            digest,
            "what a build or refresh writes changed: if the layout changed, bump \
             FORMAT_VERSION and record ({}, \"{digest}\"); if only extracted \
             content changed within the same layout, re-record ({v}, \"{digest}\")",
            v + 1
        ),
        None => panic!("record ({v}, \"{digest}\") in LAYOUTS"),
    }
    assert!(
        LAYOUTS.iter().all(|(n, d)| *n == v || *d != digest),
        "this layout was already recorded under another version"
    );
}
