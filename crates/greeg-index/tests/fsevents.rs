//! The FSEvents check against directory changes a stamp hides (macOS). A
//! separate test binary with one test: it sets the event read's cutoff
//! before any thread starts.
#![cfg(target_os = "macos")]

use greeg_index::Index;
use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Changes, Mode};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

struct Tmp {
    base: PathBuf,
    root: PathBuf,
    dir: PathBuf,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// A tree with `lib/thing.py`, in a directory this run creates itself.
fn tree() -> Tmp {
    let base = (0..)
        .map(|n| std::env::temp_dir().join(format!("greeg-fsevents-{}-{n}", std::process::id())))
        .find(|b| match fs::create_dir(b) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => panic!("create {}: {e}", b.display()),
        })
        .unwrap();
    let (root, dir) = (base.join("tree"), base.join("index"));
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(
        root.join("lib/thing.py"),
        "def thing_beta():\n    return 'a'\n",
    )
    .unwrap();
    // files enough that one change stays a delta
    for i in 0..40 {
        fs::write(root.join(format!("f{i:02}.txt")), "hay\n").unwrap();
    }
    Tmp { base, root, dir }
}

fn settle() {
    std::thread::sleep(Duration::from_millis(50));
}

fn build_index(t: &Tmp) {
    let opts = BuildOpts {
        reader_threads: 1,
        quiet: true,
        ..Default::default()
    };
    build(&t.root, &t.dir, &opts).unwrap();
}

/// The FSEvents check, or the stat pass when the event read misses even a
/// long cutoff on a loaded machine (said on stderr).
fn check(t: &Tmp, what: &str) -> Changes {
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::FsEvents, 2).unwrap();
    if ch.method != "fsevents" {
        eprintln!("{what}: the stat pass answered ({})", ch.method);
    }
    ch
}

fn rels(paths: impl Iterator<Item = Vec<u8>>) -> Vec<String> {
    let mut v: Vec<String> = paths
        .map(|p| String::from_utf8_lossy(&p).into_owned())
        .collect();
    v.sort();
    v
}

fn touch_as(reference: &Path, target: &Path) {
    let st = std::process::Command::new("touch")
        .arg("-r")
        .arg(reference)
        .arg(target)
        .status()
        .unwrap();
    assert!(st.success());
}

#[test]
fn fsevents_finds_what_a_directory_stamp_hides() {
    // SAFETY: the only test in this binary, and no thread has started yet.
    unsafe { std::env::set_var("GREEG_DEBUG_FSEVENTS_CUTOFF_MS", "15000") };

    // an entry added while the directory's mtime is put back (`rsync -a`)
    let t = tree();
    settle();
    build_index(&t);
    let lib = t.root.join("lib");
    let saved = t.base.join("saved");
    touch_as(&lib, &saved);
    fs::write(lib.join("new.py"), "x = 1\n").unwrap();
    touch_as(&saved, &lib);
    let ch = check(&t, "restored mtime");
    assert_eq!(rels(ch.added.iter().map(|w| w.rel.clone())), ["lib/new.py"]);

    // a directory unreadable at build, readable again
    let t = tree();
    let locked = t.root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("inner.txt"), "needle\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_err() {
        settle();
        build_index(&t);
        let ch = check(&t, "still unreadable");
        assert!(ch.is_empty(), "an unreadable directory is no change");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let ch = check(&t, "readable again");
        assert_eq!(
            rels(ch.added.iter().map(|w| w.rel.clone())),
            ["locked/inner.txt"]
        );
    } else {
        eprintln!("skipped the unreadable case: this user reads a mode-000 directory");
    }
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));

    // a directory listed too soon, named by an event though its stamp did not move
    let t = tree();
    settle();
    fs::File::open(lib_of(&t))
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    build_index(&t);
    fs::write(
        lib_of(&t).join("thing.py"),
        "def thing_beta():\n    return 'b'\n",
    )
    .unwrap();
    let ch = check(&t, "listed too soon");
    assert_eq!(rels(ch.touched_dirs.iter().map(|d| d.rel.clone())), ["lib"]);
}

fn lib_of(t: &Tmp) -> PathBuf {
    t.root.join("lib")
}
