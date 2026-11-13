//! Build → open → delta → tombstone roundtrips, freshness under stat mode,
//! ignore-file edits, corrupt deltas and writer races, on a temp tree.

use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::RebuildReason;
use greeg_index::fresh::{self, Mode};
use greeg_index::{Index, format, plan, read_manifest};
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

/// An empty tree in a directory this run creates itself, so `Drop` never
/// removes one left behind by another run.
fn empty_tmp() -> Tmp {
    let base = loop {
        let base = std::env::temp_dir().join(format!(
            "greeg-index-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&base) {
            Ok(()) => break base,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("create fixture: {e}"),
        }
    };
    Tmp {
        root: base.join("tree"),
        dir: base.join("index"),
        base,
    }
}

/// Wait out the racy window of what was just written under `root`, so a
/// build lists its directories late enough to trust their stamps.
fn settle(root: &Path) {
    use std::os::unix::fs::MetadataExt;
    let whole = fs::metadata(root).unwrap().ctime_nsec() == 0;
    std::thread::sleep(std::time::Duration::from_millis(if whole {
        2100
    } else {
        50
    }));
}

fn tree() -> Tmp {
    let t = empty_tmp();
    let root = &t.root;
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
    settle(root);
    t
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
    let t = empty_tmp();
    let root = &t.root;
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
    settle(root);
    t
}

/// The whole point of the byte budget: a build that spills must publish the
/// same index as one that does not. Byte-for-byte, so this also pins the
/// merge's key order and its posting deduplication.
/// Many distinct words and few grams: the words reach the budget and spill
/// while the grams stay in memory, and the manifest says the build spilled.
#[test]
fn a_build_whose_words_spill_says_so() {
    let t = empty_tmp();
    fs::create_dir_all(&t.root).unwrap();
    let mut x: u64 = 42;
    for i in 0..20 {
        let mut body = String::with_capacity(45_000);
        for _ in 0..5000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            for k in 0..8 {
                body.push((b'a' + ((x >> (8 + 5 * k)) % 26) as u8) as char);
            }
            body.push(' ');
        }
        fs::write(t.root.join(format!("f{i:03}.txt")), body).unwrap();
    }
    settle(&t.root);
    let m = build(
        &t.root,
        &t.dir,
        &BuildOpts {
            reader_threads: 1,
            quiet: true,
            phase1_only: true,
            // 1.5 MiB per chunk of 5 files: about 1 MiB of grams, 2 MiB of
            // words (25,000 new ones)
            posting_budget: 6 << 20,
            block: greeg_index::integrity::BLOCK,
        },
    )
    .unwrap();
    assert!(m.spilled, "the words spilled");
}

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
            block: greeg_index::integrity::BLOCK,
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
            block: greeg_index::integrity::BLOCK,
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
    // the payloads: the headers name two different builds
    let (mm, em) = (
        read_manifest(&mem_dir).unwrap(),
        read_manifest(&ext_dir).unwrap(),
    );
    for c in [format::COMP_GRAMS, format::COMP_WORDS, format::COMP_FILES] {
        let a = fs::read(mem_dir.join(mm.component(c).unwrap().0)).unwrap();
        let b = fs::read(ext_dir.join(em.component(c).unwrap().0)).unwrap();
        assert_eq!(a.len(), b.len(), "component {c} length");
        assert!(
            a[HDR..] == b[HDR..],
            "component {c} differs between the spilled and in-memory builds"
        );
    }
    // and the scratch directory is gone, whichever path ran
    for d in [&mem_dir, &ext_dir] {
        let leftover: Vec<_> = fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("scratch-"))
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
            block: greeg_index::integrity::BLOCK,
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
            block: greeg_index::integrity::BLOCK,
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
        .map(|id| text(idx.path(id).unwrap()).to_string())
        .collect();
    v.sort();
    v
}

fn live_paths(idx: &Index) -> Vec<String> {
    let mut v: Vec<String> = idx
        .live_files()
        .map(|(_, r, _)| text(r).to_string())
        .collect();
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
    assert!(idx.tracked_files().any(|(_, r, _)| r == b".gitignore"));
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
    fs::write(t.dir.join(idx.manifest.delta(2)), b"garbage").unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.deltas.len(), 1);

    // a rebuild starts a new build directory and retires the old one, deltas
    // included, until its grace period ends
    let m2 = build(&t.root, &t.dir, &opts()).unwrap();
    assert_eq!(m2.generation, m.generation + 1);
    assert_ne!(m2.epoch, m.epoch);
    let old = greeg_index::snapshot::gen_dir(m.epoch);
    assert!(m2.retired.iter().any(|r| r.name == old));
    assert!(t.dir.join(&old).exists());
    assert!(greeg_index::snapshot::expire_retired(&t.dir) > 0);
    assert!(!t.dir.join(&old).exists());
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
    // listed late enough to trust: recorded as they are, not to list again
    settle(&t.root);
    let ch = check(&idx, &t.root);
    let mut added: Vec<&str> = ch.added.iter().map(|w| text(&w.rel)).collect();
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
        ch.added.iter().map(|w| text(&w.rel)).collect::<Vec<_>>(),
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
        ch.added.iter().map(|w| text(&w.rel)).collect::<Vec<_>>(),
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
    settle(&t.root);
    let ch = check(&idx, &t.root);
    assert_eq!(ch.deleted.len(), 1);
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert!(ids_for(&idx, "zeta_zeta").is_empty());
    assert!(check(&idx, &t.root).is_empty());
}

/// A file import resolution reads by name, added, edited or deleted, moves
/// edges of files that did not change: only a rebuild resolves them again.
#[test]
fn resolver_file_edits_force_rebuild() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(t.root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let ch = check(&idx, &t.root);
    assert_eq!(ch.rebuild, Some(RebuildReason::ResolverInputs), "{ch:?}");
    assert_eq!(
        fresh::rebuild_reason(&idx, &ch),
        Some(RebuildReason::ResolverInputs)
    );
    // a file of another name is a delta
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(t.root.join("README.md"), "readme_epsilon\n").unwrap();
    assert_eq!(check(&idx, &t.root).rebuild, None);
}

/// An index derived by another version or language registry is rebuilt.
#[test]
fn another_derivation_forces_rebuild() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let mut m = read_manifest(&t.dir).unwrap();
    assert_eq!(m.derivation, greeg_index::derive::derivation());
    assert_eq!(m.languages[..2], ["builtin:none", "builtin:python"]);
    m.derivation = "an older release".into();
    greeg_index::write_manifest(&t.dir, &m).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(
        check(&idx, &t.root).rebuild,
        Some(RebuildReason::Derivation)
    );
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
        ch.rebuild == Some(RebuildReason::IgnoreInputs),
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
        ch.rebuild == Some(RebuildReason::IgnoreInputs),
        "added src/.ignore must be detected: {ch:?}"
    );
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(live_paths(&idx), ["README.md", "src/main.rs"]);
    assert!(idx.tracked_files().any(|(_, r, _)| r == b"src/.ignore"));

    // delete it again
    fs::remove_file(t.root.join("src/.ignore")).unwrap();
    let ch = check(&idx, &t.root);
    assert!(
        ch.rebuild == Some(RebuildReason::IgnoreInputs),
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
        .find(|(_, r, _)| *r == b"src/blob.rs")
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
    let p = t.dir.join(idx.manifest.delta(1));
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
            .map(|id| text(idx.path(id).unwrap()).to_string())
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
    assert!(!t.dir.join(m.delta(2)).exists());
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
    assert!(!t.dir.join(m.delta(1)).exists());
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

fn text(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap()
}

/// A name match that ends one path and runs into the next one in the arena
/// (`a/foo`, then `bar.rs`) is no module file, and does not panic.
#[test]
fn module_lookup_ignores_a_match_across_two_paths() {
    let t = empty_tmp();
    fs::create_dir_all(t.root.join("a")).unwrap();
    fs::create_dir_all(t.root.join("src")).unwrap();
    fs::write(t.root.join("a/foo"), "x\n").unwrap();
    fs::write(t.root.join("bar.rs"), "fn z() {}\n").unwrap();
    fs::write(t.root.join("src/foo.rs"), "fn y() {}\n").unwrap();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert!(idx.module_files("foobar", 10).is_empty());
    assert!(idx.module_files_folded("FOOBAR", 10).is_empty());
    let found: Vec<_> = idx
        .module_files("foo", 10)
        .into_iter()
        .filter_map(|id| idx.path(id))
        .collect();
    assert_eq!(found, [b"src/foo.rs".as_slice()]);
}

/// A name that overlaps itself: the hit straddling `a/xa` and `aa.rs` must
/// not hide the real one starting a byte later.
#[test]
fn module_lookup_finds_a_name_overlapping_a_straddling_hit() {
    let t = empty_tmp();
    fs::create_dir_all(t.root.join("a")).unwrap();
    fs::write(t.root.join("a/xa"), "x\n").unwrap();
    fs::write(t.root.join("aa.rs"), "fn z() {}\n").unwrap();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    for found in [
        idx.module_files("aa", 10),
        idx.module_files_folded("aa", 10),
    ] {
        let found: Vec<_> = found.into_iter().filter_map(|id| idx.path(id)).collect();
        assert_eq!(found, [b"aa.rs".as_slice()]);
    }
}

/// A verification stamp from the future (the clock stepped back) is no reason
/// to skip the check, and a check that finds nothing stamps the present.
#[test]
fn a_stamp_from_the_future_is_not_trusted() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let future = |dir: &Path| {
        let mut m = read_manifest(dir).unwrap();
        m.verified_unix_ms = greeg_index::now_ms() + 3_600_000;
        greeg_index::write_manifest(dir, &m).unwrap();
    };
    future(&t.dir);
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::Auto, 2).expect("an Auto check ran");
    assert!(ch.is_empty(), "{:?}", ch.count());
    fresh::apply(&idx, &t.root, &ch).unwrap();
    drop(idx);
    assert!(read_manifest(&t.dir).unwrap().verified_unix_ms <= greeg_index::now_ms());

    future(&t.dir);
    fs::write(
        t.root.join("src/main.rs"),
        "fn main() {\n    helper_alpha();\n    helper_alpha();\n}\n",
    )
    .unwrap();
    fs::write(t.root.join("src/new.rs"), "pub fn fresh_omega() {}\n").unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::Auto, 2).expect("an Auto check ran");
    assert_eq!(ch.modified.len(), 1);
    assert_eq!(ch.added.len(), 1);
}

/// A directory whose stamp changed is listed again, and once a listing
/// comes late enough for its stamp to be trusted, no more. An mtime set
/// ahead of the clock does not keep it listed.
#[test]
fn a_directory_is_listed_again_until_its_stamp_is_trusted() {
    use std::time::{Duration, SystemTime};
    let t = tree();
    let lib = t.root.join("lib");
    let set_mtime = |at: SystemTime| fs::File::open(&lib).unwrap().set_modified(at).unwrap();
    settle(&lib);
    build(&t.root, &t.dir, &opts()).unwrap();
    let relisted = || -> Vec<String> {
        let idx = Index::open(&t.dir).unwrap();
        let ch = check(&idx, &t.root);
        fresh::apply(&idx, &t.root, &ch).unwrap();
        ch.touched_dirs
            .iter()
            .map(|d| String::from_utf8_lossy(&d.rel).into_owned())
            .collect()
    };
    for at in [
        SystemTime::now() + Duration::from_secs(10),
        SystemTime::now() - Duration::from_secs(10),
    ] {
        set_mtime(at);
        settle(&lib);
        assert_eq!(relisted(), ["lib"], "its stamp changed");
        assert!(relisted().is_empty(), "listed late enough: trusted");
    }
}

/// Files a check cannot stat for a reason other than "not found" are not
/// deleted: the check says it is unreliable, and applying it writes nothing.
#[test]
fn a_stat_error_other_than_not_found_is_no_deletion() {
    use std::os::unix::fs::PermissionsExt;
    let t = tree();
    let locked = t.root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("inner.txt"), "needle\n").unwrap();
    settle(&t.root);
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let before = greeg_index::read_manifest(&t.dir).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: this user reads a mode-000 directory (root)");
        return;
    }
    // past the second within which `apply` leaves the stamp as it is anyway
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let ch = check(&idx, &t.root);
    let applied = fresh::apply(&idx, &t.root, &ch);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ch.unreliable, "{ch:?}");
    assert!(ch.deleted.is_empty(), "{ch:?}");
    assert_eq!(applied.unwrap(), 0);
    let after = greeg_index::read_manifest(&t.dir).unwrap();
    assert_eq!(after.snapshot(), before.snapshot());
    assert_eq!(after.verified_unix_ms, before.verified_unix_ms);
}

/// A file and a directory stamped a day ahead (`touch -t`, an unpacked
/// archive) are trusted once their ctime is: no check re-extracts them, and
/// an edit is still seen.
#[test]
fn a_stamp_set_in_the_future_is_no_change_on_every_check() {
    let t = tree();
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(86_400);
    let file = t.root.join("src/ahead.rs");
    fs::write(&file, "fn ahead() {}\n").unwrap();
    for p in [&file, &t.root.join("lib")] {
        fs::File::open(p).unwrap().set_modified(ahead).unwrap();
    }
    settle(&t.root);
    build(&t.root, &t.dir, &opts()).unwrap();
    for _ in 0..3 {
        let idx = Index::open(&t.dir).unwrap();
        let ch = check(&idx, &t.root);
        assert!(
            ch.modified.is_empty() && ch.touched_dirs.is_empty(),
            "{ch:?}"
        );
        fresh::apply(&idx, &t.root, &ch).unwrap();
    }
    assert_eq!(read_manifest(&t.dir).unwrap().deltas, 0);
    fs::write(&file, "fn behind() {}\n").unwrap();
    let ch = check(&Index::open(&t.dir).unwrap(), &t.root);
    assert_eq!(ch.modified.len(), 1, "{ch:?}");
}

/// A directory the build could not list is no change while it stays so, and
/// is listed by the first check after it becomes readable.
#[test]
fn an_unlisted_directory_is_listed_once_readable() {
    use std::os::unix::fs::PermissionsExt;
    let t = tree();
    let locked = t.root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("inner.txt"), "needle\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: this user reads a mode-000 directory (root)");
        return;
    }
    settle(&t.root);
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.unlisted_dirs(), [b"locked".to_vec()]);
    assert!(
        check(&idx, &t.root).is_empty(),
        "still unlistable: no change"
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let ch = check(&idx, &t.root);
    let added: Vec<&str> = ch.added.iter().map(|w| text(&w.rel)).collect();
    assert_eq!(added, ["locked/inner.txt"]);
    fresh::apply(&idx, &t.root, &ch).unwrap();
    assert!(Index::open(&t.dir).unwrap().unlisted_dirs().is_empty());
}
