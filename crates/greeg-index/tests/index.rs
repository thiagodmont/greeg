//! Build → open → delta → tombstone roundtrips, freshness under stat mode,
//! ignore-file edits, corrupt deltas and writer races, on a temp tree.

use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Mode};
use greeg_index::{Index, plan, read_manifest};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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
    bad[16 + 8..16 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&p, &bad).unwrap();
    assert!(
        Index::open(&t.dir).is_err(),
        "oversized files section must fail open"
    );

    // a section length that overflows into the tombstone bitmap
    let mut bad = good.clone();
    let gl = u32::from_le_bytes(good[16 + 12..16 + 16].try_into().unwrap());
    bad[16 + 12..16 + 16].copy_from_slice(&(gl + 8).to_le_bytes());
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
